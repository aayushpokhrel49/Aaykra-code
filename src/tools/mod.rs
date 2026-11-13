//! Built-in tool interface and registry.

pub mod bash;
pub mod file;
pub mod git;
pub mod lsp;
pub mod meta;
pub mod search;
pub mod web;

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use crate::runtime::Runtime;
use crate::types::{PermissionRequest, RiskLevel, TodoItem, ToolDefinition};

use self::bash::BashTool;
use self::file::{DeleteTool, EditTool, MoveTool, MultiEditTool, ReadTool, WriteTool};
use self::git::GitTool;
use self::lsp::LspTool;
use self::meta::{
    AgentTool, AskUserQuestionTool, CheckpointTool, EnterPlanModeTool, EnterWorktreeTool,
    ExitPlanModeTool, ExitWorktreeTool, McpTool, MemoryTool, SkillTool, TodoWriteTool,
};
use self::search::{GlobTool, GrepTool, RepoMapTool};
use self::web::{WebFetchTool, WebSearchTool};

#[derive(Clone)]
pub struct ToolContext {
    pub runtime: Arc<Runtime>,
    pub agent_name: String,
    pub mcp_allowed: Option<Vec<String>>,
}

impl ToolContext {
    #[must_use]
    pub fn new(runtime: Arc<Runtime>, agent_name: impl Into<String>) -> Self {
        Self {
            runtime,
            agent_name: agent_name.into(),
            mcp_allowed: None,
        }
    }

    #[must_use]
    pub fn cwd(&self) -> PathBuf {
        self.runtime.cwd()
    }

    #[must_use]
    pub fn resolve_path(&self, raw: &str) -> Result<PathBuf> {
        let raw = raw.trim();
        if raw.is_empty() {
            bail!("path must not be empty");
        }
        let expanded = crate::paths::expand_tilde(raw);
        let joined = if expanded.is_absolute() {
            expanded
        } else {
            self.cwd().join(expanded)
        };
        Ok(normalize_path(&joined))
    }

    #[must_use]
    pub fn display_path(&self, path: &Path) -> String {
        let cwd = self.cwd();
        path.strip_prefix(&cwd)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string()
    }

    pub async fn authorize(&self, request: PermissionRequest) -> Result<()> {
        self.runtime.authorize(request).await
    }

    pub async fn checkpoint(&self, path: &Path) -> Result<()> {
        self.runtime.checkpoint(path).await
    }

    pub async fn record_change(&self, path: &Path) {
        self.runtime.record_change(&self.display_path(path)).await;
    }

    pub fn ensure_not_cancelled(&self) -> Result<()> {
        if self.runtime.cancel.is_cancelled() {
            bail!("cancelled by user");
        }
        Ok(())
    }

    #[must_use]
    pub fn max_output_chars(&self) -> usize {
        self.runtime.config.context.max_tool_output_chars
    }

    #[must_use]
    pub fn max_output_lines(&self) -> usize {
        self.runtime.config.context.max_tool_output_lines
    }

    pub async fn update_todos(&self, todos: Vec<TodoItem>) {
        {
            let mut session = self.runtime.session.lock().await;
            session.todos = todos.clone();
            let _ = self.runtime.store.save(&session);
        }
        self.runtime
            .emit(crate::types::AgentEvent::TodoUpdated(todos));
    }
}

#[must_use]
pub fn permission_request(
    tool: &str,
    target: impl Into<String>,
    reason: impl Into<String>,
    read_only: bool,
    risk: RiskLevel,
    affected_paths: Vec<PathBuf>,
) -> PermissionRequest {
    PermissionRequest {
        id: Uuid::new_v4().to_string(),
        tool: tool.to_string(),
        target: target.into(),
        reason: reason.into(),
        diff: None,
        read_only,
        risk,
        suggested_rule: String::new(),
        affected_paths,
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    fn read_only(&self) -> bool {
        false
    }
    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String>;
}

pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    #[must_use]
    pub fn standard() -> Self {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(ReadTool),
            Arc::new(WriteTool),
            Arc::new(EditTool),
            Arc::new(MultiEditTool),
            Arc::new(MoveTool),
            Arc::new(DeleteTool),
            Arc::new(GlobTool),
            Arc::new(GrepTool),
            Arc::new(RepoMapTool),
            Arc::new(BashTool),
            Arc::new(GitTool),
            Arc::new(WebSearchTool),
            Arc::new(WebFetchTool),
            Arc::new(LspTool),
            Arc::new(TodoWriteTool),
            Arc::new(AgentTool),
            Arc::new(SkillTool),
            Arc::new(AskUserQuestionTool),
            Arc::new(EnterPlanModeTool),
            Arc::new(ExitPlanModeTool),
            Arc::new(EnterWorktreeTool),
            Arc::new(ExitWorktreeTool),
            Arc::new(CheckpointTool),
            Arc::new(MemoryTool),
            Arc::new(McpTool),
        ];
        Self { tools }
    }

    #[must_use]
    pub fn filter(
        mut self,
        allowed: Option<&[String]>,
        denied: &[String],
        disabled: &[String],
    ) -> Self {
        self.tools.retain(|tool| {
            let name = tool.definition().name;
            let allowed_ok = allowed.is_none_or(|allowed| {
                allowed
                    .iter()
                    .any(|candidate| candidate.eq_ignore_ascii_case(&name))
            });
            let denied_ok = !denied
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(&name));
            let disabled_ok = !disabled
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(&name));
            allowed_ok && denied_ok && disabled_ok
        });
        self
    }

    #[must_use]
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|tool| tool.definition()).collect()
    }

    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.tools
            .iter()
            .map(|tool| tool.definition().name)
            .collect()
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .iter()
            .find(|tool| tool.definition().name.eq_ignore_ascii_case(name))
            .cloned()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// Lexically normalize without following symlinks. Authorization performs the
/// canonical/symlink check separately.
#[must_use]
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

pub fn truncate_tool_output(context: &ToolContext, content: String) -> String {
    let max_chars = context.max_output_chars();
    let max_lines = context.max_output_lines();
    let mut lines = 0;
    let mut bytes = 0;
    let mut end = content.len();
    for (index, ch) in content.char_indices() {
        if ch == '\n' {
            lines += 1;
            if lines >= max_lines {
                end = index;
                break;
            }
        }
        bytes += ch.len_utf8();
        if bytes >= max_chars {
            end = index;
            break;
        }
    }
    if end >= content.len() {
        return content;
    }
    let mut truncated = content[..end].to_string();
    truncated.push_str(&format!(
        "\n… output truncated at {} lines / {} characters",
        max_lines, max_chars
    ));
    truncated
}

pub fn parse_input<T: serde::de::DeserializeOwned>(input: Value) -> Result<T> {
    serde_json::from_value(input).context("invalid tool input")
}

pub fn definition(name: &str, description: impl Into<String>, schema: Value) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: description.into(),
        input_schema: schema,
    }
}

pub fn require_str(input: &Value, field: &str) -> Result<String> {
    input
        .get(field)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .with_context(|| format!("missing string field `{field}`"))
}

pub fn optional_str(input: &Value, field: &str) -> Option<String> {
    input
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

pub fn require_u64(input: &Value, field: &str) -> Result<u64> {
    input
        .get(field)
        .and_then(Value::as_u64)
        .with_context(|| format!("missing integer field `{field}`"))
}

pub fn optional_u64(input: &Value, field: &str) -> Option<u64> {
    input.get(field).and_then(Value::as_u64)
}

pub fn optional_bool(input: &Value, field: &str, default: bool) -> bool {
    input.get(field).and_then(Value::as_bool).unwrap_or(default)
}

/// True when the path is a regular file (following symlinks).
pub fn read_file_limited(path: &Path, max_bytes: usize) -> Result<Vec<u8>> {
    let metadata =
        std::fs::metadata(path).with_context(|| format!("could not read {}", path.display()))?;
    if !metadata.is_file() {
        bail!("not a regular file: {}", path.display());
    }
    if metadata.len() > max_bytes as u64 {
        bail!(
            "file is too large ({} bytes; limit {}); use Read with an offset or Grep",
            metadata.len(),
            max_bytes
        );
    }
    let bytes =
        std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    Ok(bytes)
}

#[must_use]
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8_000).any(|byte| *byte == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_parent_components() {
        assert_eq!(
            normalize_path(Path::new("/a/./b/../c")),
            PathBuf::from("/a/c")
        );
    }

    #[test]
    fn registry_has_core_tools() {
        let registry = ToolRegistry::standard();
        for name in ["Read", "Write", "Edit", "Bash", "Grep", "Agent", "Mcp"] {
            assert!(registry.get(name).is_some(), "missing {name}");
        }
    }
}
