//! Rendering for the interactive interface.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Gauge, Paragraph, Wrap};

use super::Theme;
use super::app::{
    App, Block as TranscriptBlock, Modal, NoticeLevel, PermissionModal, QuestionModal,
};
use crate::types::{PermissionMode, compact_number};

impl App {
    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let input_height = (self.input.line_count() as u16 + 2).clamp(3, 8);
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(area);

        render_header(frame, chunks[0], self);
        if area.width >= 100 {
            let body =
                Layout::horizontal([Constraint::Min(40), Constraint::Length(32)]).split(chunks[1]);
            render_transcript(frame, body[0], self);
            render_sidebar(frame, body[1], self);
        } else {
            render_transcript(frame, chunks[1], self);
        }
        render_input(frame, chunks[2], self);
        render_status(frame, chunks[3], self);
        if self.modal.is_some() {
            render_modal(frame, area, &mut self.modal, &self.theme);
        }
    }
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let spinner = if app.busy() {
        const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
        FRAMES[(app.spinner as usize) % FRAMES.len()]
    } else {
        "●"
    };
    let left = Line::from(vec![
        Span::styled(
            " Aaykra Code ",
            Style::default()
                .fg(Color::Black)
                .bg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}  ", truncate(&app.session_title, 60)),
            Style::default().fg(app.theme.text),
        ),
        Span::styled(
            format!("turn {} ", app.turn),
            Style::default().fg(app.theme.muted),
        ),
    ]);
    let right = Line::from(vec![
        Span::styled(
            if app.running_turn {
                format!("{spinner} working ")
            } else if app.command_running {
                format!("{spinner} command ")
            } else {
                format!("{spinner} ready ")
            },
            Style::default()
                .fg(if app.busy() {
                    app.theme.warning
                } else {
                    app.theme.success
                })
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{}/{} ", app.provider, truncate(&app.model, 34)),
            Style::default().fg(app.theme.muted),
        ),
    ]);
    let right_width = (right.width() as u16).min(area.width / 2);
    let chunks =
        Layout::horizontal([Constraint::Min(10), Constraint::Length(right_width)]).split(area);
    frame.render_widget(
        Paragraph::new(left).style(Style::default().bg(app.theme.panel_bg)),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(right)
            .style(Style::default().bg(app.theme.panel_bg))
            .alignment(ratatui::layout::Alignment::Right),
        chunks[1],
    );
}

fn render_transcript(frame: &mut Frame, area: Rect, app: &mut App) {
    let lines = transcript_lines(app);
    let title = Line::from(vec![
        Span::styled(" Conversation ", Style::default().fg(app.theme.accent)),
        Span::styled(
            format!("{} messages", app.transcript.len()),
            Style::default().fg(app.theme.muted),
        ),
    ]);
    let block = Block::bordered()
        .title_top(title)
        .border_style(Style::default().fg(app.theme.border))
        .style(Style::default().bg(Color::Reset));
    let inner = block.inner(area);
    let paragraph = Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: false });
    let count = paragraph.line_count(inner.width);
    let max_scroll = count
        .saturating_sub(inner.height as usize)
        .min(u16::MAX as usize) as u16;
    app.max_scroll = max_scroll;
    if app.follow {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
    }
    let scroll = if app.follow {
        app.max_scroll
    } else {
        app.scroll
    };
    frame.render_widget(paragraph.scroll((scroll, 0)), area);

    if app.notice.is_some() && area.height > 4 {
        if let Some((notice, _)) = &app.notice {
            let line = Line::from(Span::styled(
                format!(" {notice} "),
                Style::default()
                    .fg(app.theme.panel_bg)
                    .bg(app.theme.accent_alt)
                    .add_modifier(Modifier::BOLD),
            ));
            let width = (line.width() as u16).min(area.width);
            let x = area.x + area.width.saturating_sub(width + 2);
            let y = area.y + area.height.saturating_sub(2);
            frame.render_widget(
                Paragraph::new(line).style(Style::default().bg(app.theme.panel_bg)),
                Rect::new(x, y, width, 1),
            );
        }
    }
}

fn transcript_lines(app: &App) -> Vec<Line<'static>> {
    let theme = &app.theme;
    if app.transcript.is_empty() {
        return welcome_lines(theme);
    }
    let mut out: Vec<Line<'static>> = Vec::new();
    let last_index = app.transcript.len().saturating_sub(1);
    for (index, block) in app.transcript.iter().enumerate() {
        match block {
            TranscriptBlock::User { text, images } => {
                let mut body = markdown_lines(text, theme, Style::default().fg(theme.text));
                if *images > 0 {
                    if body.is_empty() {
                        body.push(Line::default());
                    }
                    body[0].spans.push(Span::styled(
                        format!(
                            "  [{} image{}]",
                            images,
                            if *images == 1 { "" } else { "s" }
                        ),
                        Style::default().fg(theme.accent_alt),
                    ));
                }
                out.extend(prefix_lines(
                    body,
                    Span::styled(
                        "❯ ",
                        Style::default().fg(theme.user).add_modifier(Modifier::BOLD),
                    ),
                ));
            }
            TranscriptBlock::Assistant { text } => {
                let mut body = markdown_lines(text, theme, Style::default().fg(theme.text));
                if body.is_empty() {
                    body.push(Line::default());
                }
                if app.running_turn && index == last_index {
                    body.last_mut()
                        .unwrap()
                        .spans
                        .push(Span::styled("▌", Style::default().fg(theme.accent)));
                }
                out.extend(prefix_lines(
                    body,
                    Span::styled(
                        "⏺ ",
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                ));
            }
            TranscriptBlock::Thinking { text } => {
                let body = markdown_lines(
                    text,
                    theme,
                    Style::default()
                        .fg(theme.muted)
                        .add_modifier(Modifier::ITALIC),
                );
                out.extend(prefix_lines(
                    body,
                    Span::styled("✻ ", Style::default().fg(theme.muted)),
                ));
            }
            TranscriptBlock::Tool {
                name,
                input,
                output,
                error,
                ..
            } => {
                let color = if *error { theme.error } else { theme.success };
                out.push(Line::from(vec![
                    Span::styled("⏺ ", Style::default().fg(color)),
                    Span::styled(
                        format!("{name}"),
                        Style::default()
                            .fg(theme.accent_alt)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" {}", tool_summary(input)),
                        Style::default().fg(theme.muted),
                    ),
                ]));
                if output.is_empty() {
                    if app.running_turn {
                        out.push(Line::from(Span::styled(
                            "  │ running…",
                            Style::default().fg(theme.muted),
                        )));
                    }
                } else {
                    let limit = if app.show_tool_expanded { 80 } else { 6 };
                    let total = output.lines().count();
                    for line in output.lines().take(limit) {
                        out.push(Line::from(vec![
                            Span::styled("  │ ", Style::default().fg(theme.border)),
                            Span::styled(
                                line.to_string(),
                                Style::default().fg(if *error { theme.error } else { theme.muted }),
                            ),
                        ]));
                    }
                    if total > limit {
                        out.push(Line::from(Span::styled(
                            format!("  │ … {} more lines (Ctrl+O)", total - limit),
                            Style::default().fg(theme.accent_alt),
                        )));
                    }
                }
            }
            TranscriptBlock::Notice { level, text } => {
                let (symbol, color) = match level {
                    NoticeLevel::Info => ("·", theme.muted),
                    NoticeLevel::Warn => ("!", theme.warning),
                    NoticeLevel::Error => ("✗", theme.error),
                    NoticeLevel::Success => ("✓", theme.success),
                };
                let body = markdown_lines(
                    text,
                    theme,
                    Style::default().fg(color).add_modifier(Modifier::ITALIC),
                );
                out.extend(prefix_lines(
                    body,
                    Span::styled(format!("{symbol} "), Style::default().fg(color)),
                ));
            }
            TranscriptBlock::Command { text } => {
                let body = markdown_lines(text, theme, Style::default().fg(theme.accent_alt));
                out.extend(prefix_lines(
                    body,
                    Span::styled(
                        "◆ ",
                        Style::default()
                            .fg(theme.accent_alt)
                            .add_modifier(Modifier::BOLD),
                    ),
                ));
            }
        }
        out.push(Line::default());
    }
    out
}

fn welcome_lines(theme: &Theme) -> Vec<Line<'static>> {
    vec![
        Line::default(),
        Line::from(vec![
            Span::styled(
                "  A A Y K R A ",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " C O D E",
                Style::default()
                    .fg(theme.accent_alt)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            "  Native Rust coding agent",
            Style::default().fg(theme.muted),
        )),
        Line::default(),
        Line::from(vec![
            Span::styled("  Ask for a change", Style::default().fg(theme.text)),
            Span::styled(
                " — describe the task, paste an image, or point at files.",
                Style::default().fg(theme.muted),
            ),
        ]),
        Line::default(),
        Line::from(Span::styled("  Try", Style::default().fg(theme.accent))),
        Line::from(vec![
            Span::styled("  /init          ", Style::default().fg(theme.accent_alt)),
            Span::styled(
                "learn the repository and write AAYKRA.md",
                Style::default().fg(theme.text),
            ),
        ]),
        Line::from(vec![
            Span::styled("  /plan          ", Style::default().fg(theme.accent_alt)),
            Span::styled(
                "explore read-only before editing",
                Style::default().fg(theme.text),
            ),
        ]),
        Line::from(vec![
            Span::styled("  /code-review   ", Style::default().fg(theme.accent_alt)),
            Span::styled(
                "review the working-tree changes",
                Style::default().fg(theme.text),
            ),
        ]),
        Line::from(vec![
            Span::styled("  /help          ", Style::default().fg(theme.accent_alt)),
            Span::styled("every command and key", Style::default().fg(theme.text)),
        ]),
        Line::default(),
        Line::from(Span::styled(
            "  Shift+Tab cycles permission mode · Ctrl+V pastes an image · Ctrl+O expands tools",
            Style::default().fg(theme.muted),
        )),
    ]
}

fn markdown_lines(text: &str, theme: &Theme, base: Style) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut in_code = false;
    for raw in text.split('\n') {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            let language = trimmed.trim_start_matches("```").trim();
            if in_code && !language.is_empty() {
                out.push(Line::from(Span::styled(
                    format!("  {language}"),
                    Style::default()
                        .fg(theme.muted)
                        .add_modifier(Modifier::ITALIC),
                )));
            }
            continue;
        }
        if in_code {
            out.push(Line::from(Span::styled(
                format!("  {raw}"),
                Style::default().fg(theme.text).bg(theme.code_bg),
            )));
            continue;
        }
        if trimmed.starts_with('#') {
            let heading = trimmed.trim_start_matches('#').trim();
            out.push(Line::from(Span::styled(
                heading.to_string(),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            let indent = raw.len() - trimmed.len();
            let mut spans = vec![
                Span::raw(" ".repeat(indent)),
                Span::styled("• ", Style::default().fg(theme.accent)),
            ];
            spans.extend(inline_spans(rest, theme, base));
            out.push(Line::from(spans));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("> ") {
            let mut spans = vec![Span::styled("▏ ", Style::default().fg(theme.border))];
            spans.extend(inline_spans(
                rest,
                theme,
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::ITALIC),
            ));
            out.push(Line::from(spans));
            continue;
        }
        if raw.is_empty() {
            out.push(Line::default());
        } else {
            out.push(Line::from(inline_spans(raw, theme, base)));
        }
    }
    while out.last().is_some_and(|line| line.width() == 0) {
        out.pop();
    }
    out
}

fn inline_spans(text: &str, theme: &Theme, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buffer = String::new();
    let mut in_code = false;
    let mut in_bold = false;
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '`' {
            if !buffer.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut buffer),
                    styled(base, in_code, in_bold, theme),
                ));
            }
            in_code = !in_code;
        } else if character == '*' && chars.peek() == Some(&'*') {
            chars.next();
            if !buffer.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut buffer),
                    styled(base, in_code, in_bold, theme),
                ));
            }
            in_bold = !in_bold;
        } else {
            buffer.push(character);
        }
    }
    if !buffer.is_empty() {
        spans.push(Span::styled(buffer, styled(base, in_code, in_bold, theme)));
    }
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
}

fn styled(base: Style, code: bool, bold: bool, theme: &Theme) -> Style {
    let mut style = base;
    if code {
        style = style.fg(theme.accent_alt).bg(theme.code_bg);
    }
    if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    style
}

fn prefix_lines(lines: Vec<Line<'static>>, prefix: Span<'static>) -> Vec<Line<'static>> {
    if lines.is_empty() {
        return vec![Line::from(prefix)];
    }
    let mut out = Vec::with_capacity(lines.len());
    for (index, mut line) in lines.into_iter().enumerate() {
        if index == 0 {
            line.spans.insert(0, prefix.clone());
        } else {
            line.spans.insert(0, Span::raw("  "));
        }
        out.push(line);
    }
    out
}

fn tool_summary(input: &str) -> String {
    let input = input.trim();
    if input.is_empty() || input == "{}" {
        return String::new();
    }
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(input) {
        let mut parts = Vec::new();
        for (key, value) in map.iter().take(3) {
            let value = match value {
                serde_json::Value::String(text) => text.replace('\n', " "),
                other => other.to_string(),
            };
            parts.push(format!("{key}={}", truncate(&value, 52)));
        }
        return parts.join("  ");
    }
    truncate(&input.replace('\n', " "), 120)
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    format!(
        "{}…",
        text.chars().take(max.saturating_sub(1)).collect::<String>()
    )
}

fn render_sidebar(frame: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let chunks = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length(8),
        Constraint::Min(4),
        Constraint::Length(7),
    ])
    .split(area);

    let ratio = if app.context_limit == 0 {
        0.0
    } else {
        (app.context_tokens as f64 / app.context_limit as f64).clamp(0.0, 1.0)
    };
    let gauge = Gauge::default()
        .block(
            Block::bordered()
                .title_top(Line::from(Span::styled(
                    " Context ",
                    Style::default().fg(theme.accent),
                )))
                .border_style(Style::default().fg(theme.border)),
        )
        .gauge_style(
            Style::default()
                .fg(if ratio > 0.85 {
                    theme.error
                } else if ratio > 0.65 {
                    theme.warning
                } else {
                    theme.accent
                })
                .bg(theme.panel_bg),
        )
        .ratio(ratio)
        .label(format!(
            "{} / {} · {:.0}%",
            compact_number(app.context_tokens as u64),
            compact_number(app.context_limit as u64),
            ratio * 100.0
        ));
    frame.render_widget(gauge, chunks[0]);

    let mut session_lines = vec![
        Line::from(vec![
            Span::styled("model   ", Style::default().fg(theme.muted)),
            Span::styled(
                format!("{}/{}", app.provider, app.model),
                Style::default().fg(theme.text),
            ),
        ]),
        Line::from(vec![
            Span::styled("agent   ", Style::default().fg(theme.muted)),
            Span::styled(app.agent.clone(), Style::default().fg(theme.text)),
        ]),
        Line::from(vec![
            Span::styled("mode    ", Style::default().fg(theme.muted)),
            Span::styled(
                app.mode.label().to_string(),
                Style::default()
                    .fg(mode_color(app.mode, theme))
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("tokens  ", Style::default().fg(theme.muted)),
            Span::styled(
                format!(
                    "{} in / {} out",
                    compact_number(app.usage.input_tokens),
                    compact_number(app.usage.output_tokens)
                ),
                Style::default().fg(theme.text),
            ),
        ]),
        Line::from(vec![
            Span::styled("cost    ", Style::default().fg(theme.muted)),
            Span::styled(
                crate::models::format_cost(app.cost),
                Style::default().fg(theme.text),
            ),
        ]),
        Line::from(vec![
            Span::styled("files   ", Style::default().fg(theme.muted)),
            Span::styled(
                format!("{} changed", app.files_changed),
                Style::default().fg(theme.text),
            ),
        ]),
        Line::from(vec![
            Span::styled("tools   ", Style::default().fg(theme.muted)),
            Span::styled(
                format!("{} mcp · {} lsp", app.mcp_count, app.lsp_count),
                Style::default().fg(theme.text),
            ),
        ]),
    ];
    if let Some(summary) = &app.last_turn_summary {
        session_lines.push(Line::from(Span::styled(
            truncate(summary, 34),
            Style::default().fg(theme.muted),
        )));
    }
    frame.render_widget(
        Paragraph::new(Text::from(session_lines))
            .block(
                Block::bordered()
                    .title_top(Line::from(Span::styled(
                        " Session ",
                        Style::default().fg(theme.accent),
                    )))
                    .border_style(Style::default().fg(theme.border)),
            )
            .wrap(Wrap { trim: true }),
        chunks[1],
    );

    let mut todo_lines = Vec::new();
    if app.todos.is_empty() {
        todo_lines.push(Line::from(Span::styled(
            "No todos",
            Style::default().fg(theme.muted),
        )));
    } else {
        for todo in app.todos.iter().take(12) {
            let color = match todo.status {
                crate::types::TodoStatus::Completed => theme.success,
                crate::types::TodoStatus::InProgress => theme.warning,
                crate::types::TodoStatus::Cancelled => theme.muted,
                crate::types::TodoStatus::Pending => theme.text,
            };
            todo_lines.push(Line::from(vec![
                Span::styled(
                    format!("{} ", todo.status.symbol()),
                    Style::default().fg(color),
                ),
                Span::styled(truncate(&todo.content, 30), Style::default().fg(color)),
            ]));
        }
    }
    frame.render_widget(
        Paragraph::new(Text::from(todo_lines))
            .block(
                Block::bordered()
                    .title_top(Line::from(Span::styled(
                        " Todos ",
                        Style::default().fg(theme.accent),
                    )))
                    .border_style(Style::default().fg(theme.border)),
            )
            .wrap(Wrap { trim: true }),
        chunks[2],
    );

    let keys = vec![
        Line::from(Span::styled(
            "Enter      send",
            Style::default().fg(theme.text),
        )),
        Line::from(Span::styled(
            "Shift+Enter newline",
            Style::default().fg(theme.text),
        )),
        Line::from(Span::styled(
            "Shift+Tab  mode",
            Style::default().fg(theme.text),
        )),
        Line::from(Span::styled(
            "Ctrl+V     image",
            Style::default().fg(theme.text),
        )),
        Line::from(Span::styled(
            "Ctrl+O     tools",
            Style::default().fg(theme.text),
        )),
        Line::from(Span::styled(
            "Ctrl+C     cancel",
            Style::default().fg(theme.text),
        )),
    ];
    frame.render_widget(
        Paragraph::new(Text::from(keys)).block(
            Block::bordered()
                .title_top(Line::from(Span::styled(
                    " Keys ",
                    Style::default().fg(theme.accent),
                )))
                .border_style(Style::default().fg(theme.border)),
        ),
        chunks[3],
    );
}

fn mode_color(mode: PermissionMode, theme: &Theme) -> Color {
    match mode {
        PermissionMode::Ask => theme.accent,
        PermissionMode::AcceptEdits => theme.success,
        PermissionMode::Plan => theme.accent_alt,
        PermissionMode::Auto => theme.warning,
        PermissionMode::Bypass => theme.error,
    }
}

fn render_input(frame: &mut Frame, area: Rect, app: &mut App) {
    let theme = &app.theme;
    let mut title = vec![Span::styled(
        " Message ",
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    )];
    if !app.attachments.is_empty() {
        title.push(Span::styled(
            format!(
                "· {} image{} ",
                app.attachments.len(),
                if app.attachments.len() == 1 { "" } else { "s" }
            ),
            Style::default().fg(theme.accent_alt),
        ));
    }
    if !app.queued.is_empty() {
        title.push(Span::styled(
            format!("· {} queued ", app.queued.len()),
            Style::default().fg(theme.warning),
        ));
    }
    if app.busy() {
        title.push(Span::styled(
            if app.command_running {
                "· command running "
            } else {
                "· agent running "
            },
            Style::default().fg(theme.warning),
        ));
    }
    let block = Block::bordered()
        .title_top(Line::from(title))
        .title_bottom(Line::from(Span::styled(
            " Enter send · Shift+Enter newline · Ctrl+V image · // literal slash ",
            Style::default().fg(theme.muted),
        )))
        .border_style(Style::default().fg(if app.busy() {
            theme.warning
        } else {
            theme.border
        }));
    let inner = block.inner(area);
    let (cursor_x, cursor_y) = app.input.visual_cursor(inner.width, inner.height);
    let paragraph = if app.input.text.is_empty() {
        Paragraph::new(Span::styled(
            "Ask anything…",
            Style::default().fg(theme.muted),
        ))
    } else {
        Paragraph::new(app.input.text.clone())
            .style(Style::default().fg(theme.text))
            .scroll((app.input.row_scroll as u16, app.input.col_scroll as u16))
    };
    frame.render_widget(paragraph.block(block), area);
    if !app.busy() || app.modal.is_none() {
        frame.set_cursor_position((inner.x + cursor_x, inner.y + cursor_y));
    }
}

fn render_status(frame: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let percent = if app.context_limit == 0 {
        0.0
    } else {
        app.context_tokens as f64 / app.context_limit as f64 * 100.0
    };
    let mut spans = vec![
        Span::styled(" ", Style::default()),
        Span::styled(
            app.mode.label().to_string(),
            Style::default()
                .fg(mode_color(app.mode, theme))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · ", Style::default().fg(theme.muted)),
        Span::styled(
            format!("{}/{}", app.provider, truncate(&app.model, 28)),
            Style::default().fg(theme.text),
        ),
        Span::styled(" · ctx ", Style::default().fg(theme.muted)),
        Span::styled(
            format!("{percent:.0}%"),
            Style::default().fg(if percent > 85.0 {
                theme.error
            } else if percent > 65.0 {
                theme.warning
            } else {
                theme.text
            }),
        ),
        Span::styled(" · ", Style::default().fg(theme.muted)),
        Span::styled(
            format!(
                "{} in / {} out",
                compact_number(app.usage.input_tokens),
                compact_number(app.usage.output_tokens)
            ),
            Style::default().fg(theme.text),
        ),
        Span::styled(" · ", Style::default().fg(theme.muted)),
        Span::styled(
            crate::models::format_cost(app.cost),
            Style::default().fg(theme.text),
        ),
        Span::styled(" · ", Style::default().fg(theme.muted)),
        Span::styled(
            std::path::Path::new(&app.cwd)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&app.cwd)
                .to_string(),
            Style::default().fg(theme.muted),
        ),
    ];
    if let Some(started) = app.turn_started {
        spans.push(Span::styled(" · ", Style::default().fg(theme.muted)));
        spans.push(Span::styled(
            format!("{:.0}s", started.elapsed().as_secs_f64()),
            Style::default().fg(theme.warning),
        ));
    }
    if !app.queued.is_empty() {
        spans.push(Span::styled(" · ", Style::default().fg(theme.muted)));
        spans.push(Span::styled(
            format!("{} queued", app.queued.len()),
            Style::default().fg(theme.warning),
        ));
    }
    spans.push(Span::styled(" ", Style::default()));
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.panel_bg)),
        area,
    );
}

fn render_modal(frame: &mut Frame, area: Rect, modal: &mut Option<Modal>, theme: &Theme) {
    let Some(modal) = modal.as_mut() else {
        return;
    };
    match modal {
        Modal::Permission(permission) => render_permission(frame, area, permission, theme),
        Modal::Question(question) => render_question(frame, area, question, theme),
    }
}

fn modal_rect(area: Rect, max_width: u16, max_height: u16) -> Rect {
    let width = area.width.saturating_sub(4).min(max_width).max(10);
    let height = area.height.saturating_sub(2).min(max_height).max(6);
    Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    )
}

fn render_permission(
    frame: &mut Frame,
    area: Rect,
    permission: &mut PermissionModal,
    theme: &Theme,
) {
    let rect = modal_rect(area, 104, 36);
    frame.render_widget(Clear, rect);
    let risk_color = match permission.request.risk {
        crate::types::RiskLevel::Low => theme.success,
        crate::types::RiskLevel::Medium => theme.warning,
        crate::types::RiskLevel::High | crate::types::RiskLevel::Critical => theme.error,
    };
    let block = Block::bordered()
        .title_top(Line::from(vec![
            Span::styled(
                " Permission ",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "· {} · {} ",
                    permission.request.tool,
                    permission.risk_label()
                ),
                Style::default().fg(risk_color),
            ),
        ]))
        .border_style(Style::default().fg(risk_color))
        .style(Style::default().bg(theme.panel_bg));
    frame.render_widget(block.clone(), rect);
    let inner = block.inner(rect);
    let chunks = Layout::vertical([
        Constraint::Length(10),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .split(inner);

    let mut details = vec![
        Line::from(vec![
            Span::styled("Target  ", Style::default().fg(theme.muted)),
            Span::styled(
                truncate(&permission.request.target, 110),
                Style::default().fg(theme.text),
            ),
        ]),
        Line::from(vec![
            Span::styled("Reason  ", Style::default().fg(theme.muted)),
            Span::styled(
                permission.request.reason.clone(),
                Style::default().fg(theme.text),
            ),
        ]),
    ];
    if !permission.request.affected_paths.is_empty() {
        let paths = permission
            .request
            .affected_paths
            .iter()
            .take(3)
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        details.push(Line::from(vec![
            Span::styled("Paths   ", Style::default().fg(theme.muted)),
            Span::styled(truncate(&paths, 110), Style::default().fg(theme.text)),
        ]));
    }
    if !permission.request.suggested_rule.is_empty() {
        details.push(Line::from(vec![
            Span::styled("Rule    ", Style::default().fg(theme.muted)),
            Span::styled(
                permission.request.suggested_rule.clone(),
                Style::default().fg(theme.accent_alt),
            ),
        ]));
    }
    details.push(Line::default());
    let mut actions = Vec::new();
    for (index, action) in permission.actions.iter().enumerate() {
        let style = if index == permission.selected {
            Style::default()
                .fg(theme.panel_bg)
                .bg(if matches!(action, super::app::PermissionAction::Deny) {
                    theme.error
                } else {
                    theme.accent
                })
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text)
        };
        actions.push(Span::styled(format!(" {} ", action.label()), style));
        actions.push(Span::raw("  "));
    }
    details.push(Line::from(actions));
    details.push(Line::from(Span::styled(
        "y allow · a allow for session · n deny · Enter choose · PgUp/PgDn diff",
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(
        Paragraph::new(Text::from(details))
            .wrap(Wrap { trim: true })
            .style(Style::default().bg(theme.panel_bg)),
        chunks[0],
    );

    let diff_text = permission
        .request
        .diff
        .clone()
        .filter(|diff| !diff.trim().is_empty())
        .unwrap_or_else(|| "No textual diff preview for this action.".into());
    let diff_lines: Vec<Line> = diff_text
        .lines()
        .map(|line| {
            let style = if line.starts_with('+') {
                Style::default().fg(theme.success)
            } else if line.starts_with('-') {
                Style::default().fg(theme.error)
            } else if line.starts_with("@@") {
                Style::default().fg(theme.accent_alt)
            } else {
                Style::default().fg(theme.muted)
            };
            Line::from(Span::styled(line.to_string(), style))
        })
        .collect();
    let diff_block = Block::bordered()
        .title_top(Line::from(Span::styled(
            " Diff preview ",
            Style::default().fg(theme.muted),
        )))
        .border_style(Style::default().fg(theme.border));
    let diff_inner = diff_block.inner(chunks[1]);
    let max_scroll = diff_lines
        .len()
        .saturating_sub(diff_inner.height as usize)
        .min(u16::MAX as usize) as u16;
    permission.diff_scroll = permission.diff_scroll.min(max_scroll);
    frame.render_widget(
        Paragraph::new(Text::from(diff_lines))
            .block(diff_block)
            .scroll((permission.diff_scroll, 0))
            .style(Style::default().bg(theme.panel_bg)),
        chunks[1],
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(
                "{} · {}",
                permission.risk_label(),
                if permission.request.read_only {
                    "read-only"
                } else {
                    "may modify files"
                }
            ),
            Style::default().fg(risk_color),
        )))
        .style(Style::default().bg(theme.panel_bg)),
        chunks[2],
    );
}

fn render_question(frame: &mut Frame, area: Rect, question: &mut QuestionModal, theme: &Theme) {
    let rect = modal_rect(area, 84, 24);
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .title_top(Line::from(vec![
            Span::styled(
                " Question ",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("· {} ", question.question.header),
                Style::default().fg(theme.accent_alt),
            ),
        ]))
        .border_style(Style::default().fg(theme.accent))
        .style(Style::default().bg(theme.panel_bg));
    frame.render_widget(block.clone(), rect);
    let inner = block.inner(rect);
    let chunks = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(4),
        Constraint::Length(2),
    ])
    .split(inner);

    frame.render_widget(
        Paragraph::new(Span::styled(
            question.question.question.clone(),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ))
        .wrap(Wrap { trim: true })
        .style(Style::default().bg(theme.panel_bg)),
        chunks[0],
    );

    if question.custom_active {
        let inner_area = chunks[1];
        let (cursor_x, cursor_y) = question.custom.visual_cursor(
            inner_area.width.saturating_sub(2),
            inner_area.height.saturating_sub(1),
        );
        let text = if question.custom.text.is_empty() {
            "Type your answer…".to_string()
        } else {
            question.custom.text.clone()
        };
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(if question.custom.text.is_empty() {
                    theme.muted
                } else {
                    theme.text
                }))
                .scroll((question.custom.row_scroll as u16, 0)),
            inner_area,
        );
        if !question.custom.text.is_empty() {
            frame.set_cursor_position((inner_area.x + cursor_x, inner_area.y + cursor_y));
        }
    } else {
        let mut lines = Vec::new();
        for (index, option) in question.question.options.iter().enumerate() {
            let selected = index == question.selected;
            let marker = if question.question.multiple {
                if question.toggled[index] {
                    "[x] "
                } else {
                    "[ ] "
                }
            } else if selected {
                "(•) "
            } else {
                "( ) "
            };
            let style = if selected {
                Style::default()
                    .fg(theme.panel_bg)
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            let mut spans = vec![
                Span::styled(marker.to_string(), style),
                Span::styled(option.label.clone(), style),
            ];
            if !option.description.is_empty() {
                spans.push(Span::styled(
                    format!(" — {}", truncate(&option.description, 70)),
                    if selected {
                        style
                    } else {
                        Style::default().fg(theme.muted)
                    },
                ));
            }
            lines.push(Line::from(spans));
        }
        let other_selected = question.selected == question.other_index();
        lines.push(Line::from(vec![
            Span::styled(
                "✎ ",
                Style::default().fg(if other_selected {
                    theme.accent
                } else {
                    theme.muted
                }),
            ),
            Span::styled(
                "Other… (type a custom answer)",
                if other_selected {
                    Style::default()
                        .fg(theme.panel_bg)
                        .bg(theme.accent_alt)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.muted)
                },
            ),
        ]));
        frame.render_widget(
            Paragraph::new(Text::from(lines))
                .wrap(Wrap { trim: true })
                .style(Style::default().bg(theme.panel_bg)),
            chunks[1],
        );
    }

    let hint = if question.custom_active {
        "Enter submit · Esc back"
    } else if question.question.multiple {
        "Space toggle · Enter confirm · Esc skip"
    } else {
        "Enter choose · Esc skip"
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(theme.muted),
        )))
        .style(Style::default().bg(theme.panel_bg)),
        chunks[2],
    );
}
