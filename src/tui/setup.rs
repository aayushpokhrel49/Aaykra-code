//! In-app provider onboarding.
//!
//! When the resolved provider needs an API key and none is configured, the
//! TUI shows this setup flow before the main interface: pick a provider,
//! paste a masked API key, or register a custom OpenAI-compatible endpoint.
//! Nothing leaves the machine: keys are stored in the mode-0600 credential
//! store and the custom provider is appended to the user config file.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use tokio::sync::mpsc;

use super::app::InputState;
use super::{TerminalSession, Theme};
use crate::auth::Credentials;
use crate::bootstrap::{ConfigOverrides, Services};
use crate::paths::AaykraPaths;
use crate::session::Session;

/// What the setup flow ended up choosing, if the user did not quit.
pub(super) struct SetupOutcome {
    pub provider: String,
    pub model: String,
    /// True when a custom provider was written to the config and services
    /// must be reloaded before use.
    pub reload_config: bool,
}

/// Everything needed to rebuild services after onboarding mutates config.
#[derive(Default)]
pub struct SetupContext {
    pub config_path: Option<PathBuf>,
    pub overrides: ConfigOverrides,
    /// Overrides to apply when a brand-new session must be created after
    /// onboarding (used when session construction failed before setup).
    pub agent: Option<String>,
    pub mode: Option<crate::types::PermissionMode>,
    pub name: Option<String>,
}

struct ProviderEntry {
    name: String,
    requires_key: bool,
    has_key: bool,
    default_model: Option<String>,
}

impl ProviderEntry {
    fn status(&self) -> String {
        if self.has_key {
            "✓ ready".to_string()
        } else if self.requires_key {
            "set API key".to_string()
        } else {
            "local · no key needed".to_string()
        }
    }
}

#[derive(Default)]
struct CustomForm {
    url: InputState,
    model: InputState,
    key: InputState,
    focus: usize,
    reveal_key: bool,
}

struct ApiKeyForm {
    field: InputState,
    reveal: bool,
}

impl Default for ApiKeyForm {
    fn default() -> Self {
        Self {
            field: InputState::default(),
            reveal: false,
        }
    }
}

enum SetupState {
    ProviderSelect { selected: usize },
    ApiKey { provider: String, form: ApiKeyForm },
    Custom { form: CustomForm },
}

enum PendingOutcome {
    None,
    Done(SetupOutcome),
}

/// A key decision that has been detached from the live state so the action
/// can touch other parts of `Setup` without fighting the borrow checker.
enum Action {
    MoveSelection(isize),
    Choose,
    Quit,
    Back(usize),
    SetReveal(bool),
    SaveKey(String, String),
    FocusDelta(isize),
    SubmitCustom,
    None,
}

struct Setup {
    services: Services,
    session: Option<Session>,
    state: SetupState,
    entries: Vec<ProviderEntry>,
    theme: Theme,
    notice: Option<(String, bool, Instant)>,
    pending: PendingOutcome,
}

impl Services {
    /// Cheap clone for the setup struct; every field is an `Arc` or a plain
    /// cloneable value.
    fn clone_ref(&self) -> Services {
        Services {
            config: self.config.clone(),
            paths: self.paths.clone(),
            cwd: self.cwd.clone(),
            store: self.store.clone(),
            skills: self.skills.clone(),
            agents: self.agents.clone(),
            hooks: self.hooks.clone(),
            mcp: self.mcp.clone(),
            lsp: self.lsp.clone(),
            provider_runtime: self.provider_runtime.clone(),
            warnings: self.warnings.clone(),
            config_files: self.config_files.clone(),
        }
    }
}

/// Run the setup flow. Returns `None` when the user quits.
pub(super) async fn run(
    terminal: &mut TerminalSession,
    services: &Services,
    session: Option<&Session>,
    _ctx: &SetupContext,
) -> Result<Option<SetupOutcome>> {
    let mut entries: Vec<ProviderEntry> = services
        .config
        .enabled_providers()
        .map(|(name, config)| ProviderEntry {
            name: name.to_string(),
            requires_key: config.requires_key,
            has_key: services.provider_runtime.has_key(name),
            default_model: config.default_model.clone(),
        })
        .collect();
    // Prefer the session's provider; otherwise the first provider that needs
    // a missing key (so Enter leads straight to the key form).
    let preferred = session
        .map(|session| session.provider.clone())
        .or_else(|| {
            services
                .config
                .enabled_providers()
                .find_map(|(name, config)| {
                    (config.requires_key && !services.provider_runtime.has_key(name))
                        .then(|| name.to_string())
                })
        })
        .unwrap_or_default();
    entries.sort_by_key(|entry| (!entry.has_key, entry.requires_key, entry.name.clone()));
    let selected = entries
        .iter()
        .position(|entry| entry.name == preferred)
        .unwrap_or(0);

    let mut setup = Setup {
        services: services.clone_ref(),
        session: session.cloned(),
        state: SetupState::ProviderSelect { selected },
        entries,
        theme: Theme::from_name(&services.config.ui.theme),
        notice: None,
        pending: PendingOutcome::None,
    };

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();
    super::spawn_terminal_reader(event_tx);
    let mut ticker = tokio::time::interval(Duration::from_millis(200));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            Some(event) = event_rx.recv() => setup.handle_event(event),
            _ = ticker.tick() => setup.tick(),
            else => break Ok(None),
        }
        let _ = terminal.terminal.autoresize();
        if let Err(error) = terminal.terminal.draw(|frame| setup.render(frame)) {
            break Err(anyhow::anyhow!("could not draw setup: {error}"));
        }
        match std::mem::replace(&mut setup.pending, PendingOutcome::None) {
            PendingOutcome::None => {}
            PendingOutcome::Done(outcome) => break Ok(Some(outcome)),
        }
    }
}

impl Setup {
    fn tick(&mut self) {
        if let Some((_, _, at)) = &self.notice
            && at.elapsed() > Duration::from_secs(5)
        {
            self.notice = None;
        }
    }

    fn set_notice(&mut self, text: impl Into<String>, failed: bool) {
        self.notice = Some((text.into(), failed, Instant::now()));
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Paste(text) => {
                let text = text.replace('\r', "").replace('\x1b', "");
                match &mut self.state {
                    SetupState::ProviderSelect { .. } => {}
                    SetupState::ApiKey { form, .. } => form.field.insert_str(&text),
                    SetupState::Custom { form } => match form.focus {
                        0 => form.url.insert_str(&text),
                        1 => form.model.insert_str(&text),
                        _ => form.key.insert_str(&text),
                    },
                }
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let action = resolve_key(&mut self.state, &self.entries, key);
        self.apply_action(action);
    }

    fn apply_action(&mut self, action: Action) {
        match action {
            Action::MoveSelection(delta) => {
                if let SetupState::ProviderSelect { selected } = &mut self.state {
                    let max = self.entries.len();
                    *selected = if delta < 0 {
                        selected.saturating_sub(1)
                    } else {
                        (*selected + 1).min(max)
                    };
                }
            }
            Action::Choose => {
                let selected = match &self.state {
                    SetupState::ProviderSelect { selected } => *selected,
                    _ => 0,
                };
                self.choose_provider(selected);
            }
            Action::Quit => {
                // A missing provider (empty string) signals "leave without
                // configuring"; the caller treats it as a clean quit.
                self.pending = PendingOutcome::Done(SetupOutcome {
                    provider: self
                        .session
                        .as_ref()
                        .map(|session| session.provider.clone())
                        .unwrap_or_default(),
                    model: self
                        .session
                        .as_ref()
                        .map(|session| session.model.clone())
                        .unwrap_or_default(),
                    reload_config: false,
                });
            }
            Action::Back(selected) => {
                self.state = SetupState::ProviderSelect { selected };
            }
            Action::SetReveal(reveal) => {
                if let SetupState::ApiKey { form, .. } = &mut self.state {
                    form.reveal = reveal;
                }
            }
            Action::SaveKey(provider, key) => {
                if key.is_empty() {
                    self.set_notice("Paste or type the API key first.", true);
                    return;
                }
                match Credentials::set(&self.services.paths, &provider, &key) {
                    Ok(()) => {
                        let model = self.default_model(&provider);
                        self.pending = PendingOutcome::Done(SetupOutcome {
                            provider,
                            model,
                            reload_config: false,
                        });
                    }
                    Err(error) => {
                        self.set_notice(format!("Could not save the key: {error:#}"), true);
                    }
                }
            }
            Action::FocusDelta(delta) => {
                if let SetupState::Custom { form } = &mut self.state {
                    form.focus = if delta < 0 {
                        form.focus.saturating_sub(1)
                    } else {
                        (form.focus + 1).min(2)
                    };
                }
            }
            Action::SubmitCustom => {
                let (url, model, key, focus) = match &self.state {
                    SetupState::Custom { form } => (
                        form.url.text.trim().to_string(),
                        form.model.text.trim().to_string(),
                        form.key.text.trim().to_string(),
                        form.focus,
                    ),
                    _ => return,
                };
                if focus < 2 {
                    if let SetupState::Custom { form } = &mut self.state {
                        form.focus = focus + 1;
                    }
                } else {
                    self.submit_custom(url, model, key);
                }
            }
            Action::None => {}
        }
    }

    fn submit_custom(&mut self, url: String, model: String, key: String) {
        if url.is_empty() {
            self.set_notice("A base URL is required.", true);
            return;
        }
        if model.is_empty() {
            self.set_notice("A default model is required.", true);
            return;
        }
        let key = if key.is_empty() { None } else { Some(key) };
        match save_custom_provider(&self.services.paths, &url, &model, key.as_deref()) {
            Ok(()) => {
                self.pending = PendingOutcome::Done(SetupOutcome {
                    provider: "custom".into(),
                    model,
                    reload_config: true,
                });
            }
            Err(error) => self.set_notice(format!("{error:#}"), true),
        }
    }

    fn default_model(&self, provider: &str) -> String {
        self.entries
            .iter()
            .find(|entry| entry.name == provider)
            .and_then(|entry| entry.default_model.clone())
            .unwrap_or_else(|| "gpt-5.1".into())
    }

    fn choose_provider(&mut self, selected: usize) {
        if selected >= self.entries.len() {
            self.state = SetupState::Custom {
                form: CustomForm::default(),
            };
            return;
        }
        let entry = &self.entries[selected];
        if entry.requires_key && !entry.has_key {
            self.state = SetupState::ApiKey {
                provider: entry.name.clone(),
                form: ApiKeyForm::default(),
            };
            return;
        }
        let Some(model) = entry.default_model.clone() else {
            self.set_notice(
                format!("Provider `{}` has no default model configured.", entry.name),
                true,
            );
            return;
        };
        self.pending = PendingOutcome::Done(SetupOutcome {
            provider: entry.name.clone(),
            model,
            reload_config: false,
        });
    }

    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        match &mut self.state {
            SetupState::ProviderSelect { selected } => {
                let selected = *selected;
                render_provider_select(
                    frame,
                    area,
                    &self.entries,
                    &self.theme,
                    self.notice.as_ref(),
                    selected,
                );
            }
            SetupState::ApiKey { provider, form } => {
                render_api_key(
                    frame,
                    area,
                    provider,
                    form,
                    &self.theme,
                    self.notice.as_ref(),
                    &self.services.paths.credentials(),
                );
            }
            SetupState::Custom { form } => {
                render_custom(frame, area, form, &self.theme, self.notice.as_ref());
            }
        }
    }
}

/// Map a key event to a detached action based only on the current screen.
fn resolve_key(state: &mut SetupState, entries: &[ProviderEntry], key: KeyEvent) -> Action {
    match state {
        SetupState::ProviderSelect { .. } => match key.code {
            KeyCode::Up | KeyCode::Char('k') => Action::MoveSelection(-1),
            KeyCode::Down | KeyCode::Char('j') => Action::MoveSelection(1),
            KeyCode::Enter => Action::Choose,
            KeyCode::Esc | KeyCode::Char('q') => Action::Quit,
            _ => Action::None,
        },
        SetupState::ApiKey { provider, form } => match key.code {
            KeyCode::Esc => {
                let selected = entries
                    .iter()
                    .position(|entry| entry.name == *provider)
                    .unwrap_or(0);
                Action::Back(selected)
            }
            KeyCode::Tab => Action::SetReveal(!form.reveal),
            KeyCode::Enter => Action::SaveKey(provider.clone(), form.field.text.trim().to_string()),
            _ => {
                form.field.handle_edit_key(key);
                Action::None
            }
        },
        SetupState::Custom { form } => match key.code {
            KeyCode::Esc => Action::Back(entries.len()),
            KeyCode::Tab => Action::FocusDelta(1),
            KeyCode::Up | KeyCode::Char('k') => Action::FocusDelta(-1),
            KeyCode::Down | KeyCode::Char('j') => Action::FocusDelta(1),
            KeyCode::Enter => Action::SubmitCustom,
            _ => {
                let field = match form.focus {
                    0 => &mut form.url,
                    1 => &mut form.model,
                    _ => &mut form.key,
                };
                field.handle_edit_key(key);
                Action::None
            }
        },
    }
}

/// Append a `[providers.custom]` block to the user config file so the next
/// config load knows the endpoint. Existing content (including comments) is
/// preserved.
fn save_custom_provider(
    paths: &AaykraPaths,
    url: &str,
    model: &str,
    key: Option<&str>,
) -> Result<()> {
    let url = normalize_url(url);
    let path = paths.global_config();
    let mut text = if path.exists() {
        std::fs::read_to_string(&path)
            .with_context(|| format!("could not read {}", path.display()))?
    } else {
        String::new()
    };
    let parsed = match toml::from_str::<toml::Value>(&text) {
        Ok(value) => value,
        Err(_) => toml::Value::Table(toml::map::Map::new()),
    };
    if parsed
        .get("providers")
        .and_then(|providers| providers.get("custom"))
        .is_some()
    {
        bail!(
            "providers.custom already exists in {}; edit it directly instead",
            path.display()
        );
    }
    while text.ends_with('\n') {
        text.pop();
    }
    text.push('\n');
    text.push_str(&format!(
        "\n# Added by the Aaykra Code TUI setup\n[providers.custom]\nbase_url = {:?}\ndefault_model = {:?}\nrequires_key = false\n",
        url, model
    ));
    crate::paths::atomic_write(&path, text.as_bytes())?;
    crate::paths::secure_file_permissions(&path)?;
    if let Some(key) = key
        && !key.is_empty()
    {
        Credentials::set(paths, "custom", key)?;
    }
    Ok(())
}

fn normalize_url(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = area.width.saturating_sub(4).min(width).max(40);
    let height = area.height.saturating_sub(2).min(height).max(8);
    Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    )
}

fn shell(area: Rect, title: &str, theme: &Theme) -> (Block<'static>, Rect) {
    let block = Block::bordered()
        .title_top(Line::from(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )))
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.panel_bg));
    let inner = block.inner(area);
    (block, inner)
}

fn render_provider_select(
    frame: &mut Frame,
    area: Rect,
    entries: &[ProviderEntry],
    theme: &Theme,
    notice: Option<&(String, bool, Instant)>,

    selected: usize,
) {
    let rect = centered(area, 88, 30);
    frame.render_widget(Clear, rect);
    let (block, inner) = shell(rect, "Aaykra Code setup — choose a provider", theme);
    frame.render_widget(block, rect);

    let mut rows: Vec<Line<'static>> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let active = index == selected;
        let style = if active {
            Style::default()
                .fg(theme.panel_bg)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text)
        };
        let status_style = if active {
            style
        } else if entry.has_key {
            Style::default().fg(theme.success)
        } else if entry.requires_key {
            Style::default().fg(theme.warning)
        } else {
            Style::default().fg(theme.muted)
        };
        rows.push(Line::from(vec![
            Span::styled(
                format!(" {:>2}  ", index + 1),
                if active {
                    style
                } else {
                    Style::default().fg(theme.muted)
                },
            ),
            Span::styled(format!("{:<12}", entry.name), style),
            Span::styled(entry.status(), status_style),
            Span::styled(" ", style),
        ]));
    }
    let custom_index = entries.len();
    let custom_active = selected == custom_index;
    let custom_style = if custom_active {
        Style::default()
            .fg(theme.panel_bg)
            .bg(theme.accent_alt)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.accent_alt)
    };
    rows.push(Line::from(vec![
        Span::styled(
            format!(" {:>2}  ", custom_index + 1),
            if custom_active {
                custom_style
            } else {
                Style::default().fg(theme.muted)
            },
        ),
        Span::styled("custom endpoint", custom_style),
        Span::styled(
            "  OpenAI-compatible · BYO URL/key",
            if custom_active {
                custom_style
            } else {
                Style::default().fg(theme.muted)
            },
        ),
    ]));

    let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).split(inner);
    frame.render_widget(
        Paragraph::new(ratatui::text::Text::from(rows)).style(Style::default().bg(theme.panel_bg)),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "↑/↓ or j/k move · Enter select · Esc quit · Ctrl+V pastes keys",
            Style::default().fg(theme.muted),
        )))
        .style(Style::default().bg(theme.panel_bg)),
        chunks[1],
    );
    render_notice(frame, chunks[1], theme, notice);
}

fn render_notice(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    notice: Option<&(String, bool, Instant)>,
) {
    if let Some((text, failed, _)) = notice.map(|entry| (entry.0.as_str(), entry.1, entry.2)) {
        let color = if failed { theme.error } else { theme.success };
        let line = Line::from(Span::styled(
            format!(" {text} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
        let width = (line.width() as u16).min(area.width.saturating_sub(2));
        let x = area.x + area.width.saturating_sub(width + 2);
        frame.render_widget(
            Paragraph::new(line).style(Style::default().bg(theme.panel_bg)),
            Rect::new(x, area.y, width, 1),
        );
    }
}

fn render_field(
    frame: &mut Frame,
    area: Rect,
    label: &str,
    field: &mut InputState,
    placeholder: &str,
    masked: bool,
    focused: bool,
    theme: &Theme,
) {
    let inner = Rect::new(
        area.x + 1,
        area.y,
        area.width.saturating_sub(3),
        area.height,
    );
    let mut spans = vec![
        Span::styled(
            format!(" {label} "),
            Style::default()
                .fg(if focused {
                    theme.accent_alt
                } else {
                    theme.muted
                })
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
    ];
    if masked && !field.text.is_empty() {
        spans.push(Span::styled(
            "•".repeat(field.text.chars().count()),
            Style::default().fg(theme.text),
        ));
    } else if field.text.is_empty() {
        spans.push(Span::styled(
            placeholder.to_string(),
            Style::default().fg(theme.muted),
        ));
    } else {
        spans.push(Span::styled(
            field.text.clone(),
            Style::default().fg(theme.text),
        ));
    }
    let paragraph = Paragraph::new(Line::from(spans))
        .wrap(Wrap { trim: false })
        .style(Style::default().bg(theme.panel_bg));
    frame.render_widget(paragraph, inner);
    if focused && !field.text.is_empty() {
        let (x, y) = field.visual_cursor(inner.width.saturating_sub(1), inner.height);
        frame.set_cursor_position((inner.x + x, inner.y + y));
    }
}

fn render_api_key(
    frame: &mut Frame,
    area: Rect,
    provider: &str,
    form: &mut ApiKeyForm,
    theme: &Theme,
    notice: Option<&(String, bool, Instant)>,

    cred_path: &std::path::Path,
) {
    let rect = centered(area, 78, 14);
    frame.render_widget(Clear, rect);
    let (block, inner) = shell(rect, &format!("Set an API key for {provider}"), theme);
    frame.render_widget(block, rect);

    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Length(2),
    ])
    .split(inner);

    render_field(
        frame,
        chunks[0],
        "API key",
        &mut form.field,
        "Paste your key (Ctrl+V)…",
        !form.reveal,
        true,
        theme,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            if form.reveal {
                "Showing key — press Tab to mask"
            } else {
                "Key is masked — press Tab to show"
            },
            Style::default().fg(theme.muted),
        )))
        .style(Style::default().bg(theme.panel_bg)),
        chunks[1],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("Saved to {} (permissions 0600)", cred_path.display()),
            Style::default().fg(theme.muted),
        )))
        .style(Style::default().bg(theme.panel_bg)),
        chunks[2],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Enter save · Tab show/hide · Esc back · Ctrl+V paste",
            Style::default().fg(theme.muted),
        )))
        .style(Style::default().bg(theme.panel_bg)),
        chunks[3],
    );
    render_notice(frame, chunks[3], theme, notice);
}

fn render_custom(
    frame: &mut Frame,
    area: Rect,
    form: &mut CustomForm,
    theme: &Theme,
    notice: Option<&(String, bool, Instant)>,
) {
    let rect = centered(area, 80, 16);
    frame.render_widget(Clear, rect);
    let (block, inner) = shell(rect, "Add a custom OpenAI-compatible endpoint", theme);
    frame.render_widget(block, rect);

    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(2),
    ])
    .split(inner);

    render_field(
        frame,
        chunks[0],
        "Base URL",
        &mut form.url,
        "http://localhost:8080/v1",
        false,
        form.focus == 0,
        theme,
    );
    render_field(
        frame,
        chunks[1],
        "Default model",
        &mut form.model,
        "e.g. qwen3-coder",
        false,
        form.focus == 1,
        theme,
    );
    render_field(
        frame,
        chunks[2],
        "API key (optional)",
        &mut form.key,
        "leave empty if the endpoint needs none",
        !form.reveal_key,
        form.focus == 2,
        theme,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "This writes a [providers.custom] section to your user config.",
            Style::default().fg(theme.muted),
        )))
        .style(Style::default().bg(theme.panel_bg)),
        chunks[3],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Tab / ↑↓ move · Enter next/save · Esc back",
            Style::default().fg(theme.muted),
        )))
        .style(Style::default().bg(theme.panel_bg)),
        chunks[4],
    );
    render_notice(frame, chunks[4], theme, notice);
}
