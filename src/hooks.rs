//! Lifecycle hooks implemented as shell commands or in-process Rust callbacks.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::HookConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookKind {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PreCompact,
    PostCompact,
    Stop,
    SessionEnd,
    Notification,
}

impl HookKind {
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::UserPromptSubmit => "user_prompt_submit",
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::PreCompact => "pre_compact",
            Self::PostCompact => "post_compact",
            Self::Stop => "stop",
            Self::SessionEnd => "session_end",
            Self::Notification => "notification",
        }
    }

    #[must_use]
    pub fn display(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PreCompact => "PreCompact",
            Self::PostCompact => "PostCompact",
            Self::Stop => "Stop",
            Self::SessionEnd => "SessionEnd",
            Self::Notification => "Notification",
        }
    }

    fn normalized_key(key: &str) -> String {
        key.chars()
            .filter(char::is_ascii_alphanumeric)
            .flat_map(char::to_lowercase)
            .collect()
    }
}

#[derive(Debug, Clone, Default)]
pub struct HookContext {
    pub event: Option<HookKind>,
    pub session_id: String,
    pub cwd: String,
    pub provider: String,
    pub model: String,
    pub tool_name: Option<String>,
    pub tool_input: Option<Value>,
    pub tool_output: Option<String>,
    pub tool_error: bool,
    pub prompt: Option<String>,
    pub extra: IndexMap<String, Value>,
}

impl HookContext {
    #[must_use]
    pub fn new(session_id: impl Into<String>, cwd: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            cwd: cwd.into(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn payload(&self, kind: HookKind) -> Value {
        json!({
            "hook_event_name": kind.display(),
            "event": kind.key(),
            "session_id": self.session_id,
            "cwd": self.cwd,
            "provider": self.provider,
            "model": self.model,
            "tool_name": self.tool_name,
            "tool_input": self.tool_input,
            "tool_output": self.tool_output,
            "tool_error": self.tool_error,
            "prompt": self.prompt,
            "extra": self.extra,
        })
    }

    fn env(&self, kind: HookKind) -> HashMap<String, String> {
        let mut map = HashMap::new();
        map.insert("AAYKRA_HOOK_EVENT".into(), kind.key().into());
        map.insert("AAYKRA_SESSION_ID".into(), self.session_id.clone());
        map.insert("AAYKRA_CWD".into(), self.cwd.clone());
        map.insert("AAYKRA_PROVIDER".into(), self.provider.clone());
        map.insert("AAYKRA_MODEL".into(), self.model.clone());
        if let Some(tool) = &self.tool_name {
            map.insert("AAYKRA_TOOL_NAME".into(), tool.clone());
        }
        if let Some(input) = &self.tool_input {
            map.insert("AAYKRA_TOOL_INPUT".into(), input.to_string());
        }
        if let Some(output) = &self.tool_output {
            map.insert("AAYKRA_TOOL_OUTPUT".into(), output.clone());
        }
        if let Some(prompt) = &self.prompt {
            map.insert("AAYKRA_USER_PROMPT".into(), prompt.clone());
        }
        map
    }
}

#[derive(Debug, Clone, Default)]
pub struct HookOutcome {
    pub blocked: bool,
    pub reason: Option<String>,
    pub messages: Vec<String>,
}

impl HookOutcome {
    pub fn merge(&mut self, other: Self) {
        self.blocked |= other.blocked;
        if self.reason.is_none() {
            self.reason = other.reason;
        }
        self.messages.extend(other.messages);
    }
}

#[async_trait]
pub trait HookCallback: Send + Sync {
    fn event(&self) -> HookKind;
    async fn run(&self, kind: HookKind, context: &HookContext) -> HookOutcome;
}

pub struct HookRunner {
    config: IndexMap<String, Vec<HookConfig>>,
    callbacks: Arc<Vec<Arc<dyn HookCallback>>>,
}

impl HookRunner {
    #[must_use]
    pub fn new(config: IndexMap<String, Vec<HookConfig>>) -> Self {
        Self {
            config,
            callbacks: Arc::new(Vec::new()),
        }
    }

    #[must_use]
    pub fn with_callbacks(mut self, callbacks: Vec<Arc<dyn HookCallback>>) -> Self {
        self.callbacks = Arc::new(callbacks);
        self
    }

    #[must_use]
    pub fn has_hooks(&self, kind: HookKind) -> bool {
        self.matching_command_count(kind) > 0
            || self
                .callbacks
                .iter()
                .any(|callback| callback.event() == kind)
    }

    fn matching_command_count(&self, kind: HookKind) -> usize {
        let target = HookKind::normalized_key(kind.display());
        self.config
            .iter()
            .find(|(key, _)| HookKind::normalized_key(key) == target)
            .map_or(0, |(_, hooks)| hooks.len())
    }

    pub async fn run(&self, kind: HookKind, context: &HookContext) -> HookOutcome {
        let mut outcome = HookOutcome::default();
        for callback in self
            .callbacks
            .iter()
            .filter(|callback| callback.event() == kind)
        {
            let result =
                tokio::time::timeout(Duration::from_secs(30), callback.run(kind, context)).await;
            match result {
                Ok(result) => outcome.merge(result),
                Err(_) => {
                    outcome
                        .messages
                        .push(format!("{} callback timed out", kind.display()));
                    outcome.blocked = true;
                    outcome.reason = Some("hook callback timed out".into());
                }
            }
        }

        let target = HookKind::normalized_key(kind.display());
        let hooks: Vec<HookConfig> = self
            .config
            .iter()
            .find(|(key, _)| HookKind::normalized_key(key) == target)
            .map(|(_, hooks)| hooks.clone())
            .unwrap_or_default();

        for hook in hooks {
            if let Some(matcher) = &hook.matcher {
                let haystack = match kind {
                    HookKind::PreToolUse | HookKind::PostToolUse => {
                        context.tool_name.as_deref().unwrap_or("")
                    }
                    _ => context.prompt.as_deref().unwrap_or(""),
                };
                let Ok(regex) = regex::Regex::new(matcher) else {
                    outcome
                        .messages
                        .push(format!("hook matcher `{matcher}` is invalid; skipped"));
                    continue;
                };
                if !regex.is_match(haystack) {
                    continue;
                }
            }
            let result = run_command_hook(&hook, kind, context).await;
            match result {
                Ok((success, stdout, stderr)) => {
                    if !stdout.trim().is_empty() {
                        outcome.messages.push(stdout.trim().to_string());
                    }
                    if !success {
                        let message = if stderr.trim().is_empty() {
                            format!("{} hook exited unsuccessfully", kind.display())
                        } else {
                            stderr.trim().to_string()
                        };
                        if hook.blocking {
                            outcome.blocked = true;
                            outcome.reason = Some(message.clone());
                        }
                        outcome.messages.push(message);
                    } else if !stderr.trim().is_empty() {
                        outcome.messages.push(stderr.trim().to_string());
                    }
                }
                Err(error) => {
                    let message = format!("{} hook failed: {error:#}", kind.display());
                    if hook.blocking {
                        outcome.blocked = true;
                        outcome.reason = Some(message.clone());
                    }
                    outcome.messages.push(message);
                }
            }
        }
        outcome
    }
}

async fn run_command_hook(
    hook: &HookConfig,
    kind: HookKind,
    context: &HookContext,
) -> anyhow::Result<(bool, String, String)> {
    if hook.command.trim().is_empty() {
        anyhow::bail!("empty hook command");
    }
    let shell = if cfg!(windows) { "cmd" } else { "sh" };
    let args: &[&str] = if cfg!(windows) { &["/C"] } else { &["-lc"] };
    let mut command = Command::new(shell);
    command
        .args(args)
        .arg(&hook.command)
        .current_dir(&context.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in context.env(kind) {
        command.env(key, value);
    }
    let mut child = command
        .spawn()
        .map_err(|error| anyhow::anyhow!("could not spawn hook: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let payload = context.payload(kind).to_string();
        stdin.write_all(payload.as_bytes()).await.ok();
        drop(stdin);
    }
    let timeout = Duration::from_millis(hook.timeout_ms.clamp(1_000, 300_000));
    let output = tokio::time::timeout(timeout, child.wait_with_output()).await;
    match output {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            Ok((
                output.status.success(),
                truncate(&stdout, 8_000),
                truncate(&stderr, 8_000),
            ))
        }
        Ok(Err(error)) => Err(error.into()),
        Err(_) => {
            // `kill_on_drop(true)` terminates the child when the timed-out
            // wait future is dropped.
            anyhow::bail!("timed out after {}ms", timeout.as_millis())
        }
    }
}

fn truncate(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut boundary = max;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}…[truncated]", &value[..boundary])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(command: &str, blocking: bool) -> HookConfig {
        HookConfig {
            command: command.into(),
            matcher: None,
            timeout_ms: 5_000,
            blocking,
        }
    }

    #[tokio::test]
    async fn blocking_hook_blocks_tool() {
        let mut config = IndexMap::new();
        config.insert(
            "pre_tool_use".into(),
            vec![hook("echo denied >&2; exit 2", true)],
        );
        let runner = HookRunner::new(config);
        let context = HookContext::new("s", "/tmp");
        let outcome = runner.run(HookKind::PreToolUse, &context).await;
        assert!(outcome.blocked);
        assert!(outcome.reason.unwrap().contains("denied"));
    }

    #[tokio::test]
    async fn successful_hook_does_not_block() {
        let mut config = IndexMap::new();
        config.insert("session_start".into(), vec![hook("echo hello", true)]);
        let runner = HookRunner::new(config);
        let outcome = runner
            .run(HookKind::SessionStart, &HookContext::new("s", "/tmp"))
            .await;
        assert!(!outcome.blocked);
        assert_eq!(outcome.messages, vec!["hello"]);
    }
}
