//! Image loading for vision prompts: files, data URLs, and the clipboard.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use base64::Engine as _;

use crate::types::ImageAttachment;

const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// Load an image file, sniffing the media type from magic bytes first and the
/// extension second. Base64 is returned without a data-URL prefix.
pub fn load_file(path: &Path) -> Result<ImageAttachment> {
    let bytes =
        std::fs::read(path).with_context(|| format!("could not read image {}", path.display()))?;
    if bytes.len() > MAX_IMAGE_BYTES {
        bail!(
            "image {} is too large ({} bytes; max {MAX_IMAGE_BYTES})",
            path.display(),
            bytes.len()
        );
    }
    let media_type = sniff_media_type(&bytes)
        .or_else(|| media_type_from_extension(path))
        .with_context(|| {
            format!(
                "unsupported image format for {} (support: png, jpeg, gif, webp)",
                path.display()
            )
        })?;
    Ok(ImageAttachment {
        media_type: media_type.into(),
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

/// Accept a `data:image/...;base64,...` URL or raw base64 with an explicit type.
pub fn parse_input(input: &str, media_type: Option<&str>) -> Result<ImageAttachment> {
    if let Some(rest) = input.strip_prefix("data:") {
        let (header, data) = rest
            .split_once(',')
            .context("malformed data URL: missing comma")?;
        let header = header.strip_suffix(";base64").unwrap_or(header);
        if !header.starts_with("image/") {
            bail!("data URL is not an image: {header}");
        }
        return Ok(ImageAttachment {
            media_type: header.to_string(),
            data: data.to_string(),
        });
    }
    let path = Path::new(input);
    if path.is_file() {
        return load_file(path);
    }
    let media_type = media_type.context(
        "raw base64 images need --image-type (for example image/png); prefer passing a file path",
    )?;
    if !media_type.starts_with("image/") {
        bail!("--image-type must start with `image/`");
    }
    Ok(ImageAttachment {
        media_type: media_type.to_string(),
        data: input.to_string(),
    })
}

/// Try the platform clipboard for an image. Returns `Ok(None)` when no image is
/// available (an empty or text-only clipboard is not an error).
pub fn read_clipboard() -> Result<Option<ImageAttachment>> {
    let mut attempts: Vec<Vec<&str>> = Vec::new();
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        attempts.push(vec!["wl-paste", "--no-newline", "--type", "image/png"]);
    }
    attempts.push(vec![
        "xclip",
        "-selection",
        "clipboard",
        "-t",
        "image/png",
        "-o",
    ]);
    attempts.push(vec!["pngpaste", "-"]);

    let mut last_error: Option<anyhow::Error> = None;
    for args in attempts {
        let Some(program) = args.first().copied() else {
            continue;
        };
        match Command::new(program).args(&args[1..]).output() {
            Ok(output) if output.status.success() && !output.stdout.is_empty() => {
                let bytes = output.stdout;
                if bytes.len() > MAX_IMAGE_BYTES {
                    bail!("clipboard image is too large ({} bytes)", bytes.len());
                }
                let media_type = sniff_media_type(&bytes)
                    .or_else(|| {
                        if program == "pngpaste" {
                            Some("image/png")
                        } else {
                            None
                        }
                    })
                    .context("clipboard image has an unsupported format")?;
                return Ok(Some(ImageAttachment {
                    media_type: media_type.to_string(),
                    data: base64::engine::general_purpose::STANDARD.encode(bytes),
                }));
            }
            Ok(_) => {}
            Err(error) => last_error = Some(anyhow::anyhow!("{program}: {error}")),
        }
    }
    if last_error.is_some() {
        return Ok(None);
    }
    Ok(None)
}

#[must_use]
pub fn sniff_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn media_type_from_extension(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_common_formats() {
        assert_eq!(
            sniff_media_type(b"\x89PNG\r\n\x1a\nrest"),
            Some("image/png")
        );
        assert_eq!(
            sniff_media_type(&[0xFF, 0xD8, 0xFF, 0x00]),
            Some("image/jpeg")
        );
        assert_eq!(sniff_media_type(b"GIF89a"), Some("image/gif"));
        assert_eq!(sniff_media_type(b"RIFFxxxxWEBP"), Some("image/webp"));
        assert_eq!(sniff_media_type(b"plain"), None);
    }

    #[test]
    fn parses_data_urls() {
        let image = parse_input("data:image/png;base64,AAAA", None).unwrap();
        assert_eq!(image.media_type, "image/png");
        assert_eq!(image.data, "AAAA");
        assert!(parse_input("data:text/plain;base64,AAAA", None).is_err());
    }
}
