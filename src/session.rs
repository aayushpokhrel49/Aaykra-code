//! Persistent, human-readable sessions.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::models::format_cost;
use crate::paths::{AaykraPaths, atomic_write};
use crate::types::{ContentBlock, ImageAttachment, Message, PermissionMode, TodoItem, Usage};

pub const SESSION_VERSION: u32 = 1;

/// Session metadata key set whenever the active model changes or a session is
/// resumed. The next turn uses it to refresh codebase context and condense old
/// history into a portable handoff brief.
pub const CONTEXT_HANDOFF_KEY: &str = "context_handoff_pending";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnRecord {
    pub number: u64,
    pub prompt_index: usize,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub files_changed: Vec<String>,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub cwd: PathBuf,
    pub provider: String,
    pub model: String,
    #[serde(default = "default_agent")]
    pub agent: String,
    pub permission_mode: PermissionMode,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub todos: Vec<TodoItem>,
    #[serde(default)]
    pub turns: Vec<TurnRecord>,
    #[serde(default)]
    pub current_turn: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<PathBuf>,
    #[serde(default)]
    pub files_changed: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_stop_reason: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

fn default_agent() -> String {
    "general".into()
}

impl Session {
    #[must_use]
    pub fn new(provider: impl Into<String>, model: impl Into<String>, cwd: PathBuf) -> Self {
        let now = Utc::now();
        Self {
            version: SESSION_VERSION,
            id: Uuid::new_v4().to_string(),
            name: None,
            title: "New session".into(),
            created_at: now,
            updated_at: now,
            cwd,
            provider: provider.into(),
            model: model.into(),
            agent: default_agent(),
            permission_mode: PermissionMode::Ask,
            messages: Vec::new(),
            usage: Usage::default(),
            cost_usd: 0.0,
            todos: Vec::new(),
            turns: Vec::new(),
            current_turn: 0,
            parent_id: None,
            worktree: None,
            files_changed: Vec::new(),
            last_stop_reason: None,
            metadata: BTreeMap::new(),
        }
    }

    pub fn begin_turn(&mut self, message: Message) -> u64 {
        self.current_turn += 1;
        let number = self.current_turn;
        let prompt_index = self.messages.len();
        if self.title == "New session" || self.title.trim().is_empty() {
            self.title = title_from_prompt(&message.text());
        }
        self.messages.push(message);
        self.updated_at = Utc::now();
        self.turns.push(TurnRecord {
            number,
            prompt_index,
            started_at: Utc::now(),
            finished_at: None,
            files_changed: Vec::new(),
            usage: Usage::default(),
            cost_usd: 0.0,
            stop_reason: None,
        });
        number
    }

    pub fn finish_turn(&mut self, usage: Usage, cost_usd: f64, stop_reason: impl Into<String>) {
        self.updated_at = Utc::now();
        self.last_stop_reason = Some(stop_reason.into());
        if let Some(turn) = self.turns.last_mut() {
            turn.finished_at = Some(Utc::now());
            turn.usage = usage;
            turn.cost_usd = cost_usd;
            turn.stop_reason = self.last_stop_reason.clone();
        }
        self.usage.add(usage);
        self.cost_usd = ((self.cost_usd + cost_usd) * 1_000_000.0).round() / 1_000_000.0;
    }

    pub fn push_message(&mut self, message: Message) {
        self.messages.push(message);
        self.updated_at = Utc::now();
    }

    pub fn record_file_change(&mut self, path: impl Into<String>) {
        let path = path.into();
        if !self.files_changed.contains(&path) {
            self.files_changed.push(path.clone());
        }
        if let Some(turn) = self.turns.last_mut()
            && !turn.files_changed.contains(&path)
        {
            turn.files_changed.push(path);
        }
    }

    /// Remove the last turn's messages, usage, and metadata. File restoration is
    /// handled by the checkpoint store. Returns the removed turn.
    pub fn undo_last_turn(&mut self) -> Option<TurnRecord> {
        let turn = self.turns.pop()?;
        self.messages.truncate(turn.prompt_index);
        self.current_turn = self.turns.last().map_or(0, |t| t.number);
        self.usage.input_tokens = self
            .usage
            .input_tokens
            .saturating_sub(turn.usage.input_tokens);
        self.usage.output_tokens = self
            .usage
            .output_tokens
            .saturating_sub(turn.usage.output_tokens);
        self.usage.cache_creation_input_tokens = self
            .usage
            .cache_creation_input_tokens
            .saturating_sub(turn.usage.cache_creation_input_tokens);
        self.usage.cache_read_input_tokens = self
            .usage
            .cache_read_input_tokens
            .saturating_sub(turn.usage.cache_read_input_tokens);
        self.usage.reasoning_tokens = self
            .usage
            .reasoning_tokens
            .saturating_sub(turn.usage.reasoning_tokens);
        self.cost_usd = (self.cost_usd - turn.cost_usd).max(0.0);
        for path in &turn.files_changed {
            self.files_changed.retain(|p| p != path);
        }
        self.updated_at = Utc::now();
        Some(turn)
    }

    /// Change the active provider/model and remember that the next turn needs a
    /// context handoff. Returns true when the model actually changed.
    pub fn set_model(&mut self, provider: impl Into<String>, model: impl Into<String>) -> bool {
        let provider = provider.into();
        let model = model.into();
        let changed = self.provider != provider || self.model != model;
        if changed {
            self.mark_context_handoff();
            self.provider = provider;
            self.model = model;
        }
        self.updated_at = Utc::now();
        changed
    }

    /// Mark this session for a portable context handoff on the next turn.
    pub fn mark_context_handoff(&mut self) {
        self.metadata.insert(CONTEXT_HANDOFF_KEY.into(), "1".into());
    }

    /// Consume a pending handoff marker.
    pub fn take_context_handoff(&mut self) -> bool {
        self.metadata.remove(CONTEXT_HANDOFF_KEY).is_some()
    }

    #[must_use]
    pub fn display_title(&self) -> String {
        if let Some(name) = &self.name {
            return name.clone();
        }
        if self.title.trim().is_empty() {
            "New session".into()
        } else {
            self.title.clone()
        }
    }

    #[must_use]
    pub fn latest_assistant_text(&self) -> Option<String> {
        self.messages.iter().rev().find_map(|message| {
            if message.role == crate::types::Role::Assistant {
                let text = message.text();
                if text.is_empty() { None } else { Some(text) }
            } else {
                None
            }
        })
    }

    #[must_use]
    pub fn estimated_context_tokens(&self) -> usize {
        self.messages
            .iter()
            .map(Message::estimated_chars)
            .sum::<usize>()
            / 4
    }

    #[must_use]
    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            title: self.display_title(),
            updated_at: self.updated_at,
            created_at: self.created_at,
            cwd: self.cwd.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            messages: self.messages.len(),
            turns: self.turns.len(),
            usage: self.usage,
            cost_usd: self.cost_usd,
            parent_id: self.parent_id.clone(),
            worktree: self.worktree.clone(),
        }
    }

    pub fn set_name(&mut self, name: Option<String>) -> Result<()> {
        if let Some(name) = &name {
            let name = name.trim();
            if name.is_empty() {
                self.name = None;
            } else if name.len() > 80 {
                bail!("session name is too long (max 80 characters)");
            } else {
                self.name = Some(name.to_string());
            }
        } else {
            self.name = None;
        }
        self.updated_at = Utc::now();
        Ok(())
    }

    pub fn fork(&self) -> Self {
        let now = Utc::now();
        let mut forked = self.clone();
        forked.id = Uuid::new_v4().to_string();
        forked.name = None;
        forked.title = format!("Fork of {}", self.display_title());
        forked.created_at = now;
        forked.updated_at = now;
        forked.parent_id = Some(self.id.clone());
        forked.turns.clear();
        forked.current_turn = 0;
        // A fork starts a fresh checkpoint lineage while preserving history.
        forked
    }
}

pub fn title_from_prompt(prompt: &str) -> String {
    let first = prompt
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("New session")
        .trim()
        .trim_start_matches(['#', '/', '-', '>'])
        .trim();
    let mut title: String = first.chars().take(64).collect();
    if first.chars().count() > 64 {
        title.push('…');
    }
    if title.is_empty() {
        "New session".into()
    } else {
        title
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub name: Option<String>,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub cwd: PathBuf,
    pub provider: String,
    pub model: String,
    pub messages: usize,
    pub turns: usize,
    pub usage: Usage,
    pub cost_usd: f64,
    pub parent_id: Option<String>,
    pub worktree: Option<PathBuf>,
}

impl SessionSummary {
    #[must_use]
    pub fn short_id(&self) -> &str {
        self.id.get(..8).unwrap_or(&self.id)
    }
}

#[derive(Clone)]
pub struct SessionStore {
    paths: AaykraPaths,
}

impl SessionStore {
    #[must_use]
    pub fn new(paths: AaykraPaths) -> Self {
        Self { paths }
    }

    #[must_use]
    pub fn paths(&self) -> &AaykraPaths {
        &self.paths
    }

    pub fn save(&self, session: &Session) -> Result<()> {
        let path = self.paths.session_file(&session.id);
        let body = serde_json::to_vec_pretty(session).context("could not serialize session")?;
        atomic_write(&path, &body)
    }

    pub fn load(&self, id: &str) -> Result<Session> {
        let path = self.paths.session_file(id);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("could not read session `{id}`"))?;
        let session: Session =
            serde_json::from_str(&text).with_context(|| format!("session `{id}` is corrupt"))?;
        Ok(session)
    }

    pub fn list(&self) -> Result<Vec<SessionSummary>> {
        let mut sessions = Vec::new();
        let entries = match std::fs::read_dir(&self.paths.sessions) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("could not list sessions"),
        };
        for entry in entries {
            let entry = match entry {
                Ok(value) => value,
                Err(_) => continue,
            };
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(session) = serde_json::from_str::<Session>(&text) else {
                continue;
            };
            sessions.push(session.summary());
        }
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(sessions)
    }

    pub fn latest(&self) -> Result<Option<Session>> {
        let mut summaries = self.list()?;
        summaries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        let Some(summary) = summaries.first() else {
            return Ok(None);
        };
        Ok(Some(self.load(&summary.id)?))
    }

    pub fn resolve(&self, reference: &str) -> Result<Session> {
        let reference = reference.trim();
        if reference.is_empty() {
            bail!("session reference is empty");
        }
        // Exact ID.
        if self.paths.session_file(reference).is_file() {
            return self.load(reference);
        }
        let sessions = self.list()?;
        let mut matches: Vec<&SessionSummary> = sessions
            .iter()
            .filter(|session| {
                session.id.starts_with(reference)
                    || session
                        .name
                        .as_deref()
                        .is_some_and(|name| name == reference)
            })
            .collect();
        if matches.is_empty() {
            matches = sessions
                .iter()
                .filter(|session| {
                    session
                        .title
                        .to_ascii_lowercase()
                        .contains(&reference.to_ascii_lowercase())
                        || session.name.as_deref().is_some_and(|name| {
                            name.to_ascii_lowercase()
                                .contains(&reference.to_ascii_lowercase())
                        })
                })
                .collect();
        }
        match matches.len() {
            0 => bail!("no session matching `{reference}`"),
            1 => self.load(&matches[0].id),
            _ => {
                let choices = matches
                    .iter()
                    .take(5)
                    .map(|s| format!("{} ({})", s.short_id(), s.title))
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!("session reference `{reference}` is ambiguous: {choices}")
            }
        }
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let path = self.paths.session_file(id);
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("could not delete session `{id}`"))?;
        }
        let checkpoint = self.paths.checkpoints.join(id);
        if checkpoint.exists() {
            std::fs::remove_dir_all(&checkpoint).ok();
        }
        Ok(())
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<Session> {
        let mut session = self.load(id)?;
        session.set_name(Some(name.to_string()))?;
        self.save(&session)?;
        Ok(session)
    }

    pub fn cleanup(&self, retention_days: u64) -> Result<usize> {
        if retention_days == 0 {
            return Ok(0);
        }
        let cutoff = Utc::now() - chrono::Duration::days(retention_days as i64);
        let mut removed = 0;
        for summary in self.list()? {
            if summary.updated_at < cutoff {
                self.delete(&summary.id)?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    #[must_use]
    pub fn render_list(sessions: &[SessionSummary]) -> String {
        if sessions.is_empty() {
            return "No sessions yet.".into();
        }
        let mut out = String::new();
        for session in sessions {
            out.push_str(&format!(
                "{}  {:<36} {:<24} {:>4} turns  {:>8}  {}\n",
                &session.id[..8.min(session.id.len())],
                truncate(&session.title, 36),
                format!("{}/{}", session.provider, compact_model(&session.model)),
                session.turns,
                format_cost(session.cost_usd),
                session.updated_at.format("%Y-%m-%d %H:%M")
            ));
        }
        out
    }
}

fn compact_model(model: &str) -> String {
    if model.chars().count() > 34 {
        format!("{}…", model.chars().take(33).collect::<String>())
    } else {
        model.to_string()
    }
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        format!(
            "{}…",
            value
                .chars()
                .take(max.saturating_sub(1))
                .collect::<String>()
        )
    }
}

/// Build a user message with optional image blocks. Images are placed before
/// text for vision models.
#[must_use]
pub fn user_message(prompt: String, images: &[ImageAttachment]) -> Message {
    let mut content = Vec::new();
    for image in images {
        content.push(ContentBlock::Image {
            media_type: image.media_type.clone(),
            data: image.data.clone(),
        });
    }
    if !prompt.is_empty() {
        content.push(ContentBlock::text(prompt));
    }
    Message {
        role: crate::types::Role::User,
        content,
    }
}

#[must_use]
pub fn find_session_files(paths: &AaykraPaths) -> Vec<PathBuf> {
    let mut result = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&paths.sessions) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                result.push(path);
            }
        }
    }
    result.sort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> AaykraPaths {
        let root = tempfile::tempdir().unwrap().keep();
        AaykraPaths {
            sessions: root.join("sessions"),
            checkpoints: root.join("checkpoints"),
            logs: root.join("logs"),
            skills: root.join("skills"),
            agents: root.join("agents"),
            cache: root.join("cache"),
            home: root.clone(),
        }
    }

    fn session() -> Session {
        Session::new(
            "anthropic",
            "claude-sonnet-4-5",
            PathBuf::from("/tmp/project"),
        )
    }

    #[test]
    fn round_trips_session() {
        let paths = paths();
        let store = SessionStore::new(paths);
        let mut session = session();
        session.set_name(Some("demo".into())).unwrap();
        let turn = session.begin_turn(Message::user_text("hello world"));
        session.finish_turn(
            Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Usage::default()
            },
            0.01,
            "end_turn",
        );
        assert_eq!(turn, 1);
        session.push_message(Message::assistant_text("hi"));
        store.save(&session).unwrap();
        let loaded = store.resolve("demo").unwrap();
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.title, "hello world");
    }

    #[test]
    fn undo_removes_turn_messages_and_usage() {
        let mut session = session();
        session.begin_turn(Message::user_text("first"));
        session.push_message(Message::assistant_text("answer"));
        session.finish_turn(
            Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Usage::default()
            },
            0.25,
            "end_turn",
        );
        assert_eq!(session.messages.len(), 2);
        let turn = session.undo_last_turn().unwrap();
        assert_eq!(turn.number, 1);
        assert!(session.messages.is_empty());
        assert_eq!(session.usage.input_tokens, 0);
        assert_eq!(session.current_turn, 0);
    }

    #[test]
    fn model_changes_leave_a_handoff_marker() {
        let mut session = session();
        assert!(!session.take_context_handoff());
        assert!(!session.set_model("anthropic", "claude-sonnet-4-5"));
        assert!(!session.take_context_handoff());
        assert!(session.set_model("openai", "gpt-5.1"));
        assert!(session.take_context_handoff());
        assert!(!session.take_context_handoff());
        assert_eq!(session.provider, "openai");
        assert_eq!(session.model, "gpt-5.1");
    }

    #[test]
    fn ambiguous_prefix_is_rejected() {
        let paths = paths();
        let store = SessionStore::new(paths);
        let mut a = session();
        a.id = "abc11111-1111-1111-1111-111111111111".into();
        a.set_name(Some("alpha".into())).unwrap();
        let mut b = session();
        b.id = "abc22222-2222-2222-2222-222222222222".into();
        b.set_name(Some("alphabet".into())).unwrap();
        store.save(&a).unwrap();
        store.save(&b).unwrap();
        assert!(store.resolve("abc").is_err());
        assert!(store.resolve("alpha").is_ok());
    }
}
