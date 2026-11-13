//! Ratatui + crossterm interactive user interface.

mod app;
mod render;

use std::io::{Stdout, stdout};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{mpsc, oneshot};

use crate::bootstrap::Services;
use crate::session::Session;
use crate::types::{
    AgentEvent, ImageAttachment, PermissionDecision, PermissionRequest, UiGateway, UserAnswer,
    UserQuestion,
};
use app::App;

/// An event emitted by the agent or a request that needs a UI decision.
pub(super) enum UiMessage {
    Event(AgentEvent),
    Authorize {
        request: PermissionRequest,
        reply: oneshot::Sender<PermissionDecision>,
    },
    Ask {
        question: UserQuestion,
        reply: oneshot::Sender<Option<UserAnswer>>,
    },
    TurnDone(Result<crate::agent::RunOutcome, String>),
    CommandDone(crate::commands::CommandAction),
}

struct TuiGateway {
    tx: mpsc::UnboundedSender<UiMessage>,
}

#[async_trait::async_trait]
impl UiGateway for TuiGateway {
    async fn authorize(&self, request: PermissionRequest) -> PermissionDecision {
        let (reply, receiver) = oneshot::channel();
        if self
            .tx
            .send(UiMessage::Authorize { request, reply })
            .is_err()
        {
            return PermissionDecision::Deny {
                reason: Some("interface closed".into()),
            };
        }
        receiver.await.unwrap_or(PermissionDecision::Deny {
            reason: Some("interface closed".into()),
        })
    }

    async fn ask(&self, question: UserQuestion) -> Option<UserAnswer> {
        let (reply, receiver) = oneshot::channel();
        if self.tx.send(UiMessage::Ask { question, reply }).is_err() {
            return None;
        }
        receiver.await.ok().flatten()
    }

    fn emit(&self, event: AgentEvent) {
        let _ = self.tx.send(UiMessage::Event(event));
    }
}

pub(super) struct Theme {
    pub accent: ratatui::style::Color,
    pub accent_alt: ratatui::style::Color,
    pub border: ratatui::style::Color,
    pub text: ratatui::style::Color,
    pub muted: ratatui::style::Color,
    pub success: ratatui::style::Color,
    pub warning: ratatui::style::Color,
    pub error: ratatui::style::Color,
    pub panel_bg: ratatui::style::Color,
    pub code_bg: ratatui::style::Color,
    pub user: ratatui::style::Color,
}

impl Theme {
    fn from_name(name: &str) -> Self {
        use ratatui::style::Color;
        match name.trim().to_ascii_lowercase().as_str() {
            "amber" | "warm" => Self {
                accent: Color::Rgb(229, 192, 123),
                accent_alt: Color::Rgb(232, 158, 76),
                border: Color::Rgb(94, 82, 62),
                text: Color::Rgb(220, 215, 205),
                muted: Color::Rgb(128, 120, 108),
                success: Color::Rgb(160, 190, 120),
                warning: Color::Rgb(230, 180, 100),
                error: Color::Rgb(235, 120, 120),
                panel_bg: Color::Rgb(35, 32, 27),
                code_bg: Color::Rgb(48, 44, 36),
                user: Color::Rgb(229, 192, 123),
            },
            "mono" | "gray" | "grey" => Self {
                accent: Color::White,
                accent_alt: Color::Gray,
                border: Color::DarkGray,
                text: Color::Gray,
                muted: Color::DarkGray,
                success: Color::White,
                warning: Color::Gray,
                error: Color::White,
                panel_bg: Color::Reset,
                code_bg: Color::DarkGray,
                user: Color::White,
            },
            _ => Self {
                accent: Color::Rgb(122, 162, 247),
                accent_alt: Color::Rgb(187, 154, 247),
                border: Color::Rgb(59, 66, 97),
                text: Color::Rgb(192, 202, 245),
                muted: Color::Rgb(86, 95, 137),
                success: Color::Rgb(158, 206, 106),
                warning: Color::Rgb(224, 175, 104),
                error: Color::Rgb(247, 118, 142),
                panel_bg: Color::Rgb(30, 32, 48),
                code_bg: Color::Rgb(36, 40, 59),
                user: Color::Rgb(125, 207, 255),
            },
        }
    }
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    mouse: bool,
    keyboard_enhancement: bool,
}

impl TerminalSession {
    fn start(mouse: bool) -> Result<Self> {
        enable_raw_mode().context("could not enable raw mode")?;
        let mut out = stdout();
        execute!(out, EnterAlternateScreen, EnableBracketedPaste)
            .context("could not enter the alternate screen")?;
        if mouse {
            execute!(out, EnableMouseCapture).context("could not enable mouse capture")?;
        }
        let keyboard_enhancement = execute!(
            out,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
            )
        )
        .is_ok();
        let terminal = Terminal::new(CrosstermBackend::new(out))
            .context("could not initialize the terminal")?;
        Ok(Self {
            terminal,
            mouse,
            keyboard_enhancement,
        })
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut out = stdout();
        if self.mouse {
            let _ = execute!(out, DisableMouseCapture);
        }
        if self.keyboard_enhancement {
            let _ = execute!(out, PopKeyboardEnhancementFlags);
        }
        let _ = execute!(out, DisableBracketedPaste, LeaveAlternateScreen, Show);
    }
}

pub async fn run(
    services: Services,
    session: Session,
    initial_prompt: String,
    initial_images: Vec<ImageAttachment>,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<UiMessage>();
    let gateway = Arc::new(TuiGateway { tx: tx.clone() });
    let theme = Theme::from_name(&services.config.ui.theme);
    let show_thinking = services.config.ui.show_thinking;
    let mouse = services.config.ui.mouse;
    let runtime = services.runtime(session, gateway);

    let mut terminal = TerminalSession::start(mouse)?;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();
    spawn_terminal_reader(event_tx);

    let mut app = App::new(runtime.clone(), tx, theme, show_thinking);
    if !initial_prompt.trim().is_empty() || !initial_images.is_empty() {
        app.submit_external(initial_prompt, initial_images);
    }
    let mut ticker = tokio::time::interval(Duration::from_millis(200));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let loop_result = loop {
        tokio::select! {
            Some(event) = event_rx.recv() => app.handle_terminal_event(event),
            Some(message) = rx.recv() => app.handle_ui_message(message).await,
            _ = ticker.tick() => app.tick(),
            else => break Ok(()),
        }
        if let Err(error) = terminal.terminal.draw(|frame| app.render(frame)) {
            break Err(anyhow::anyhow!("could not draw the interface: {error}"));
        }
        if app.should_quit {
            break Ok(());
        }
    };

    if app.running_turn {
        runtime.cancel.cancel();
    }
    let _ = runtime.save_session().await;
    runtime.mcp.shutdown().await;
    drop(terminal);
    loop_result
}

fn spawn_terminal_reader(tx: mpsc::UnboundedSender<Event>) {
    std::thread::spawn(move || {
        loop {
            if tx.is_closed() {
                break;
            }
            match crossterm::event::poll(Duration::from_millis(100)) {
                Ok(true) => match crossterm::event::read() {
                    Ok(event) => {
                        if tx.send(event).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });
}
