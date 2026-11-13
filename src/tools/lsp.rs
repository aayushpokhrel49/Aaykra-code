//! Language Server Protocol tools: diagnostics, definitions, references, hover.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::lsp::render_diagnostics;
use crate::tools::{Tool, ToolContext, definition, permission_request, truncate_tool_output};
use crate::types::{RiskLevel, ToolDefinition};

pub struct LspTool;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Diagnostics,
    Definition,
    References,
    Hover,
}

#[derive(Debug, Deserialize)]
struct LspInput {
    action: Action,
    path: String,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    column: Option<u64>,
    #[serde(default)]
    wait_ms: Option<u64>,
}

#[async_trait]
impl Tool for LspTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "Lsp",
            "Query a language server for real compiler diagnostics, go-to-definition, references, or hover/type information. Configure servers under [lsp.*] or rely on auto-detection (rust-analyzer, pyright/pylsp, typescript-language-server, gopls, clangd, bash-language-server). Lines and columns are 1-based.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["diagnostics", "definition", "references", "hover"]},
                    "path": {"type": "string", "description": "Source file path."},
                    "line": {"type": "integer", "minimum": 1, "description": "1-based line (definition/references/hover)."},
                    "column": {"type": "integer", "minimum": 1, "description": "1-based column (definition/references/hover)."},
                    "wait_ms": {"type": "integer", "minimum": 100, "maximum": 30000, "description": "How long to wait for diagnostics. Default 1500."}
                },
                "required": ["action", "path"],
                "additionalProperties": false
            }),
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        context.ensure_not_cancelled()?;
        let input: LspInput = serde_json::from_value(input).context("invalid Lsp input")?;
        let path = context.resolve_path(&input.path)?;
        if !path.is_file() {
            bail!("file does not exist: {}", path.display());
        }
        if !context.runtime.lsp.has_server_for(&path) {
            bail!(
                "no LSP server is available for .{} files; add one under [lsp.<name>] in config",
                path.extension().and_then(|ext| ext.to_str()).unwrap_or("?")
            );
        }
        let mut request = permission_request(
            "Lsp",
            context.display_path(&path),
            "query the language server",
            true,
            RiskLevel::Low,
            vec![path.clone()],
        );
        request.suggested_rule = "Lsp(*)".into();
        context.authorize(request).await?;

        let (line, column) = position(&input)?;
        let output = match input.action {
            Action::Diagnostics => {
                let wait = Duration::from_millis(input.wait_ms.unwrap_or(1_500).clamp(100, 30_000));
                let diagnostics = tokio::time::timeout(
                    wait + Duration::from_secs(10),
                    context.runtime.lsp.diagnostics(&path, wait),
                )
                .await
                .map_err(|_| anyhow::anyhow!("language server did not respond in time"))?
                .context("language server diagnostics failed")?;
                if diagnostics.is_empty() {
                    format!("{}: no diagnostics", context.display_path(&path))
                } else {
                    render_diagnostics(&path, &context.runtime.project_root, &diagnostics)
                }
            }
            Action::Definition => context
                .runtime
                .lsp
                .definition(&path, line, column)
                .await
                .context("go-to-definition failed")?,
            Action::References => context
                .runtime
                .lsp
                .references(&path, line, column)
                .await
                .context("find references failed")?,
            Action::Hover => context
                .runtime
                .lsp
                .hover(&path, line, column)
                .await
                .context("hover failed")?,
        };
        let output = strip_ansi(&output);
        let output = if output.trim().is_empty() {
            format!("{} returned no information", action_label(input.action))
        } else {
            output
        };
        Ok(truncate_tool_output(context, output))
    }
}

fn action_label(action: Action) -> &'static str {
    match action {
        Action::Diagnostics => "diagnostics",
        Action::Definition => "go-to-definition",
        Action::References => "references",
        Action::Hover => "hover",
    }
}

fn position(input: &LspInput) -> Result<(u32, u32)> {
    match input.action {
        Action::Diagnostics => Ok((0, 0)),
        _ => {
            let line = input.line.context("line is required for this action")?;
            let column = input.column.context("column is required for this action")?;
            let line = u32::try_from(line.max(1)).context("line is too large")?;
            let column = u32::try_from(column.max(1)).context("column is too large")?;
            Ok((line - 1, column - 1))
        }
    }
}

fn strip_ansi(input: &str) -> String {
    static ANSI: OnceLock<Regex> = OnceLock::new();
    let ansi = ANSI.get_or_init(|| Regex::new(r"\x1b\[[0-9;]*m").unwrap());
    ansi.replace_all(input, "").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_are_zero_based_for_lsp() {
        let input = LspInput {
            action: Action::Hover,
            path: "src/lib.rs".into(),
            line: Some(2),
            column: Some(3),
            wait_ms: None,
        };
        assert_eq!(position(&input).unwrap(), (1, 2));
    }

    #[test]
    fn diagnostic_actions_need_no_position() {
        let input = LspInput {
            action: Action::Diagnostics,
            path: "src/lib.rs".into(),
            line: None,
            column: None,
            wait_ms: None,
        };
        assert_eq!(position(&input).unwrap(), (0, 0));
    }
}
