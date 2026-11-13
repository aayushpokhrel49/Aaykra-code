//! Interactive application state and event handling.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use tokio::sync::{mpsc, oneshot};

use super::{Theme, UiMessage};
use crate::agent::RunOutcome;
use crate::commands::{self, CommandAction};
use crate::images;
use crate::runtime::Runtime;
use crate::session::user_message;
use crate::types::{
    ContentBlock, ImageAttachment, Message, PermissionDecision, PermissionMode, PermissionRequest,
    RiskLevel, Role, TodoItem, Usage, UserAnswer, UserQuestion,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NoticeLevel {
    Info,
    Warn,
    Error,
    Success,
}

pub(super) enum Block {
    User {
        text: String,
        images: usize,
    },
    Assistant {
        text: String,
    },
    Thinking {
        text: String,
    },
    Tool {
        id: String,
        name: String,
        input: String,
        output: String,
        error: bool,
    },
    Notice {
        level: NoticeLevel,
        text: String,
    },
    Command {
        text: String,
    },
}

#[derive(Default)]
pub(super) struct InputState {
    pub text: String,
    /// Cursor position in characters, not bytes.
    pub cursor: usize,
    pub row_scroll: usize,
    pub col_scroll: usize,
}

impl InputState {
    pub fn is_blank(&self) -> bool {
        self.text.trim().is_empty()
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        self.row_scroll = 0;
        self.col_scroll = 0;
        std::mem::take(&mut self.text)
    }

    fn byte_index(&self, char_index: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_index)
            .map_or(self.text.len(), |(index, _)| index)
    }

    pub fn insert_str(&mut self, value: &str) {
        let byte = self.byte_index(self.cursor);
        self.text.insert_str(byte, value);
        self.cursor += value.chars().count();
    }

    pub fn newline(&mut self) {
        self.insert_str("\n");
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.byte_index(self.cursor - 1);
        let end = self.byte_index(self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.text.chars().count() {
            return;
        }
        let start = self.byte_index(self.cursor);
        let end = self.byte_index(self.cursor + 1);
        self.text.replace_range(start..end, "");
    }

    pub fn delete_word(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let chars: Vec<char> = self.text.chars().collect();
        let mut start = self.cursor;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        let start_byte = self.byte_index(start);
        let end_byte = self.byte_index(self.cursor);
        self.text.replace_range(start_byte..end_byte, "");
        self.cursor = start;
    }

    pub fn line_col(&self) -> (usize, usize) {
        let mut row = 0;
        let mut col = 0;
        for character in self.text.chars().take(self.cursor) {
            if character == '\n' {
                row += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        (row, col)
    }

    pub fn line_count(&self) -> usize {
        self.text.split('\n').count().max(1)
    }

    fn move_vertical(&mut self, delta: isize) {
        let lines: Vec<&str> = self.text.split('\n').collect();
        let (row, col) = self.line_col();
        let target =
            (row as isize + delta).clamp(0, lines.len().saturating_sub(1) as isize) as usize;
        let mut cursor = 0;
        for line in &lines[..target] {
            cursor += line.chars().count() + 1;
        }
        cursor += col.min(lines[target].chars().count());
        self.cursor = cursor;
    }

    pub fn home(&mut self) {
        let (_, col) = self.line_col();
        self.cursor = self.cursor.saturating_sub(col);
    }

    pub fn end(&mut self) {
        let lines: Vec<&str> = self.text.split('\n').collect();
        let (row, _) = self.line_col();
        let mut cursor = 0;
        for line in &lines[..row] {
            cursor += line.chars().count() + 1;
        }
        cursor += lines[row].chars().count();
        self.cursor = cursor;
    }

    pub fn cursor_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn cursor_right(&mut self) {
        if self.cursor < self.text.chars().count() {
            self.cursor += 1;
        }
    }

    /// Handle ordinary editing keys. Returns true when the key was consumed.
    pub fn handle_edit_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                let mut buffer = [0u8; 4];
                self.insert_str(character.encode_utf8(&mut buffer));
                true
            }
            KeyCode::Backspace => {
                self.backspace();
                true
            }
            KeyCode::Delete => {
                self.delete();
                true
            }
            KeyCode::Left => {
                self.cursor_left();
                true
            }
            KeyCode::Right => {
                self.cursor_right();
                true
            }
            KeyCode::Up => {
                self.move_vertical(-1);
                true
            }
            KeyCode::Down => {
                self.move_vertical(1);
                true
            }
            KeyCode::Home => {
                self.home();
                true
            }
            KeyCode::End => {
                self.end();
                true
            }
            KeyCode::Tab => {
                self.insert_str("    ");
                true
            }
            _ => false,
        }
    }

    /// Update scroll offsets so the cursor is visible, and return its position
    /// relative to the inner drawing area.
    pub fn visual_cursor(&mut self, width: u16, height: u16) -> (u16, u16) {
        let (row, col) = self.line_col();
        let width = width.max(1) as usize;
        let height = height.max(1) as usize;
        if row < self.row_scroll {
            self.row_scroll = row;
        } else if row >= self.row_scroll + height {
            self.row_scroll = row + 1 - height;
        }
        if col < self.col_scroll {
            self.col_scroll = col;
        } else if col >= self.col_scroll + width {
            self.col_scroll = col + 1 - width;
        }
        let visible_row = row.saturating_sub(self.row_scroll) as u16;
        let visible_col = col.saturating_sub(self.col_scroll) as u16;
        (
            visible_col.min(width.saturating_sub(1) as u16),
            visible_row.min(height.saturating_sub(1) as u16),
        )
    }
}

pub(super) struct PermissionModal {
    pub request: PermissionRequest,
    pub actions: Vec<PermissionAction>,
    pub selected: usize,
    pub diff_scroll: u16,
    reply: Option<oneshot::Sender<PermissionDecision>>,
}

#[derive(Debug, Clone)]
pub(super) enum PermissionAction {
    Allow,
    AllowForSession(String),
    Deny,
}

impl PermissionAction {
    pub fn label(&self) -> String {
        match self {
            Self::Allow => "Allow once".into(),
            Self::AllowForSession(rule) => format!("Allow for this session ({rule})"),
            Self::Deny => "Deny".into(),
        }
    }

    fn decision(&self) -> PermissionDecision {
        match self {
            Self::Allow => PermissionDecision::Allow,
            Self::AllowForSession(rule) => {
                PermissionDecision::AllowForSession { rule: rule.clone() }
            }
            Self::Deny => PermissionDecision::Deny {
                reason: Some("denied by the user".into()),
            },
        }
    }
}

impl PermissionModal {
    fn new(request: PermissionRequest, reply: oneshot::Sender<PermissionDecision>) -> Self {
        let mut actions = vec![PermissionAction::Allow];
        if !request.suggested_rule.trim().is_empty() {
            actions.push(PermissionAction::AllowForSession(
                request.suggested_rule.clone(),
            ));
        }
        actions.push(PermissionAction::Deny);
        let selected = if matches!(request.risk, RiskLevel::High | RiskLevel::Critical) {
            actions.len() - 1
        } else {
            0
        };
        Self {
            request,
            actions,
            selected,
            diff_scroll: 0,
            reply: Some(reply),
        }
    }

    pub fn risk_label(&self) -> &'static str {
        match self.request.risk {
            RiskLevel::Low => "low risk",
            RiskLevel::Medium => "medium risk",
            RiskLevel::High => "high risk",
            RiskLevel::Critical => "critical risk",
        }
    }
}

pub(super) struct QuestionModal {
    pub question: UserQuestion,
    pub selected: usize,
    pub toggled: Vec<bool>,
    pub custom: InputState,
    pub custom_active: bool,
    reply: Option<oneshot::Sender<Option<UserAnswer>>>,
}

impl QuestionModal {
    fn new(question: UserQuestion, reply: oneshot::Sender<Option<UserAnswer>>) -> Self {
        let toggled = vec![false; question.options.len()];
        let custom_active = question.options.is_empty();
        Self {
            question,
            selected: 0,
            toggled,
            custom: InputState::default(),
            custom_active,
            reply: Some(reply),
        }
    }

    pub fn other_index(&self) -> usize {
        self.question.options.len()
    }

    pub fn item_count(&self) -> usize {
        self.question.options.len() + 1
    }
}

pub(super) enum Modal {
    Permission(PermissionModal),
    Question(QuestionModal),
}

pub(super) struct QueuedPrompt {
    text: String,
    images: Vec<ImageAttachment>,
}

pub(super) struct App {
    pub runtime: Arc<Runtime>,
    pub ui_tx: mpsc::UnboundedSender<UiMessage>,
    pub transcript: Vec<Block>,
    pub input: InputState,
    pub attachments: Vec<ImageAttachment>,
    pub running_turn: bool,
    pub command_running: bool,
    pub cancel_requested: bool,
    pub queued: VecDeque<QueuedPrompt>,
    pub todos: Vec<TodoItem>,
    pub usage: Usage,
    pub turn_usage: Usage,
    pub cost: f64,
    pub context_tokens: usize,
    pub context_limit: usize,
    pub provider: String,
    pub model: String,
    pub agent: String,
    pub cwd: String,
    pub mode: PermissionMode,
    pub session_title: String,
    pub files_changed: usize,
    pub turn: u64,
    pub turn_started: Option<Instant>,
    pub follow: bool,
    pub scroll: u16,
    pub max_scroll: u16,
    pub show_thinking: bool,
    pub show_tool_expanded: bool,
    pub modal: Option<Modal>,
    pub should_quit: bool,
    pub theme: Theme,
    pub spinner: u64,
    pub notice: Option<(String, Instant)>,
    pub mcp_count: usize,
    pub lsp_count: usize,
    pub last_turn_summary: Option<String>,
}

impl App {
    pub fn new(
        runtime: Arc<Runtime>,
        ui_tx: mpsc::UnboundedSender<UiMessage>,
        theme: Theme,
        show_thinking: bool,
    ) -> Self {
        let snapshot = runtime
            .session
            .try_lock()
            .ok()
            .map(|session| session.clone());
        let mut transcript = Vec::new();
        if let Some(session) = &snapshot {
            transcript = transcript_from_messages(&session.messages);
        }
        let (
            provider,
            model,
            agent,
            mode,
            cwd,
            session_title,
            turn,
            usage,
            cost,
            todos,
            files_changed,
            context_tokens,
        ) = match snapshot {
            Some(session) => {
                let session_title = session.display_title();
                let files_changed = session.files_changed.len();
                let context_tokens = session.estimated_context_tokens();
                (
                    session.provider,
                    session.model,
                    session.agent,
                    session.permission_mode,
                    session.cwd.display().to_string(),
                    session_title,
                    session.current_turn,
                    session.usage,
                    session.cost_usd,
                    session.todos,
                    files_changed,
                    context_tokens,
                )
            }
            None => (
                String::new(),
                String::new(),
                String::new(),
                runtime.permissions.mode(),
                runtime.cwd().display().to_string(),
                "New session".into(),
                0,
                Usage::default(),
                0.0,
                Vec::new(),
                0,
                0,
            ),
        };
        let context_limit = runtime.context_limit();
        let show_tool_expanded =
            runtime.config.ui.show_tool_output || !runtime.config.ui.compact_tools;
        let mcp_count = runtime.mcp.configured_names().len();
        let lsp_count = runtime.config.lsp.len();
        Self {
            runtime,
            ui_tx,
            transcript,
            input: InputState::default(),
            attachments: Vec::new(),
            running_turn: false,
            command_running: false,
            cancel_requested: false,
            queued: VecDeque::new(),
            todos,
            usage,
            turn_usage: Usage::default(),
            cost,
            context_tokens,
            context_limit,
            provider,
            model,
            agent,
            cwd,
            mode,
            session_title,
            files_changed,
            turn,
            turn_started: None,
            follow: true,
            scroll: 0,
            max_scroll: 0,
            show_thinking,
            show_tool_expanded,
            modal: None,
            should_quit: false,
            theme,
            spinner: 0,
            notice: None,
            mcp_count,
            lsp_count,
            last_turn_summary: None,
        }
    }

    pub fn busy(&self) -> bool {
        self.running_turn || self.command_running
    }

    pub fn tick(&mut self) {
        self.spinner = self.spinner.wrapping_add(1);
        if let Some((_, at)) = &self.notice
            && at.elapsed() > Duration::from_secs(5)
        {
            self.notice = None;
        }
    }

    pub fn push_block(&mut self, block: Block) {
        self.transcript.push(block);
        if self.follow {
            self.scroll = u16::MAX;
        }
    }

    pub fn push_notice(&mut self, text: impl Into<String>, level: NoticeLevel) {
        self.push_block(Block::Notice {
            level,
            text: text.into(),
        });
    }

    fn set_notice(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), Instant::now()));
    }

    pub fn submit_external(&mut self, text: String, images: Vec<ImageAttachment>) {
        self.submit_text(text, images);
    }

    fn submit(&mut self) {
        if self.input.is_blank() && self.attachments.is_empty() {
            return;
        }
        let text = self.input.take();
        let images = std::mem::take(&mut self.attachments);
        self.submit_text(text, images);
    }

    fn submit_text(&mut self, text: String, images: Vec<ImageAttachment>) {
        if self.busy() {
            self.queued.push_back(QueuedPrompt {
                text: text.clone(),
                images,
            });
            let preview: String = text.trim().chars().take(60).collect();
            self.set_notice(format!("Queued: {preview}"));
            return;
        }
        let trimmed = text.trim();
        if trimmed.is_empty() && images.is_empty() {
            return;
        }
        if let Some(literal) = trimmed.strip_prefix("//") {
            self.start_turn(literal.to_string(), images);
            return;
        }
        if commands::is_command(trimmed) && images.is_empty() {
            self.command_running = true;
            self.push_block(Block::Command {
                text: format!("▸ {trimmed}"),
            });
            let runtime = self.runtime.clone();
            let tx = self.ui_tx.clone();
            let command = trimmed.to_string();
            tokio::spawn(async move {
                let action = commands::handle(runtime, &command).await;
                let _ = tx.send(UiMessage::CommandDone(action));
            });
            return;
        }
        self.start_turn(text, images);
    }

    fn start_turn(&mut self, text: String, images: Vec<ImageAttachment>) {
        if self.busy() {
            self.queued.push_back(QueuedPrompt { text, images });
            return;
        }
        self.running_turn = true;
        self.cancel_requested = false;
        self.turn_started = Some(Instant::now());
        self.turn_usage = Usage::default();
        self.push_block(Block::User {
            text: text.clone(),
            images: images.len(),
        });
        self.follow = true;
        self.scroll = u16::MAX;
        let runtime = self.runtime.clone();
        let tx = self.ui_tx.clone();
        let message = user_message(text, &images);
        tokio::spawn(async move {
            let result = crate::agent::run_turn(runtime, message, None)
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = tx.send(UiMessage::TurnDone(result));
        });
    }

    fn process_queue(&mut self) {
        if self.busy() {
            return;
        }
        if let Some(next) = self.queued.pop_front() {
            self.submit_text(next.text, next.images);
        }
    }

    pub fn handle_terminal_event(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Paste(text) => {
                let text = text.replace('\r', "").replace('\x1b', "");
                self.input.insert_str(&text);
            }
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if self.modal.is_some() {
            self.handle_modal_key(key);
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    if self.running_turn {
                        if !self.cancel_requested {
                            self.cancel_requested = true;
                            self.runtime.cancel.cancel();
                            self.set_notice("Cancelling…");
                        }
                    } else {
                        self.set_notice("Press Ctrl+D to quit");
                    }
                    return;
                }
                KeyCode::Char('d') => {
                    if self.running_turn {
                        self.runtime.cancel.cancel();
                    }
                    self.should_quit = true;
                    return;
                }
                KeyCode::Char('o') => {
                    self.show_tool_expanded = !self.show_tool_expanded;
                    self.set_notice(if self.show_tool_expanded {
                        "Tool output expanded"
                    } else {
                        "Tool output compact"
                    });
                    return;
                }
                KeyCode::Char('v') => {
                    match images::read_clipboard() {
                        Ok(Some(image)) => {
                            self.attachments.push(image);
                            self.set_notice(format!(
                                "Pasted image ({} attached)",
                                self.attachments.len()
                            ));
                        }
                        Ok(None) => self.set_notice("No image found on the clipboard"),
                        Err(error) => self.set_notice(format!("Clipboard error: {error:#}")),
                    }
                    return;
                }
                KeyCode::Char('u') => {
                    self.input = InputState::default();
                    return;
                }
                KeyCode::Char('w') => {
                    self.input.delete_word();
                    return;
                }
                KeyCode::Char('l') => {
                    self.transcript.clear();
                    return;
                }
                KeyCode::Char('t') => {
                    self.show_thinking = !self.show_thinking;
                    self.set_notice(if self.show_thinking {
                        "Thinking visible"
                    } else {
                        "Thinking hidden"
                    });
                    return;
                }
                KeyCode::Char('j') => {
                    self.input.newline();
                    return;
                }
                _ => {}
            }
        }

        if key.code == KeyCode::BackTab
            || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT))
        {
            self.cycle_mode();
            return;
        }

        match key.code {
            KeyCode::Enter => {
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
                {
                    self.input.newline();
                } else {
                    self.submit();
                }
            }
            KeyCode::PageUp => self.scroll_up(10),
            KeyCode::PageDown => self.scroll_down(10),
            KeyCode::Esc => {
                if self.running_turn {
                    self.runtime.cancel.cancel();
                    self.cancel_requested = true;
                    self.set_notice("Cancelling…");
                }
            }
            _ => {
                self.input.handle_edit_key(key);
            }
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_up(3),
            MouseEventKind::ScrollDown => self.scroll_down(3),
            _ => {}
        }
    }

    fn cycle_mode(&mut self) {
        let current = self.runtime.permissions.mode();
        let next = current.next();
        if next == PermissionMode::Plan {
            self.runtime.enter_plan_mode();
        } else {
            self.runtime.set_mode(next);
        }
        self.mode = next;
    }

    pub fn scroll_up(&mut self, amount: u16) {
        self.follow = false;
        self.scroll = self.scroll.saturating_sub(amount);
    }

    pub fn scroll_down(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_add(amount);
        if self.scroll >= self.max_scroll {
            self.scroll = self.max_scroll;
            self.follow = true;
        }
    }

    pub async fn handle_ui_message(&mut self, message: UiMessage) {
        match message {
            UiMessage::Event(event) => self.handle_agent_event(event),
            UiMessage::Authorize { request, reply } => {
                if self.modal.is_some() {
                    let _ = reply.send(PermissionDecision::Deny {
                        reason: Some("another prompt is already open".into()),
                    });
                } else {
                    self.modal = Some(Modal::Permission(PermissionModal::new(request, reply)));
                }
            }
            UiMessage::Ask { question, reply } => {
                if self.modal.is_some() {
                    let _ = reply.send(None);
                } else {
                    self.modal = Some(Modal::Question(QuestionModal::new(question, reply)));
                }
            }
            UiMessage::TurnDone(result) => self.finish_turn(result).await,
            UiMessage::CommandDone(action) => self.finish_command(action).await,
        }
    }

    async fn finish_turn(&mut self, result: Result<RunOutcome, String>) {
        self.running_turn = false;
        self.turn_started = None;
        let cancelled = self.cancel_requested;
        self.cancel_requested = false;
        self.modal = None;
        match result {
            Ok(outcome) => {
                if cancelled {
                    self.push_notice("Turn cancelled.", NoticeLevel::Warn);
                }
                self.last_turn_summary = Some(format!(
                    "{} · {} in / {} out · {}",
                    outcome.stop_reason,
                    crate::types::compact_number(outcome.usage.input_tokens),
                    crate::types::compact_number(outcome.usage.output_tokens),
                    crate::models::format_cost(outcome.cost_usd)
                ));
            }
            Err(error) => {
                if cancelled {
                    self.push_notice("Turn cancelled.", NoticeLevel::Warn);
                } else {
                    self.push_notice(format!("Turn failed: {error}"), NoticeLevel::Error);
                }
            }
        }
        self.refresh_snapshot().await;
        self.process_queue();
    }

    async fn finish_command(&mut self, action: CommandAction) {
        self.command_running = false;
        self.refresh_snapshot().await;
        match action {
            CommandAction::NotACommand => {
                self.push_notice("Internal: command was not recognized.", NoticeLevel::Error);
            }
            CommandAction::Reply(text) => self.push_block(Block::Command { text }),
            CommandAction::RunPrompt(prompt) => self.start_turn(prompt, Vec::new()),
            CommandAction::Quit => self.should_quit = true,
            CommandAction::Clear => {
                self.transcript.clear();
                self.push_notice("Context cleared.", NoticeLevel::Success);
            }
        }
        self.process_queue();
    }

    async fn refresh_snapshot(&mut self) {
        let session = self.runtime.session_snapshot().await;
        self.usage = session.usage;
        self.cost = session.cost_usd;
        self.todos = session.todos.clone();
        self.provider = session.provider.clone();
        self.model = session.model.clone();
        self.agent = session.agent.clone();
        self.mode = session.permission_mode;
        self.cwd = session.cwd.display().to_string();
        self.session_title = session.display_title();
        self.files_changed = session.files_changed.len();
        self.context_tokens = session.estimated_context_tokens();
        self.turn = session.current_turn;
        self.context_limit = self.runtime.context_limit();
    }

    fn handle_agent_event(&mut self, event: crate::types::AgentEvent) {
        use crate::types::AgentEvent;
        match event {
            AgentEvent::TextDelta(text) => {
                if let Some(Block::Assistant { text: existing }) = self.transcript.last_mut() {
                    existing.push_str(&text);
                } else {
                    self.transcript.push(Block::Assistant { text });
                }
                if self.follow {
                    self.scroll = u16::MAX;
                }
            }
            AgentEvent::ThinkingDelta(text) if self.show_thinking => {
                if let Some(Block::Thinking { text: existing }) = self.transcript.last_mut() {
                    existing.push_str(&text);
                } else {
                    self.transcript.push(Block::Thinking { text });
                }
            }
            AgentEvent::ThinkingDelta(_) => {}
            AgentEvent::ToolStarted { id, name, input } => {
                self.transcript.push(Block::Tool {
                    id,
                    name,
                    input,
                    output: String::new(),
                    error: false,
                });
            }
            AgentEvent::ToolFinished {
                id,
                name: _,
                output,
                is_error,
            } => {
                if let Some(Block::Tool {
                    output: existing,
                    error,
                    ..
                }) = self.transcript.iter_mut().rev().find(
                    |block| matches!(block, Block::Tool { id: tool_id, .. } if *tool_id == id),
                ) {
                    *existing = output;
                    *error = is_error;
                } else {
                    self.transcript.push(Block::Tool {
                        id,
                        name: "tool".into(),
                        input: String::new(),
                        output,
                        error: is_error,
                    });
                }
            }
            AgentEvent::Notice(text) => {
                self.push_notice(text, NoticeLevel::Info);
            }
            AgentEvent::Warning(text) => {
                self.push_notice(text, NoticeLevel::Warn);
            }
            AgentEvent::Error(text) => {
                self.push_notice(text, NoticeLevel::Error);
            }
            AgentEvent::TodoUpdated(todos) => self.todos = todos,
            AgentEvent::Usage {
                turn,
                session_usage,
                session_cost_usd,
            } => {
                self.turn_usage = turn;
                self.usage = session_usage;
                self.cost = session_cost_usd;
                self.context_tokens = self.context_tokens.max(turn.input_tokens as usize);
            }
            AgentEvent::CompactionStarted => {
                self.push_notice("Compacting conversation…", NoticeLevel::Info);
            }
            AgentEvent::CompactionFinished { removed_messages } => {
                self.push_notice(
                    format!("Compacted {removed_messages} messages."),
                    NoticeLevel::Info,
                );
            }
            AgentEvent::SubagentStarted { description, .. } => {
                self.push_notice(
                    format!("Subagent started: {description}"),
                    NoticeLevel::Info,
                );
            }
            AgentEvent::SubagentFinished {
                success, summary, ..
            } => {
                self.push_notice(
                    format!(
                        "Subagent {}: {}",
                        if success { "finished" } else { "failed" },
                        summary.chars().take(200).collect::<String>()
                    ),
                    if success {
                        NoticeLevel::Success
                    } else {
                        NoticeLevel::Error
                    },
                );
            }
            AgentEvent::PermissionModeChanged(mode) => self.mode = mode,
            AgentEvent::ModelChanged { provider, model } => {
                self.provider = provider;
                self.model = model;
            }
            AgentEvent::TurnStarted { turn } => {
                self.turn = turn;
            }
            AgentEvent::TurnFinished {
                usage,
                cost_usd,
                session_cost_usd,
                ..
            } => {
                self.turn_usage = usage;
                self.cost = session_cost_usd;
                let _ = cost_usd;
            }
            AgentEvent::RunFailed(error) => {
                self.push_notice(format!("Run failed: {error}"), NoticeLevel::Error);
            }
            AgentEvent::QueuedUserMessage(text) => {
                self.push_notice(format!("Queued: {text}"), NoticeLevel::Info);
            }
            AgentEvent::SessionStarted { title, .. } => {
                if self.session_title == "New session" && !title.is_empty() {
                    self.session_title = title;
                }
            }
            // Permission and question prompts are delivered to the TUI through
            // its dedicated gateway channels, not through the event stream.
            AgentEvent::PermissionRequested { .. }
            | AgentEvent::PermissionResolved { .. }
            | AgentEvent::QuestionRequested { .. }
            | AgentEvent::QuestionResolved { .. } => {}
        }
    }

    fn handle_modal_key(&mut self, key: KeyEvent) {
        let Some(modal) = self.modal.take() else {
            return;
        };
        match modal {
            Modal::Permission(mut permission) => {
                let mut finish = false;
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        permission.selected = permission.selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                        permission.selected =
                            (permission.selected + 1).min(permission.actions.len() - 1);
                    }
                    KeyCode::PageUp => {
                        permission.diff_scroll = permission.diff_scroll.saturating_sub(5);
                    }
                    KeyCode::PageDown => {
                        permission.diff_scroll = permission.diff_scroll.saturating_add(5);
                    }
                    KeyCode::Char('y') => {
                        permission.selected = 0;
                        finish = true;
                    }
                    KeyCode::Char('a') => {
                        if let Some(index) = permission.actions.iter().position(|action| {
                            matches!(action, PermissionAction::AllowForSession(_))
                        }) {
                            permission.selected = index;
                            finish = true;
                        }
                    }
                    KeyCode::Char('n') | KeyCode::Esc => {
                        permission.selected = permission.actions.len() - 1;
                        finish = true;
                    }
                    KeyCode::Enter => finish = true,
                    _ => {}
                }
                if finish {
                    let action = permission.actions[permission.selected].clone();
                    let decision = action.decision();
                    let label = action.label();
                    if let Some(reply) = permission.reply.take() {
                        let _ = reply.send(decision);
                    }
                    let level = if matches!(action, PermissionAction::Deny) {
                        NoticeLevel::Warn
                    } else {
                        NoticeLevel::Success
                    };
                    self.push_notice(format!("{label}: {}", permission.request.tool), level);
                } else {
                    self.modal = Some(Modal::Permission(permission));
                }
            }
            Modal::Question(mut question) => {
                let mut finish: Option<Option<UserAnswer>> = None;
                if question.custom_active {
                    match key.code {
                        KeyCode::Enter
                            if key
                                .modifiers
                                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
                        {
                            question.custom.newline();
                        }
                        KeyCode::Enter => {
                            let text = question.custom.text.trim().to_string();
                            if !text.is_empty() {
                                finish = Some(Some(UserAnswer::Custom { text }));
                            }
                        }
                        KeyCode::Esc => question.custom_active = false,
                        _ => {
                            question.custom.handle_edit_key(key);
                        }
                    }
                } else {
                    match key.code {
                        KeyCode::Up | KeyCode::Char('k') => {
                            question.selected = question.selected.saturating_sub(1);
                        }
                        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                            question.selected =
                                (question.selected + 1).min(question.item_count() - 1);
                        }
                        KeyCode::Char(' ') => {
                            if question.question.multiple
                                && question.selected < question.question.options.len()
                            {
                                let slot = &mut question.toggled[question.selected];
                                *slot = !*slot;
                            }
                        }
                        KeyCode::Enter => {
                            if question.selected == question.other_index() {
                                question.custom_active = true;
                            } else if question.question.multiple {
                                let values: Vec<String> = question
                                    .toggled
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, toggled)| **toggled)
                                    .map(|(index, _)| {
                                        question.question.options[index].label.clone()
                                    })
                                    .collect();
                                if values.is_empty() {
                                    finish = Some(Some(UserAnswer::Selected {
                                        values: vec![
                                            question.question.options[question.selected]
                                                .label
                                                .clone(),
                                        ],
                                    }));
                                } else {
                                    finish = Some(Some(UserAnswer::Selected { values }));
                                }
                            } else {
                                finish = Some(Some(UserAnswer::Selected {
                                    values: vec![
                                        question.question.options[question.selected].label.clone(),
                                    ],
                                }));
                            }
                        }
                        KeyCode::Esc => finish = Some(None),
                        _ => {}
                    }
                }
                if let Some(answer) = finish {
                    if let Some(reply) = question.reply.take() {
                        let _ = reply.send(answer);
                    }
                    self.modal = None;
                } else {
                    self.modal = Some(Modal::Question(question));
                }
            }
        }
    }
}

fn transcript_from_messages(messages: &[Message]) -> Vec<Block> {
    let mut blocks = Vec::new();
    for message in messages {
        match message.role {
            Role::User => {
                let text = message.text();
                let images = message
                    .content
                    .iter()
                    .filter(|block| matches!(block, ContentBlock::Image { .. }))
                    .count();
                if !text.trim().is_empty() || images > 0 {
                    blocks.push(Block::User { text, images });
                }
                for block in &message.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } = block
                    {
                        if let Some(Block::Tool {
                            output, error, ..
                        }) = blocks.iter_mut().find(|existing| {
                            matches!(existing, Block::Tool { id, .. } if id == tool_use_id)
                        }) {
                            *output = content.clone();
                            *error = *is_error;
                        } else {
                            blocks.push(Block::Tool {
                                id: tool_use_id.clone(),
                                name: "tool".into(),
                                input: String::new(),
                                output: content.clone(),
                                error: *is_error,
                            });
                        }
                    }
                }
            }
            Role::Assistant => {
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } if !text.trim().is_empty() => {
                            blocks.push(Block::Assistant { text: text.clone() });
                        }
                        ContentBlock::Thinking { thinking, .. } => {
                            blocks.push(Block::Thinking {
                                text: thinking.clone(),
                            });
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            blocks.push(Block::Tool {
                                id: id.clone(),
                                name: name.clone(),
                                input: input.to_string(),
                                output: String::new(),
                                error: false,
                            });
                        }
                        _ => {}
                    }
                }
            }
            Role::System => {}
        }
    }
    blocks
}
