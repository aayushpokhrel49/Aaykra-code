//! Shared wire, conversation, permission, and UI-event types.

use std::fmt;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single normalized conversation message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    #[serde(default)]
    pub content: Vec<ContentBlock>,
}

impl Message {
    #[must_use]
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    #[must_use]
    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    #[must_use]
    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::Assistant,
            content,
        }
    }

    #[must_use]
    pub fn tool_result(
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: tool_use_id.into(),
                content: content.into(),
                is_error,
            }],
        }
    }

    #[must_use]
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[must_use]
    pub fn tool_uses(&self) -> Vec<(&str, &str, &Value)> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, name, input } => {
                    Some((id.as_str(), name.as_str(), input))
                }
                _ => None,
            })
            .collect()
    }

    pub fn estimated_chars(&self) -> usize {
        self.content
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } | ContentBlock::Thinking { thinking: text, .. } => {
                    text.len()
                }
                ContentBlock::ToolUse { input, name, .. } => name.len() + input.to_string().len(),
                ContentBlock::ToolResult { content, .. } => content.len(),
                ContentBlock::Image { data, .. } => data.len() / 3,
            })
            .sum()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::System => f.write_str("system"),
            Self::User => f.write_str("user"),
            Self::Assistant => f.write_str("assistant"),
        }
    }
}

/// Provider-neutral content block. Images are base64 without the data-URL prefix.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    Image {
        media_type: String,
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

impl ContentBlock {
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub reasoning_tokens: u64,
}

impl Usage {
    #[must_use]
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    pub fn add(&mut self, other: Self) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
    }
}

impl fmt::Display for Usage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} in / {} out{}",
            compact_number(self.input_tokens),
            compact_number(self.output_tokens),
            if self.cache_read_input_tokens > 0 {
                format!(" / {} cached", compact_number(self.cache_read_input_tokens))
            } else {
                String::new()
            }
        )
    }
}

#[must_use]
pub fn compact_number(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[value(rename_all = "kebab-case")]
pub enum PermissionMode {
    /// Ask before every side effect. The safe default.
    Ask,
    /// Automatically accept file edits inside the project.
    AcceptEdits,
    /// Read-only exploration and planning.
    Plan,
    /// Automatically allow safe actions; ask for risky ones.
    Auto,
    /// Skip all permission checks. Dangerous.
    Bypass,
}

impl PermissionMode {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::AcceptEdits => "accept-edits",
            Self::Plan => "plan",
            Self::Auto => "auto",
            Self::Bypass => "bypass",
        }
    }

    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::Ask => "ask before side effects",
            Self::AcceptEdits => "auto-accept edits in project",
            Self::Plan => "read-only plan mode",
            Self::Auto => "safe actions auto, risky ask",
            Self::Bypass => "skip all checks (dangerous)",
        }
    }

    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Ask => Self::AcceptEdits,
            Self::AcceptEdits => Self::Plan,
            Self::Plan => Self::Auto,
            Self::Auto => Self::Bypass,
            Self::Bypass => Self::Ask,
        }
    }
}

impl Default for PermissionMode {
    fn default() -> Self {
        Self::Ask
    }
}

impl fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl std::str::FromStr for PermissionMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ask" | "manual" | "default" => Ok(Self::Ask),
            "accept-edits" | "accept_edits" | "accept" | "edits" => Ok(Self::AcceptEdits),
            "plan" | "read-only" | "readonly" => Ok(Self::Plan),
            "auto" => Ok(Self::Auto),
            "bypass" | "yolo" | "dangerous" | "skip" => Ok(Self::Bypass),
            other => Err(format!(
                "unknown permission mode `{other}` (ask, accept-edits, plan, auto, bypass)"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RiskLevel {
    Low,
    #[default]
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub id: String,
    pub tool: String,
    pub target: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    pub read_only: bool,
    pub risk: RiskLevel,
    pub suggested_rule: String,
    /// Absolute or project-relative paths affected by this action. Used for
    /// scope, sensitive-file, and symlink checks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub affected_paths: Vec<std::path::PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow,
    AllowForSession {
        rule: String,
    },
    Deny {
        #[serde(default)]
        reason: Option<String>,
    },
}

impl PermissionDecision {
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        !matches!(self, Self::Deny { .. })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserQuestion {
    pub question: String,
    #[serde(default)]
    pub header: String,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multiple: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UserAnswer {
    Selected { values: Vec<String> },
    Custom { text: String },
}

impl UserAnswer {
    #[must_use]
    pub fn as_text(&self) -> String {
        match self {
            Self::Selected { values } => values.join(", "),
            Self::Custom { text } => text.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl TodoStatus {
    #[must_use]
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Pending => "○",
            Self::InProgress => "◐",
            Self::Completed => "●",
            Self::Cancelled => "×",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    #[serde(default)]
    pub status: TodoStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct TurnUsage {
    pub usage: Usage,
    pub cost_usd: f64,
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    SessionStarted {
        id: String,
        title: String,
    },
    TurnStarted {
        turn: u64,
    },
    TextDelta(String),
    ThinkingDelta(String),
    ToolStarted {
        id: String,
        name: String,
        input: String,
    },
    ToolFinished {
        id: String,
        name: String,
        output: String,
        is_error: bool,
    },
    Notice(String),
    Warning(String),
    Error(String),
    TodoUpdated(Vec<TodoItem>),
    Usage {
        turn: Usage,
        session_usage: Usage,
        session_cost_usd: f64,
    },
    CompactionStarted,
    CompactionFinished {
        removed_messages: usize,
    },
    SubagentStarted {
        id: String,
        description: String,
    },
    SubagentFinished {
        id: String,
        success: bool,
        summary: String,
    },
    PermissionModeChanged(PermissionMode),
    ModelChanged {
        provider: String,
        model: String,
    },
    TurnFinished {
        turn: u64,
        stop_reason: String,
        usage: Usage,
        cost_usd: f64,
        session_cost_usd: f64,
    },
    RunFailed(String),
    QueuedUserMessage(String),
    /// Server-facing: a tool needs a permission decision. Clients answer with
    /// `POST /api/permissions/:id`.
    PermissionRequested {
        request: PermissionRequest,
    },
    PermissionResolved {
        id: String,
        allowed: bool,
    },
    /// Server-facing: the agent asks the user a question. Clients answer with
    /// `POST /api/questions/:id`.
    QuestionRequested {
        id: String,
        question: UserQuestion,
    },
    QuestionResolved {
        id: String,
        answer: Option<String>,
    },
}

/// Explicit serialization keeps event names stable and avoids serde's
/// internally-tagged-newtype limitation for string payloads.
impl Serialize for AgentEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde_json::json;
        let value = match self {
            Self::SessionStarted { id, title } => {
                json!({"type": "session_started", "id": id, "title": title})
            }
            Self::TurnStarted { turn } => json!({"type": "turn_started", "turn": turn}),
            Self::TextDelta(text) => json!({"type": "text_delta", "text": text}),
            Self::ThinkingDelta(text) => json!({"type": "thinking_delta", "text": text}),
            Self::ToolStarted { id, name, input } => {
                json!({"type": "tool_started", "id": id, "name": name, "input": input})
            }
            Self::ToolFinished {
                id,
                name,
                output,
                is_error,
            } => json!({
                "type": "tool_finished",
                "id": id,
                "name": name,
                "output": output,
                "is_error": is_error
            }),
            Self::Notice(text) => json!({"type": "notice", "text": text}),
            Self::Warning(text) => json!({"type": "warning", "text": text}),
            Self::Error(text) => json!({"type": "error", "text": text}),
            Self::TodoUpdated(todos) => json!({"type": "todo_updated", "todos": todos}),
            Self::Usage {
                turn,
                session_usage,
                session_cost_usd,
            } => json!({
                "type": "usage",
                "turn": turn,
                "session_usage": session_usage,
                "session_cost_usd": session_cost_usd
            }),
            Self::CompactionStarted => json!({"type": "compaction_started"}),
            Self::CompactionFinished { removed_messages } => {
                json!({"type": "compaction_finished", "removed_messages": removed_messages})
            }
            Self::SubagentStarted { id, description } => {
                json!({"type": "subagent_started", "id": id, "description": description})
            }
            Self::SubagentFinished {
                id,
                success,
                summary,
            } => json!({
                "type": "subagent_finished",
                "id": id,
                "success": success,
                "summary": summary
            }),
            Self::PermissionModeChanged(mode) => {
                json!({"type": "permission_mode_changed", "mode": mode})
            }
            Self::ModelChanged { provider, model } => {
                json!({"type": "model_changed", "provider": provider, "model": model})
            }
            Self::TurnFinished {
                turn,
                stop_reason,
                usage,
                cost_usd,
                session_cost_usd,
            } => json!({
                "type": "turn_finished",
                "turn": turn,
                "stop_reason": stop_reason,
                "usage": usage,
                "cost_usd": cost_usd,
                "session_cost_usd": session_cost_usd
            }),
            Self::RunFailed(error) => json!({"type": "run_failed", "error": error}),
            Self::QueuedUserMessage(text) => {
                json!({"type": "queued_user_message", "text": text})
            }
            Self::PermissionRequested { request } => {
                json!({"type": "permission_requested", "request": request})
            }
            Self::PermissionResolved { id, allowed } => {
                json!({"type": "permission_resolved", "id": id, "allowed": allowed})
            }
            Self::QuestionRequested { id, question } => {
                json!({"type": "question_requested", "id": id, "question": question})
            }
            Self::QuestionResolved { id, answer } => {
                json!({"type": "question_resolved", "id": id, "answer": answer})
            }
        };
        value.serialize(serializer)
    }
}

/// UI bridge used by the agent runtime. Implementations must be safe to call
/// from the agent task; TUI implementations marshal through channels.
#[async_trait::async_trait]
pub trait UiGateway: Send + Sync {
    async fn authorize(&self, request: PermissionRequest) -> PermissionDecision;
    async fn ask(&self, question: UserQuestion) -> Option<UserAnswer>;
    fn emit(&self, event: AgentEvent);
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[value(rename_all = "kebab-case")]
pub enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

impl Default for OutputFormat {
    fn default() -> Self {
        Self::Text
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopInfo {
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_accumulates() {
        let mut a = Usage {
            input_tokens: 10,
            output_tokens: 2,
            ..Usage::default()
        };
        a.add(Usage {
            input_tokens: 4,
            output_tokens: 8,
            cache_read_input_tokens: 3,
            ..Usage::default()
        });
        assert_eq!(a.total(), 24);
        assert_eq!(a.cache_read_input_tokens, 3);
    }

    #[test]
    fn permission_mode_cycle() {
        assert_eq!(PermissionMode::Ask.next(), PermissionMode::AcceptEdits);
        assert_eq!(PermissionMode::Bypass.next(), PermissionMode::Ask);
    }
}
