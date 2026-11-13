//! File read/write/edit and safe move/delete tools.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::diff::{diff_summary, unified_diff};
use crate::paths::atomic_write;
use crate::tools::{
    Tool, ToolContext, definition, is_binary, normalize_path, permission_request,
    read_file_limited, truncate_tool_output,
};
use crate::types::{RiskLevel, ToolDefinition};

const MAX_READ_BYTES: usize = 10 * 1024 * 1024;
const MAX_WRITE_BYTES: usize = 10 * 1024 * 1024;

pub struct ReadTool;
pub struct WriteTool;
pub struct EditTool;
pub struct MultiEditTool;
pub struct MoveTool;
pub struct DeleteTool;

impl ReadTool {
    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Absolute path or path relative to the working directory."},
                "offset": {"type": "integer", "minimum": 1, "description": "1-based first line to read. Default 1."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 10000, "description": "Maximum number of lines to return. Default 2000."}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "Read",
            "Read a UTF-8 text file with line numbers. Detects binary files, supports 1-based line offsets and pagination, and refuses files above 10 MiB. Always read a file before editing it.",
            Self::schema(),
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        context.ensure_not_cancelled()?;
        let path = context.resolve_path(
            input
                .get("path")
                .and_then(Value::as_str)
                .context("missing `path`")?,
        )?;
        let offset = input
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(2_000)
            .clamp(1, 10_000) as usize;
        let target = context.display_path(&path);
        let mut request = permission_request(
            "Read",
            target.clone(),
            "read a file",
            true,
            RiskLevel::Low,
            vec![path.clone()],
        );
        request.suggested_rule = format!("Read({target})");
        context.authorize(request).await?;

        let bytes = read_file_limited(&path, MAX_READ_BYTES)?;
        if is_binary(&bytes) {
            bail!(
                "{} appears to be a binary file ({} bytes); use an appropriate external tool",
                target,
                bytes.len()
            );
        }
        let text = std::str::from_utf8(&bytes)
            .with_context(|| format!("{} is not valid UTF-8", path.display()))?;
        if text.is_empty() {
            return Ok(format!("<{target}: empty file>"));
        }
        let lines: Vec<&str> = text.lines().collect();
        if offset > lines.len() {
            bail!(
                "offset {offset} is past end of file ({} lines)",
                lines.len()
            );
        }
        let end = (offset - 1 + limit).min(lines.len());
        let width = end.to_string().len();
        let mut out = format!("{target} (lines {}-{} of {})\n", offset, end, lines.len());
        for (index, line) in lines[offset - 1..end].iter().enumerate() {
            let line_number = offset + index;
            let line = if line.chars().count() > 4_000 {
                let truncated: String = line.chars().take(4_000).collect();
                format!("{truncated}…")
            } else {
                (*line).to_string()
            };
            out.push_str(&format!("{line_number:>width$}\t{line}\n"));
        }
        if end < lines.len() {
            out.push_str(&format!(
                "… {} more lines; call Read with offset={} to continue\n",
                lines.len() - end,
                end + 1
            ));
        }
        Ok(truncate_tool_output(context, out))
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "Write",
            "Create or overwrite a UTF-8 file. Parent directories are created. Every write is checkpointed before modification and shown as a diff for approval. Prefer Edit for existing files.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative or absolute."},
                    "content": {"type": "string", "description": "Complete new file content."}
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        context.ensure_not_cancelled()?;
        let path = context.resolve_path(
            input
                .get("path")
                .and_then(Value::as_str)
                .context("missing `path`")?,
        )?;
        let content = input
            .get("content")
            .and_then(Value::as_str)
            .context("missing `content`")?;
        if content.len() > MAX_WRITE_BYTES {
            bail!(
                "content exceeds {} MiB limit",
                MAX_WRITE_BYTES / 1024 / 1024
            );
        }
        if path.is_dir() {
            bail!("cannot write to a directory: {}", path.display());
        }
        let target = context.display_path(&path);
        let old = if path.exists() {
            let bytes = read_file_limited(&path, MAX_WRITE_BYTES)?;
            if is_binary(&bytes) {
                bail!("refusing to overwrite binary file {}", path.display());
            }
            String::from_utf8(bytes).context("existing file is not UTF-8")?
        } else {
            String::new()
        };
        if old == content {
            return Ok(format!("No change: {target} already has this content."));
        }
        let existed = path.exists();
        let diff = if existed {
            unified_diff(&target, &old, content, 3)
        } else {
            format!("--- /dev/null\n+++ b/{target}\n@@ new file @@\n")
        };
        let mut request = permission_request(
            "Write",
            target.clone(),
            if existed {
                "overwrite a file"
            } else {
                "create a file"
            },
            false,
            RiskLevel::Medium,
            vec![path.clone()],
        );
        request.diff = Some(diff.clone());
        request.suggested_rule = format!("Write({target})");
        context.authorize(request).await?;

        context.checkpoint(&path).await?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        let mode = std::fs::metadata(&path).ok().map(|m| m.permissions());
        atomic_write(&path, content.as_bytes())?;
        if let Some(mode) = mode {
            std::fs::set_permissions(&path, mode).ok();
        }
        context.record_change(&path).await;
        let diagnostics = maybe_diagnostics(context, &path).await;
        let mut out = format!(
            "{} {target}\n{}\n",
            if !existed { "Created" } else { "Wrote" },
            diff_summary(&diff)
        );
        if !diagnostics.is_empty() {
            out.push('\n');
            out.push_str(&diagnostics);
        }
        Ok(truncate_tool_output(context, out))
    }
}

#[async_trait]
impl Tool for EditTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "Edit",
            "Edit an existing UTF-8 file by replacing an exact string. The old string must occur exactly once unless replace_all is true. The file is checkpointed and the change shown as a diff before approval.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string", "description": "Exact text to replace, including indentation."},
                    "new_string": {"type": "string", "description": "Replacement text."},
                    "replace_all": {"type": "boolean", "default": false}
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        context.ensure_not_cancelled()?;
        let path = context.resolve_path(
            input
                .get("path")
                .and_then(Value::as_str)
                .context("missing `path`")?,
        )?;
        let old_string = input
            .get("old_string")
            .and_then(Value::as_str)
            .context("missing `old_string`")?;
        let new_string = input
            .get("new_string")
            .and_then(Value::as_str)
            .context("missing `new_string`")?;
        let replace_all = input
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if old_string.is_empty() {
            bail!("old_string must not be empty");
        }
        if old_string == new_string {
            bail!("old_string and new_string are identical");
        }
        let original = read_utf8(&path)?;
        let occurrences = original.matches(old_string).count();
        if occurrences == 0 {
            bail!(
                "old_string was not found in {} — read the file and copy the exact current text",
                context.display_path(&path)
            );
        }
        if occurrences > 1 && !replace_all {
            bail!(
                "old_string occurs {occurrences} times in {}; provide more surrounding context or set replace_all=true",
                context.display_path(&path)
            );
        }
        let updated = if replace_all {
            original.replace(old_string, new_string)
        } else {
            original.replacen(old_string, new_string, 1)
        };
        apply_file_update(
            context,
            &path,
            &original,
            &updated,
            "Edit",
            "replace text in a file",
        )
        .await
    }
}

#[async_trait]
impl Tool for MultiEditTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "MultiEdit",
            "Apply several ordered exact-string edits to one file atomically. All edits are validated before anything is written; the final result is approved once and checkpointed.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 100,
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": {"type": "string"},
                                "new_string": {"type": "string"},
                                "replace_all": {"type": "boolean", "default": false}
                            },
                            "required": ["old_string", "new_string"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["path", "edits"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        context.ensure_not_cancelled()?;
        let path = context.resolve_path(
            input
                .get("path")
                .and_then(Value::as_str)
                .context("missing `path`")?,
        )?;
        #[derive(Deserialize)]
        struct Edit {
            old_string: String,
            new_string: String,
            #[serde(default)]
            replace_all: bool,
        }
        let edits: Vec<Edit> =
            serde_json::from_value(input.get("edits").cloned().context("missing `edits`")?)
                .context("invalid edits array")?;
        if edits.is_empty() {
            bail!("edits must not be empty");
        }
        let original = read_utf8(&path)?;
        let mut updated = original.clone();
        for (index, edit) in edits.iter().enumerate() {
            if edit.old_string.is_empty() {
                bail!("edit {} has an empty old_string", index + 1);
            }
            let count = updated.matches(&edit.old_string).count();
            if count == 0 {
                bail!(
                    "edit {}: old_string not found after previous edits",
                    index + 1
                );
            }
            if count > 1 && !edit.replace_all {
                bail!(
                    "edit {}: old_string occurs {count} times; add context or replace_all=true",
                    index + 1
                );
            }
            updated = if edit.replace_all {
                updated.replace(&edit.old_string, &edit.new_string)
            } else {
                updated.replacen(&edit.old_string, &edit.new_string, 1)
            };
        }
        if updated == original {
            bail!("edits produced no change");
        }
        apply_file_update(
            context,
            &path,
            &original,
            &updated,
            "MultiEdit",
            "apply multiple edits to a file",
        )
        .await
    }
}

async fn apply_file_update(
    context: &ToolContext,
    path: &Path,
    original: &str,
    updated: &str,
    tool: &str,
    reason: &str,
) -> Result<String> {
    if updated.len() > MAX_WRITE_BYTES {
        bail!(
            "resulting file exceeds {} MiB",
            MAX_WRITE_BYTES / 1024 / 1024
        );
    }
    let target = context.display_path(path);
    let diff = unified_diff(&target, original, updated, 3);
    let mut request = permission_request(
        tool,
        target.clone(),
        reason,
        false,
        RiskLevel::Medium,
        vec![path.to_path_buf()],
    );
    request.diff = Some(diff.clone());
    request.suggested_rule = format!("{tool}({target})");
    context.authorize(request).await?;
    context.checkpoint(path).await?;
    let mode = std::fs::metadata(path).ok().map(|m| m.permissions());
    atomic_write(path, updated.as_bytes())?;
    if let Some(mode) = mode {
        std::fs::set_permissions(path, mode).ok();
    }
    context.record_change(path).await;
    let diagnostics = maybe_diagnostics(context, path).await;
    let mut out = format!("Edited {target}\n{}\n", diff_summary(&diff));
    if !diagnostics.is_empty() {
        out.push('\n');
        out.push_str(&diagnostics);
    }
    Ok(truncate_tool_output(context, out))
}

#[async_trait]
impl Tool for MoveTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "Move",
            "Rename or move a regular file. Existing destinations require overwrite=true and are checkpointed. Parent directories are created; refuses to overwrite unless explicitly allowed.",
            json!({
                "type": "object",
                "properties": {
                    "source": {"type": "string"},
                    "destination": {"type": "string"},
                    "overwrite": {"type": "boolean", "default": false}
                },
                "required": ["source", "destination"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        context.ensure_not_cancelled()?;
        let source = context.resolve_path(
            input
                .get("source")
                .and_then(Value::as_str)
                .context("missing `source`")?,
        )?;
        let destination = normalize_path(
            &context.resolve_path(
                input
                    .get("destination")
                    .and_then(Value::as_str)
                    .context("missing `destination`")?,
            )?,
        );
        let overwrite = input
            .get("overwrite")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if source == destination {
            bail!("source and destination are identical");
        }
        if !source.is_file() {
            bail!(
                "Move only handles regular files; {} is not a file",
                source.display()
            );
        }
        if destination.is_dir() {
            bail!("destination is a directory: {}", destination.display());
        }
        if destination.exists() && !overwrite {
            bail!("destination already exists; pass overwrite=true to replace it");
        }
        let source_display = context.display_path(&source);
        let dest_display = context.display_path(&destination);
        let mut request = permission_request(
            "Move",
            format!("{source_display} → {dest_display}"),
            "move or rename a file",
            false,
            RiskLevel::Medium,
            vec![source.clone(), destination.clone()],
        );
        request.suggested_rule = format!("Move({source_display})");
        context.authorize(request).await?;
        context.checkpoint(&source).await?;
        if destination.exists() {
            context.checkpoint(&destination).await?;
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Err(error) = std::fs::rename(&source, &destination) {
            if error.raw_os_error() == Some(18) {
                // EXDEV: copy then remove, preserving permissions.
                let mode = std::fs::metadata(&source).ok().map(|m| m.permissions());
                std::fs::copy(&source, &destination)?;
                if let Some(mode) = mode {
                    std::fs::set_permissions(&destination, mode).ok();
                }
                std::fs::remove_file(&source)?;
            } else {
                return Err(error).context("could not move file");
            }
        }
        context.record_change(&source).await;
        context.record_change(&destination).await;
        Ok(format!("Moved {source_display} → {dest_display}"))
    }
}

#[async_trait]
impl Tool for DeleteTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "Delete",
            "Delete a file or (with recursive=true) a directory tree. Every file is checkpointed first and the action always requires high-risk approval. Refuses .git internals.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "recursive": {"type": "boolean", "default": false}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        context.ensure_not_cancelled()?;
        let path = context.resolve_path(
            input
                .get("path")
                .and_then(Value::as_str)
                .context("missing `path`")?,
        )?;
        let recursive = input
            .get("recursive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("could not access {}", path.display()))?;
        let is_dir = metadata.is_dir();
        let is_symlink = metadata.file_type().is_symlink();
        if is_dir && !recursive {
            bail!("refusing to delete a directory without recursive=true");
        }
        if path == context.cwd() {
            bail!("refusing to delete the working directory");
        }
        let target = context.display_path(&path);
        let files = if is_dir && recursive {
            collect_files(&path, 5_000)?
        } else {
            vec![path.clone()]
        };
        let mut request = permission_request(
            "Delete",
            target.clone(),
            if is_dir {
                format!("delete a directory and {} files", files.len())
            } else {
                "delete a file".into()
            },
            false,
            RiskLevel::High,
            std::iter::once(path.clone())
                .chain(files.iter().cloned())
                .collect(),
        );
        request.suggested_rule = format!("Delete({target})");
        request.diff = Some(format!(
            "Deletion preview\n================\n{}\n",
            files
                .iter()
                .take(200)
                .map(|p| format!("delete {}", context.display_path(p)))
                .collect::<Vec<_>>()
                .join("\n")
        ));
        context.authorize(request).await?;
        for file in &files {
            if file.is_file() || file.is_symlink() {
                context.checkpoint(file).await?;
            }
        }
        if is_dir {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("could not remove {}", path.display()))?;
        } else {
            std::fs::remove_file(&path)
                .with_context(|| format!("could not remove {}", path.display()))?;
        }
        context.record_change(&path).await;
        let _ = is_symlink;
        Ok(format!("Deleted {target} ({} files)", files.len()))
    }
}

fn collect_files(root: &Path, limit: usize) -> Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        if result.len() >= limit {
            bail!("directory contains more than {limit} files; delete narrower paths");
        }
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            for entry in std::fs::read_dir(&path)? {
                stack.push(entry?.path());
            }
        } else {
            result.push(path);
        }
    }
    Ok(result)
}

fn read_utf8(path: &Path) -> Result<String> {
    let bytes = read_file_limited(path, MAX_READ_BYTES)?;
    if is_binary(&bytes) {
        bail!("refusing to edit binary file {}", path.display());
    }
    String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))
}

async fn maybe_diagnostics(context: &ToolContext, path: &Path) -> String {
    if !context.runtime.config.context.auto_diagnostics || !context.runtime.lsp.has_server_for(path)
    {
        return String::new();
    }
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        context
            .runtime
            .lsp
            .diagnostics(path, std::time::Duration::from_millis(500)),
    )
    .await;
    match result {
        Ok(Ok(diagnostics)) if !diagnostics.is_empty() => {
            crate::lsp::render_diagnostics(path, &context.runtime.project_root, &diagnostics)
        }
        _ => String::new(),
    }
}
