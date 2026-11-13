//! A deliberately tiny YAML-frontmatter parser for skills and agents.
//!
//! Skills and agents are human-authored Markdown. Supporting a small, clear
//! subset avoids pulling a full YAML engine into the agent and keeps parsing
//! predictable.

use anyhow::{Context, Result};
use indexmap::IndexMap;

#[derive(Debug, Clone)]
pub struct MarkdownDoc {
    pub frontmatter: IndexMap<String, String>,
    pub body: String,
}

pub fn parse(text: &str) -> Result<MarkdownDoc> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    let Some(first) = lines.next() else {
        return Ok(MarkdownDoc {
            frontmatter: IndexMap::new(),
            body: String::new(),
        });
    };
    if first.trim() != "---" {
        return Ok(MarkdownDoc {
            frontmatter: IndexMap::new(),
            body: text.trim().to_string(),
        });
    }
    let mut frontmatter = IndexMap::new();
    let mut body_lines = Vec::new();
    let mut in_frontmatter = true;
    let mut saw_closer = false;
    for line in lines {
        if in_frontmatter {
            if line.trim() == "---" {
                in_frontmatter = false;
                saw_closer = true;
                continue;
            }
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line
                .split_once(':')
                .with_context(|| format!("frontmatter line is not `key: value`: {line}"))?;
            let key = key.trim().to_ascii_lowercase().replace('-', "_");
            if key.is_empty() {
                anyhow::bail!("empty frontmatter key");
            }
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(value);
            frontmatter.insert(key, value.to_string());
        } else {
            body_lines.push(line);
        }
    }
    if !saw_closer {
        anyhow::bail!("frontmatter is missing a closing `---`");
    }
    Ok(MarkdownDoc {
        frontmatter,
        body: body_lines.join("\n").trim().to_string(),
    })
}

#[must_use]
pub fn list_field(value: &str) -> Vec<String> {
    let value = value.trim();
    let value = value
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .unwrap_or(value);
    value
        .split(',')
        .map(|item| {
            item.trim()
                .trim_matches('"')
                .trim_matches('\'')
                .trim_start_matches('-')
                .trim()
                .to_string()
        })
        .filter(|item| !item.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let doc = parse("---\nname: test\ndescription: \"A test\"\n---\n# Heading\nBody").unwrap();
        assert_eq!(doc.frontmatter["name"], "test");
        assert_eq!(doc.frontmatter["description"], "A test");
        assert!(doc.body.contains("# Heading"));
    }

    #[test]
    fn plain_markdown_is_body() {
        let doc = parse("# Skill\nDo the thing").unwrap();
        assert!(doc.frontmatter.is_empty());
        assert!(doc.body.starts_with("# Skill"));
    }

    #[test]
    fn list_field_handles_inline_arrays() {
        assert_eq!(
            list_field("[Read, Grep, Bash(ls:*)]"),
            ["Read", "Grep", "Bash(ls:*)"]
        );
        assert_eq!(list_field("Read, Grep").len(), 2);
    }
}
