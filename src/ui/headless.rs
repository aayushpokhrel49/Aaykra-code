//! Non-interactive gateway used by `akc run` and CI integrations.
//!
//! Text mode writes assistant text to stdout and progress to stderr. JSON mode
//! emits one result object at the end. Stream-json mode emits newline-delimited
//! events as they happen and finishes with a `result` event.

use std::io::Write;
use std::sync::{Mutex, PoisonError};

use serde_json::{Value, json};

use crate::agent::RunOutcome;
use crate::session::Session;
use crate::types::{
    AgentEvent, OutputFormat, PermissionDecision, PermissionRequest, UiGateway, UserAnswer,
    UserQuestion, compact_number,
};

#[derive(Debug, Clone)]
pub struct HeadlessOptions {
    pub format: OutputFormat,
    /// Approve every permission prompt. Without this, requests that reach the
    /// gateway (Ask mode) are denied with an actionable message.
    pub auto_approve: bool,
    pub show_thinking: bool,
    pub show_tool_output: bool,
    pub quiet: bool,
}

impl Default for HeadlessOptions {
    fn default() -> Self {
        Self {
            format: OutputFormat::Text,
            auto_approve: false,
            show_thinking: false,
            show_tool_output: false,
            quiet: false,
        }
    }
}

pub struct HeadlessGateway {
    options: HeadlessOptions,
    events: Mutex<Vec<AgentEvent>>,
}

impl HeadlessGateway {
    #[must_use]
    pub fn new(options: HeadlessOptions) -> Self {
        Self {
            options,
            events: Mutex::new(Vec::new()),
        }
    }

    #[must_use]
    pub fn events(&self) -> Vec<AgentEvent> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn json_mode(&self) -> bool {
        self.options.format == OutputFormat::Json
    }

    fn stream_json(&self) -> bool {
        self.options.format == OutputFormat::StreamJson
    }

    fn progress(&self, message: impl std::fmt::Display) {
        if self.options.quiet {
            return;
        }
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "{message}");
        let _ = stderr.flush();
    }
}

#[async_trait::async_trait]
impl UiGateway for HeadlessGateway {
    async fn authorize(&self, request: PermissionRequest) -> PermissionDecision {
        if self.options.auto_approve {
            return PermissionDecision::Allow;
        }
        let reason = format!(
            "headless mode denied a permission request for {} ({}) on `{}`; \
             rerun with --yes, or use --permission-mode accept-edits/auto/bypass, \
             or add a rule to config",
            request.tool,
            request.risk_text(),
            request.target
        );
        self.progress(format!("permission denied: {reason}"));
        PermissionDecision::Deny {
            reason: Some(reason),
        }
    }

    async fn ask(&self, question: UserQuestion) -> Option<UserAnswer> {
        self.progress(format!(
            "skipping question in headless mode: {}",
            question.question
        ));
        None
    }

    fn emit(&self, event: AgentEvent) {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event.clone());

        if self.stream_json() {
            if let Ok(line) = serde_json::to_string(&event) {
                let mut stdout = std::io::stdout().lock();
                let _ = writeln!(stdout, "{line}");
                let _ = stdout.flush();
            }
            return;
        }
        if self.json_mode() {
            return;
        }

        match event {
            AgentEvent::TextDelta(text) => {
                let mut stdout = std::io::stdout().lock();
                let _ = write!(stdout, "{text}");
                let _ = stdout.flush();
            }
            AgentEvent::ThinkingDelta(text) if self.options.show_thinking => {
                let mut stderr = std::io::stderr().lock();
                let _ = write!(stderr, "{text}");
                let _ = stderr.flush();
            }
            AgentEvent::ToolStarted { name, input, .. } => {
                let preview = compact_json(&input, 160);
                self.progress(format!("→ {name} {preview}"));
            }
            AgentEvent::ToolFinished {
                name,
                output,
                is_error,
                ..
            } => {
                let marker = if is_error { "✗" } else { "✓" };
                if self.options.show_tool_output || is_error {
                    self.progress(format!("{marker} {name}: {}", excerpt(&output, 400)));
                } else {
                    self.progress(format!(
                        "{marker} {name} ({} lines)",
                        output.lines().count().max(1)
                    ));
                }
            }
            AgentEvent::Notice(message) | AgentEvent::Warning(message) => {
                self.progress(format!("• {message}"));
            }
            AgentEvent::Error(message) | AgentEvent::RunFailed(message) => {
                self.progress(format!("error: {message}"));
            }
            AgentEvent::CompactionStarted => self.progress("• compacting conversation…"),
            AgentEvent::CompactionFinished { removed_messages } => {
                self.progress(format!("• compacted {removed_messages} messages"));
            }
            AgentEvent::SubagentStarted { description, .. } => {
                self.progress(format!("• subagent started: {description}"));
            }
            AgentEvent::SubagentFinished {
                success, summary, ..
            } => {
                self.progress(format!(
                    "• subagent {}: {}",
                    if success { "finished" } else { "failed" },
                    excerpt(&summary, 200)
                ));
            }
            AgentEvent::ModelChanged { provider, model } => {
                self.progress(format!("• model: {provider}/{model}"));
            }
            AgentEvent::PermissionModeChanged(mode) => {
                self.progress(format!("• permission mode: {}", mode.label()));
            }
            AgentEvent::TodoUpdated(todos) => {
                let open = todos
                    .iter()
                    .filter(|todo| {
                        matches!(
                            todo.status,
                            crate::types::TodoStatus::Pending
                                | crate::types::TodoStatus::InProgress
                        )
                    })
                    .count();
                if open > 0 {
                    self.progress(format!("• {open} open todo(s)"));
                }
            }
            _ => {}
        }
    }
}

impl PermissionRequest {
    fn risk_text(&self) -> &'static str {
        match self.risk {
            crate::types::RiskLevel::Low => "low risk",
            crate::types::RiskLevel::Medium => "medium risk",
            crate::types::RiskLevel::High => "high risk",
            crate::types::RiskLevel::Critical => "critical risk",
        }
    }
}

/// Render the final result for JSON and stream-json modes. Text mode has
/// already streamed everything, so this is a no-op there.
pub fn print_result(format: OutputFormat, outcome: &RunOutcome, session: &Session) {
    let value = result_value(outcome, session);
    match format {
        OutputFormat::Text => {}
        OutputFormat::Json => {
            let mut stdout = std::io::stdout().lock();
            let _ = writeln!(
                stdout,
                "{}",
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into())
            );
            let _ = stdout.flush();
        }
        OutputFormat::StreamJson => {
            let mut stdout = std::io::stdout().lock();
            let _ = writeln!(
                stdout,
                "{}",
                serde_json::to_string(&value).unwrap_or_else(|_| "{}".into())
            );
            let _ = stdout.flush();
        }
    }
}

/// A structured error result so CI scripts can parse failures in JSON modes.
pub fn print_error(format: OutputFormat, message: &str, session: Option<&Session>) {
    match format {
        OutputFormat::Text => {
            let mut stderr = std::io::stderr().lock();
            let _ = writeln!(stderr, "error: {message}");
        }
        OutputFormat::Json | OutputFormat::StreamJson => {
            let mut value = json!({
                "type": "result",
                "subtype": "error",
                "is_error": true,
                "error": message,
            });
            if let Some(session) = session {
                value["session_id"] = json!(session.id);
                value["model"] = json!(format!("{}/{}", session.provider, session.model));
            }
            let mut stdout = std::io::stdout().lock();
            let _ = writeln!(
                stdout,
                "{}",
                serde_json::to_string(&value).unwrap_or_default()
            );
            let _ = stdout.flush();
        }
    }
}

#[must_use]
pub fn result_value(outcome: &RunOutcome, session: &Session) -> Value {
    json!({
        "type": "result",
        "subtype": if outcome.stop_reason == "error" { "error" } else { "success" },
        "is_error": outcome.stop_reason == "error",
        "session_id": session.id,
        "session_name": session.name,
        "title": session.display_title(),
        "provider": session.provider,
        "model": session.model,
        "agent": session.agent,
        "text": outcome.text,
        "stop_reason": outcome.stop_reason,
        "usage": {
            "input_tokens": outcome.usage.input_tokens,
            "output_tokens": outcome.usage.output_tokens,
            "cache_read_input_tokens": outcome.usage.cache_read_input_tokens,
            "cache_creation_input_tokens": outcome.usage.cache_creation_input_tokens,
            "reasoning_tokens": outcome.usage.reasoning_tokens,
            "total_tokens": outcome.usage.total(),
            "total_tokens_display": compact_number(outcome.usage.total()),
        },
        "cost_usd": outcome.cost_usd,
        "session_usage": {
            "input_tokens": session.usage.input_tokens,
            "output_tokens": session.usage.output_tokens,
            "total_tokens": session.usage.total(),
        },
        "session_cost_usd": session.cost_usd,
        "turns": session.turns.len(),
        "messages": session.messages.len(),
        "files_changed": session.files_changed,
        "duration_ms": session.turns.last().and_then(|turn| {
            turn.finished_at.map(|finished| (finished - turn.started_at).num_milliseconds())
        }),
    })
}

fn compact_json(input: &str, max: usize) -> String {
    if input.trim().is_empty() {
        return "{}".into();
    }
    if let Ok(value) = serde_json::from_str::<Value>(input) {
        let compact = serde_json::to_string(&value).unwrap_or_default();
        return excerpt(&compact, max);
    }
    excerpt(input, max)
}

fn excerpt(text: &str, max: usize) -> String {
    let single_line = text.replace('\n', " ⏎ ");
    if single_line.chars().count() <= max {
        single_line
    } else {
        format!(
            "{}…",
            single_line
                .chars()
                .take(max.saturating_sub(1))
                .collect::<String>()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Usage;
    use std::path::PathBuf;

    #[test]
    fn result_has_stable_fields() {
        let mut session = Session::new("anthropic", "claude-sonnet-4-5", PathBuf::from("/tmp"));
        session.usage = Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Usage::default()
        };
        let outcome = RunOutcome {
            text: "hello".into(),
            stop_reason: "end_turn".into(),
            usage: session.usage,
            cost_usd: 0.01,
        };
        let value = result_value(&outcome, &session);
        assert_eq!(value["type"], "result");
        assert_eq!(value["text"], "hello");
        assert_eq!(value["usage"]["total_tokens"], 15);
    }

    #[test]
    fn excerpt_is_single_line() {
        assert_eq!(excerpt("a\nb", 10), "a ⏎ b");
    }
}
