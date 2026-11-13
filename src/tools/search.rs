//! Fast file finding, content search, and tree-sitter repo maps.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use globset::{GlobBuilder, GlobMatcher};
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde_json::{Value, json};

use crate::repo_map::{RepoMap, RepoMapOptions};
use crate::tools::{
    Tool, ToolContext, definition, is_binary, permission_request, truncate_tool_output,
};
use crate::types::{RiskLevel, ToolDefinition};

pub struct GlobTool;
pub struct GrepTool;
pub struct RepoMapTool;

#[async_trait]
impl Tool for GlobTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "Glob",
            "Find files by glob pattern (for example `**/*.rs` or `src/**/mod.rs`). Respects .gitignore by default, supports hidden files, and returns newest files first. Use this instead of `find`.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Glob pattern matched against project-relative paths and file names."},
                    "path": {"type": "string", "description": "Directory to search. Defaults to the working directory."},
                    "include_hidden": {"type": "boolean", "default": false},
                    "no_ignore": {"type": "boolean", "default": false, "description": "Do not respect .gitignore files."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 5000, "default": 500}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        context.ensure_not_cancelled()?;
        let pattern = input
            .get("pattern")
            .and_then(Value::as_str)
            .context("missing `pattern`")?;
        let root = input
            .get("path")
            .and_then(Value::as_str)
            .map_or_else(|| Ok(context.cwd()), |path| context.resolve_path(path))?;
        if !root.is_dir() {
            bail!("search path is not a directory: {}", root.display());
        }
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(500)
            .clamp(1, 5_000) as usize;
        let include_hidden = input
            .get("include_hidden")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let no_ignore = input
            .get("no_ignore")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let matcher = compile_glob(pattern)?;
        let target = context.display_path(&root);
        let mut request = permission_request(
            "Glob",
            format!("{pattern} in {target}"),
            "find files",
            true,
            RiskLevel::Low,
            vec![root.clone()],
        );
        request.suggested_rule = format!("Glob({pattern})");
        context.authorize(request).await?;
        context.ensure_not_cancelled()?;

        let walker = WalkBuilder::new(&root)
            .hidden(!include_hidden)
            .git_ignore(!no_ignore)
            .git_global(!no_ignore)
            .git_exclude(!no_ignore)
            .filter_entry(|entry| {
                !entry.file_type().is_some_and(|kind| kind.is_dir())
                    || !common_ignored_dir(&entry.file_name().to_string_lossy())
            })
            .build();
        let mut matches: Vec<(PathBuf, Option<SystemTime>)> = Vec::new();
        for entry in walker.flatten() {
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let path = entry.path();
            let relative = path.strip_prefix(&root).unwrap_or(path);
            if glob_matches(&matcher, relative, path.file_name()) {
                let modified = entry.metadata().ok().and_then(|m| m.modified().ok());
                matches.push((relative.to_path_buf(), modified));
            }
        }
        matches.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        if matches.is_empty() {
            return Ok(format!("No files matched `{pattern}` under {target}."));
        }
        let total = matches.len();
        let mut out = String::new();
        for (path, _) in matches.iter().take(limit) {
            out.push_str(&path.to_string_lossy());
            out.push('\n');
        }
        if total > limit {
            out.push_str(&format!("… {} more files (limit {limit})\n", total - limit));
        }
        Ok(truncate_tool_output(context, out))
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "Grep",
            "Search file contents with Rust regex. Respects .gitignore, skips binaries and huge files, supports a file glob and context lines. Output is `path:line:text`; use this instead of shell grep.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Rust regular expression."},
                    "path": {"type": "string", "description": "File or directory to search. Defaults to cwd."},
                    "glob": {"type": "string", "description": "Optional file glob such as `*.rs` or `src/**`."},
                    "case_sensitive": {"type": "boolean", "default": true},
                    "context_lines": {"type": "integer", "minimum": 0, "maximum": 10, "default": 0},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 2000, "default": 200},
                    "include_hidden": {"type": "boolean", "default": false},
                    "no_ignore": {"type": "boolean", "default": false}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        context.ensure_not_cancelled()?;
        let pattern = input
            .get("pattern")
            .and_then(Value::as_str)
            .context("missing `pattern`")?;
        let regex = RegexBuilder::new(pattern)
            .case_insensitive(
                !input
                    .get("case_sensitive")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            )
            .build()
            .with_context(|| format!("invalid regex `{pattern}`"))?;
        let root = input
            .get("path")
            .and_then(Value::as_str)
            .map_or_else(|| Ok(context.cwd()), |path| context.resolve_path(path))?;
        let glob = input
            .get("glob")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(compile_glob)
            .transpose()?;
        let context_lines = input
            .get("context_lines")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .clamp(0, 10) as usize;
        let max_results = input
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(200)
            .clamp(1, 2_000) as usize;
        let include_hidden = input
            .get("include_hidden")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let no_ignore = input
            .get("no_ignore")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let target = context.display_path(&root);
        let mut request = permission_request(
            "Grep",
            format!("{pattern} in {target}"),
            "search file contents",
            true,
            RiskLevel::Low,
            vec![root.clone()],
        );
        request.suggested_rule = format!("Grep({pattern})");
        context.authorize(request).await?;

        let files = if root.is_file() {
            vec![root.clone()]
        } else {
            let walker = WalkBuilder::new(&root)
                .hidden(!include_hidden)
                .git_ignore(!no_ignore)
                .git_global(!no_ignore)
                .git_exclude(!no_ignore)
                .filter_entry(|entry| {
                    !entry.file_type().is_some_and(|kind| kind.is_dir())
                        || !common_ignored_dir(&entry.file_name().to_string_lossy())
                })
                .build();
            let mut files = Vec::new();
            for entry in walker.flatten() {
                if entry.file_type().is_some_and(|kind| kind.is_file()) {
                    let path = entry.path();
                    if let Some(glob) = &glob {
                        let relative = path.strip_prefix(&root).unwrap_or(path);
                        if !glob_matches(glob, relative, path.file_name()) {
                            continue;
                        }
                    }
                    files.push(path.to_path_buf());
                }
            }
            files.sort();
            files
        };

        let base = if root.is_dir() {
            root.as_path()
        } else {
            root.parent().unwrap_or(Path::new("."))
        };
        let mut output = String::new();
        let mut shown = 0usize;
        let mut files_searched = 0usize;
        for file in files {
            if context.runtime.cancel.is_cancelled() {
                bail!("cancelled by user");
            }
            let Ok(metadata) = std::fs::metadata(&file) else {
                continue;
            };
            if metadata.len() > 5 * 1024 * 1024 {
                continue;
            }
            let Ok(bytes) = std::fs::read(&file) else {
                continue;
            };
            if is_binary(&bytes) || std::str::from_utf8(&bytes).is_err() {
                continue;
            }
            files_searched += 1;
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = text.lines().collect();
            let matched_indices: Vec<usize> = lines
                .iter()
                .enumerate()
                .filter(|(_, line)| regex.is_match(line))
                .map(|(index, _)| index)
                .collect();
            if matched_indices.is_empty() {
                continue;
            }
            let relative = file.strip_prefix(base).unwrap_or(&file);
            let relative = relative.to_string_lossy();
            let mut ranges: Vec<(usize, usize)> = Vec::new();
            for index in &matched_indices {
                let start = index.saturating_sub(context_lines);
                let end = (*index + context_lines).min(lines.len().saturating_sub(1));
                if let Some(last) = ranges.last_mut()
                    && start <= last.1 + 1
                {
                    last.1 = last.1.max(end);
                } else {
                    ranges.push((start, end));
                }
            }
            for (range_index, (start, end)) in ranges.iter().enumerate() {
                let file_matches = matched_indices
                    .iter()
                    .filter(|index| **index >= *start && **index <= *end)
                    .count();
                if shown >= max_results {
                    break;
                }
                if range_index > 0 {
                    output.push_str("--\n");
                }
                for index in *start..=*end {
                    let is_match = matched_indices.binary_search(&index).is_ok();
                    let separator = if is_match { ':' } else { '-' };
                    let line = truncate_line(lines[index], 600);
                    output.push_str(&format!(
                        "{relative}{separator}{}{separator}{line}\n",
                        index + 1
                    ));
                }
                shown += file_matches;
            }
            if shown >= max_results {
                output.push_str(&format!("… result limit ({max_results}) reached\n"));
                break;
            }
        }
        if output.is_empty() {
            return Ok(format!(
                "No matches for `{pattern}` in {target} ({files_searched} files searched)."
            ));
        }
        output = format!(
            "Found {shown} matching line{} in {target}.\n{output}",
            if shown == 1 { "" } else { "s" }
        );
        Ok(truncate_tool_output(context, output))
    }
}

#[async_trait]
impl Tool for RepoMapTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "RepoMap",
            "Build a compact structural map of the repository using tree-sitter (Rust, Python, JS/TS, Go, shell). Use `search` to locate symbols by name, or `map` to understand architecture. Far cheaper than reading every file.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["map", "search"], "default": "map"},
                    "query": {"type": "string", "description": "Symbol or path fragment for action=search."},
                    "path": {"type": "string", "description": "Repository root. Defaults to cwd."},
                    "max_chars": {"type": "integer", "minimum": 1000, "maximum": 200000, "default": 40000}
                },
                "additionalProperties": false
            }),
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        context.ensure_not_cancelled()?;
        let root = input
            .get("path")
            .and_then(Value::as_str)
            .map_or_else(|| Ok(context.cwd()), |path| context.resolve_path(path))?;
        if !root.is_dir() {
            bail!("repo map path is not a directory: {}", root.display());
        }
        let action = input.get("action").and_then(Value::as_str).unwrap_or("map");
        let query = input
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if action == "search" && query.trim().is_empty() {
            bail!("action=search requires `query`");
        }
        let max_chars = input
            .get("max_chars")
            .and_then(Value::as_u64)
            .unwrap_or(40_000)
            .clamp(1_000, 200_000) as usize;
        let target = context.display_path(&root);
        let mut request = permission_request(
            "RepoMap",
            target.clone(),
            "build a structural code map",
            true,
            RiskLevel::Low,
            vec![root.clone()],
        );
        request.suggested_rule = format!("RepoMap({target})");
        context.authorize(request).await?;
        let options = RepoMapOptions {
            max_files: context.runtime.config.context.repo_map_files,
            ..RepoMapOptions::default()
        };
        let root_clone = root.clone();
        let map = tokio::task::spawn_blocking(move || RepoMap::build(&root_clone, &options))
            .await
            .context("repo map task failed")??;
        let output = if action == "search" {
            map.search(&query, max_chars)
        } else {
            map.render(max_chars)
        };
        Ok(truncate_tool_output(context, output))
    }
}

fn compile_glob(pattern: &str) -> Result<GlobMatcher> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(true)
        .build()
        .with_context(|| format!("invalid glob `{pattern}`"))
        .map(|glob| glob.compile_matcher())
}

fn glob_matches(
    matcher: &GlobMatcher,
    relative: &Path,
    file_name: Option<&std::ffi::OsStr>,
) -> bool {
    if matcher.is_match(relative) || matcher.is_match(relative.to_string_lossy().as_ref()) {
        return true;
    }
    let pattern_has_slash = matcher.glob().glob().contains('/');
    if !pattern_has_slash && let Some(name) = file_name {
        return matcher.is_match(Path::new(name));
    }
    false
}

fn common_ignored_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "target"
            | "node_modules"
            | ".venv"
            | "venv"
            | "__pycache__"
            | "dist"
            | "build"
            | ".next"
            | ".turbo"
            | ".aaykra"
    )
}

fn truncate_line(line: &str, max: usize) -> String {
    if line.chars().count() <= max {
        line.to_string()
    } else {
        format!("{}…", line.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_glob_matches_nested_file() {
        let matcher = compile_glob("*.rs").unwrap();
        assert!(glob_matches(
            &matcher,
            Path::new("src/lib.rs"),
            Some(std::ffi::OsStr::new("lib.rs"))
        ));
    }

    #[test]
    fn path_glob_requires_relative_match() {
        let matcher = compile_glob("src/**/*.rs").unwrap();
        assert!(glob_matches(
            &matcher,
            Path::new("src/tools/mod.rs"),
            Some(std::ffi::OsStr::new("mod.rs"))
        ));
    }

    #[test]
    fn common_dirs_are_ignored() {
        assert!(common_ignored_dir("node_modules"));
        assert!(!common_ignored_dir("src"));
    }
}
