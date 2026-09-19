use super::{
    editor::Editor,
    state::{Item, Role, ToolStatus, View},
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
#[cfg(test)]
use yourai_core::prelude::Out;
use yourai_core::prelude::{Level, SessionStatus};

pub(super) const BG: Color = Color::Rgb(21, 23, 28);
pub(super) const PANEL: Color = Color::Rgb(29, 32, 39);
pub(super) const TEXT: Color = Color::Rgb(218, 222, 231);
pub(super) const MUTED: Color = Color::Rgb(132, 141, 158);
pub(super) const ACCENT: Color = Color::Rgb(232, 177, 113);
pub(super) const GREEN: Color = Color::Rgb(137, 199, 151);
pub(super) const RED: Color = Color::Rgb(238, 131, 141);
pub(super) const BORDER: Color = Color::Rgb(62, 69, 82);
pub(super) const BLUE: Color = Color::Rgb(130, 174, 223);

pub struct Metadata {
    pub model: String,
    pub session: String,
    pub cwd: String,
    pub trusted_shell: bool,
    pub yolo: bool,
}
#[derive(Clone, Copy)]
enum FooterAction {
    Help,
    Sidebar,
    Theme,
}
#[derive(Default)]
pub struct Renderer {
    pub selection: super::selection::Selection,
    key: Option<(u64, u16)>,
    lines: Vec<Line<'static>>,
    headers: Vec<(usize, u64)>,
    hits: Vec<(Rect, u64)>,
    footer_hits: Vec<(Rect, FooterAction)>,
    command_hits: Vec<(Rect, super::commands::Command)>,
    command_area: Option<Rect>,
    todo_hit: Option<Rect>,
    todo_area: Option<Rect>,
    transcript: Rect,
    anchor: Option<(u64, usize)>,
    reveal: Option<u64>,
    animation_start: Option<std::time::Instant>,
}
impl Renderer {
    pub fn begin_selection(&mut self, x: u16, y: u16) {
        let point = Position::new(x, y);
        let region = if self.transcript.contains(point) {
            self.transcript
        } else {
            self.selection
                .screen
                .as_ref()
                .map(|b| b.area)
                .unwrap_or_default()
        };
        self.selection.begin(point, region);
    }

    pub fn click(&mut self, view: &mut View, x: u16, y: u16) {
        let point = Position::new(x, y);
        if self.command_area.is_some_and(|r| r.contains(point)) {
            if let Some((_, command)) = self.command_hits.iter().find(|(r, _)| r.contains(point)) {
                view.editor.take();
                view.editor.insert(command.text);
                if command.argument {
                    view.editor.insert(" ");
                }
            }
            return;
        }
        if let Some((_, action)) = self.footer_hits.iter().find(|(r, _)| r.contains(point)) {
            match action {
                FooterAction::Help => view.help = true,
                FooterAction::Sidebar => view.sidebar = !view.sidebar,
                FooterAction::Theme => view.theme = view.theme.next(),
            }
            return;
        }
        if self.todo_hit.is_some_and(|r| r.contains(point)) {
            view.todos_expanded = !view.todos_expanded;
            return;
        }
        if let Some((_, id)) = self.hits.iter().find(|(r, _)| r.contains(point)) {
            self.anchor = Some((*id, y.saturating_sub(self.transcript.y) as usize));
            view.toggle(*id);
        }
    }
    pub fn scroll(&self, view: &mut View, x: u16, y: u16, up: bool) {
        if self
            .todo_area
            .is_some_and(|r| r.contains(Position::new(x, y)))
            && view.todos_expanded
        {
            view.todo_scroll = if up {
                view.todo_scroll.saturating_sub(3)
            } else {
                view.todo_scroll.saturating_add(3)
            };
        } else {
            view.scroll = if up {
                view.scroll.saturating_add(3)
            } else {
                view.scroll.saturating_sub(3)
            };
            if view.scroll == 0 {
                view.follow();
            }
        }
    }
    pub fn follow(&mut self, view: &mut View) {
        self.anchor = None;
        self.reveal = None;
        view.follow();
    }
    pub fn reveal(&mut self, id: Option<u64>) {
        self.reveal = id;
    }

    pub fn draw(
        &mut self,
        f: &mut Frame<'_>,
        v: &mut View,
        m: &Metadata,
        status: &SessionStatus,
        queued: usize,
        compact: bool,
    ) {
        let area = f.area();
        self.hits.clear();
        self.footer_hits.clear();
        self.command_hits.clear();
        self.command_area = None;
        self.todo_hit = None;
        self.todo_area = None;
        f.render_widget(
            Block::default().style(Style::default().bg(BG).fg(TEXT)),
            area,
        );
        if area.width < 30 || area.height < 10 {
            f.render_widget(
                Paragraph::new("YourAI\nTerminal too small (minimum 30 x 10).\nCtrl-Q quits.")
                    .style(Style::default().fg(ACCENT)),
                area,
            );
            self.selection.clear();
            self.selection.screen = None;
            v.theme.apply(f.buffer_mut());
            return;
        }
        let cols = if v.sidebar && area.width >= 110 {
            Layout::horizontal([Constraint::Min(50), Constraint::Length(29)]).split(area)
        } else {
            Layout::horizontal([Constraint::Percentage(100)]).split(area)
        };
        let width = cols[0].width.saturating_sub(4).max(2) as usize;
        let (editor_lines, _, _) = v.editor.layout(width);
        let input_height = (editor_lines.len() as u16 + 2)
            .clamp(3, 8)
            .min(area.height.saturating_sub(6));
        let ask_height = if v.asks.is_empty() {
            0
        } else {
            (area.height / 2).clamp(4, 13)
        };
        let ask_height = ask_height.min(area.height.saturating_sub(input_height + 6));
        let todo_height = if v.todos.is_empty() {
            0
        } else if v.todos_expanded {
            (v.todos.len() as u16 + 1).min(6).min(area.height / 4)
        } else {
            1
        };
        let ask_height = ask_height.min(area.height.saturating_sub(input_height + todo_height + 6));
        let busy = v.active || compact || !v.asks.is_empty();
        let rows = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(todo_height),
            Constraint::Length(ask_height),
            Constraint::Length(0),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(cols[0]);
        let state: &str = if !v.asks.is_empty() {
            "approval / reply"
        } else if compact {
            "compacting"
        } else {
            match status {
                SessionStatus::Idle => "ready",
                SessionStatus::Running { .. } => "working",
                SessionStatus::Compacting => "compacting",
                SessionStatus::Closing => "stopping",
                SessionStatus::Closed => "closed",
                _ => "working",
            }
        };
        let elapsed = v
            .since
            .map(|t| format!(" · {}s", t.elapsed().as_secs()))
            .unwrap_or_default();
        let transcript = rows[0];
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(BORDER))
            .title(Line::from(Span::styled(
                if v.scroll > 0 {
                    format!(" Conversation · {} new · Ctrl-End follows ", v.unseen)
                } else {
                    " Conversation ".into()
                },
                Style::default().fg(MUTED),
            )));
        let inner = block.inner(transcript);
        f.render_widget(block, transcript);
        self.transcript = inner;
        let key = (v.revision, inner.width);
        if self.key != Some(key) {
            let old = self.lines.len();
            (self.lines, self.headers) = timeline(v, inner.width.saturating_sub(2) as usize);
            if self.key.is_some_and(|k| k.1 == key.1) && v.scroll > 0 {
                v.scroll = v
                    .scroll
                    .saturating_add(self.lines.len().saturating_sub(old));
            }
            self.key = Some(key);
        }
        let height = inner.height as usize;
        if let Some((id, offset)) = self.anchor.take() {
            if let Some((line, _)) = self.headers.iter().find(|(_, key)| *key == id) {
                v.scroll = self
                    .lines
                    .len()
                    .saturating_sub(line.saturating_sub(offset) + height);
            }
        }
        if let Some(id) = self.reveal.take() {
            if let Some((line, _)) = self.headers.iter().find(|(_, key)| *key == id) {
                let end = self.lines.len().saturating_sub(v.scroll);
                if *line < end.saturating_sub(height) || *line >= end {
                    v.scroll = self.lines.len().saturating_sub(*line + height);
                }
            }
        }
        v.scroll = v.scroll.min(self.lines.len().saturating_sub(height));
        let end = self.lines.len().saturating_sub(v.scroll);
        let start = end.saturating_sub(height);
        f.render_widget(Paragraph::new(self.lines[start..end].to_vec()), inner);
        for (line, id) in &self.headers {
            if *line >= start && *line < end {
                self.hits.push((
                    Rect::new(inner.x, inner.y + (*line - start) as u16, inner.width, 1),
                    *id,
                ));
            }
        }
        if todo_height > 0 {
            draw_todos(f, rows[1], v);
            self.todo_area = Some(rows[1]);
            self.todo_hit = Some(Rect::new(rows[1].x, rows[1].y, rows[1].width, 1));
        }
        if v.items.is_empty() {
            welcome(f, inner);
        }
        let mode = if v.active {
            "Message · steer current turn"
        } else {
            "Message"
        };
        draw_editor(f, &v.editor, rows[4], mode, v.asks.is_empty(), ACCENT);
        if let Some(ask) = v.asks.front() {
            let title = if ask.permission() {
                " permission · y allow once / n deny · Enter confirms "
            } else {
                " Reply · plain text; /json for structured replies "
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .style(Style::default().bg(PANEL))
                .title(title);
            let inner = block.inner(rows[2]);
            f.render_widget(block, rows[2]);
            let parts = Layout::vertical([
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(inner);
            let mut lines = Vec::new();
            if ask.permission() {
                let name = ask.payload["tool_name"].as_str().unwrap_or("tool");
                lines.push(Line::from(Span::styled(
                    format!("  Allow {name}?"),
                    Style::default().fg(ACCENT).bold(),
                )));
            }
            for line in ask.details.lines() {
                lines.extend(wrap(
                    line,
                    Style::default().fg(TEXT),
                    parts[0].width as usize,
                    " ",
                ));
            }
            let offset = ask
                .scroll
                .min(lines.len().saturating_sub(parts[0].height as usize));
            f.render_widget(
                Paragraph::new(
                    lines
                        .into_iter()
                        .skip(offset)
                        .take(parts[0].height as usize)
                        .collect::<Vec<_>>(),
                ),
                parts[0],
            );
            if let Some(error) = &ask.error {
                f.render_widget(
                    Paragraph::new(error.as_str()).style(Style::default().fg(RED)),
                    parts[1],
                );
            } else {
                f.render_widget(
                    Paragraph::new("Alt-PgUp/PgDn details · Esc cancels turn")
                        .style(Style::default().fg(MUTED)),
                    parts[1],
                );
            }
            let (reply, row, col) = ask.editor.layout(parts[2].width as usize);
            let line = reply.get(row).cloned().unwrap_or_default();
            f.render_widget(
                Paragraph::new(line).style(Style::default().fg(TEXT)),
                parts[2],
            );
            if parts[2].width > 0 && parts[2].height > 0 {
                f.set_cursor_position((
                    parts[2].x + col.min(parts[2].width.saturating_sub(1) as usize) as u16,
                    parts[2].y,
                ));
            }
        }
        let footer = rows[5];
        let controls = [
            ("F1 /", FooterAction::Help),
            ("^B", FooterAction::Sidebar),
            (v.theme.name(), FooterAction::Theme),
        ];
        let control_width: u16 = controls
            .iter()
            .map(|(text, _)| text.width() as u16 + 3)
            .sum();
        let controls_start = footer.right().saturating_sub(control_width);
        let text_width = controls_start.saturating_sub(footer.x + 2) as usize;
        let status = if busy {
            format!(
                "{}{}{}",
                activity(v, compact),
                elapsed,
                if m.yolo { " · YOLO" } else { "" }
            )
        } else {
            format!(
                "{} · {state}{}",
                m.model,
                if m.yolo { " · YOLO" } else { "" }
            )
        };
        let color = if busy && v.asks.is_empty() {
            let seconds = self
                .animation_start
                .get_or_insert_with(std::time::Instant::now)
                .elapsed()
                .as_secs_f32();
            pulse_color(seconds, v.theme)
        } else {
            self.animation_start = None;
            ACCENT
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(if busy { "● " } else { "  " }, Style::default().fg(color)),
                Span::styled(
                    elide(&status, text_width),
                    Style::default().fg(if busy { color } else { TEXT }),
                ),
            ])),
            footer,
        );
        let mut x = controls_start;
        for (text, action) in controls {
            let width = text.width() as u16 + 2;
            let rect = Rect::new(x, footer.y, width, 1);
            f.render_widget(
                Paragraph::new(format!(" {text} ")).style(Style::default().fg(ACCENT).bg(PANEL)),
                rect,
            );
            self.footer_hits.push((rect, action));
            x += width + 1;
        }
        v.commands
            .sync(&v.editor.text, v.asks.is_empty() && !v.help);
        let commands = v.commands.items();
        if !commands.is_empty() {
            let height = (commands.len() as u16 + 2).min(rows[4].y.saturating_sub(area.y));
            let rect = Rect::new(
                cols[0].x,
                rows[4].y.saturating_sub(height),
                cols[0].width.min(62),
                height,
            );
            self.command_area = Some(rect);
            let visible = height.saturating_sub(2) as usize;
            let start = v
                .commands
                .selected
                .saturating_sub(visible.saturating_sub(1));
            for (row, command) in commands.iter().skip(start).take(visible).enumerate() {
                self.command_hits.push((
                    Rect::new(
                        rect.x + 1,
                        rect.y + 1 + row as u16,
                        rect.width.saturating_sub(2),
                        1,
                    ),
                    *command,
                ));
            }
            let lines = commands
                .iter()
                .enumerate()
                .skip(start)
                .take(visible)
                .map(|(i, c)| {
                    Line::from(Span::styled(
                        format!(
                            " {} {:<17} {}",
                            if i == v.commands.selected { "›" } else { " " },
                            c.text,
                            c.description
                        ),
                        Style::default()
                            .fg(if i == v.commands.selected {
                                ACCENT
                            } else {
                                TEXT
                            })
                            .bg(if i == v.commands.selected { BG } else { PANEL }),
                    ))
                })
                .collect::<Vec<_>>();
            f.render_widget(Clear, rect);
            f.render_widget(
                Paragraph::new(lines)
                    .style(Style::default().bg(PANEL))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(BORDER))
                            .title(" Commands · ↑↓ · Enter · Tab "),
                    ),
                rect,
            );
        }
        if cols.len() > 1 {
            sidebar(f, cols[1], v, m, queued);
        }
        if v.help {
            help(f, area);
        }
        v.theme.apply(f.buffer_mut());
        self.selection
            .render(f.buffer_mut(), v.theme.color(BG), v.theme.color(BLUE));
        if let Some((message, since)) = &v.toast {
            if since.elapsed().as_secs() < 3 {
                let width = (message.width() as u16 + 6).min(area.width.saturating_sub(4));
                let rect = Rect::new(
                    area.x + (area.width - width) / 2,
                    rows[4].y.saturating_sub(4),
                    width,
                    3,
                );
                f.render_widget(Clear, rect);
                f.render_widget(
                    Paragraph::new(elide(message, width.saturating_sub(4) as usize))
                        .alignment(Alignment::Center)
                        .style(
                            Style::default()
                                .fg(v.theme.color(ACCENT))
                                .bg(v.theme.color(PANEL)),
                        )
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(v.theme.color(ACCENT))),
                        ),
                    rect,
                );
            }
        }
    }
}
// Time-based brightness keeps the indicator smooth without changing its width.
fn pulse_color(seconds: f32, theme: super::theme::Theme) -> Color {
    let intensity = (1.0 - (seconds * std::f32::consts::PI).cos()) * 0.5;
    let Color::Rgb(lr, lg, lb) = theme.color(MUTED) else {
        return ACCENT;
    };
    let Color::Rgb(hr, hg, hb) = theme.color(ACCENT) else {
        return ACCENT;
    };
    let blend = |low: u8, high: u8| (low as f32 + (high as f32 - low as f32) * intensity) as u8;
    Color::Rgb(blend(lr, hr), blend(lg, hg), blend(lb, hb))
}
fn activity(v: &View, compact: bool) -> String {
    if let Some(ask) = v.asks.front() {
        return if ask.permission() {
            "Waiting for approval"
        } else {
            "Waiting for your reply"
        }
        .into();
    }
    if compact {
        return "Compacting context".into();
    }
    if let Some(Item::Tool(tool)) = v
        .items
        .iter()
        .rev()
        .find(|item| matches!(item, Item::Tool(t) if t.status == ToolStatus::Running))
    {
        return format!("Running {}", tool.name);
    }
    v.model_activity().into()
}
fn welcome(f: &mut Frame<'_>, area: Rect) {
    let wide = area.width >= 56 && area.height >= 9;
    let lines = if wide {
        vec![
            Line::default(),
            Line::from(vec![
                Span::styled("    .--------.     ", Style::default().fg(ACCENT)),
                Span::styled("YourAI", Style::default().fg(TEXT).bold()),
            ]),
            Line::from(vec![
                Span::styled("    |  >  _  |     ", Style::default().fg(ACCENT)),
                Span::styled("Your coding companion.", Style::default().fg(MUTED)),
            ]),
            Line::from(Span::styled("    |  [__]  |", Style::default().fg(ACCENT))),
            Line::from(vec![
                Span::styled("    '---..---'     ", Style::default().fg(ACCENT)),
                Span::styled("What would you like to build?", Style::default().fg(TEXT)),
            ]),
            Line::from(Span::styled(r"       /__\", Style::default().fg(ACCENT))),
            Line::default(),
            Line::from(Span::styled(
                "    Read code. Make changes. Run tests.",
                Style::default().fg(MUTED),
            )),
            Line::from(Span::styled(
                "    /help for commands and shortcuts",
                Style::default().fg(MUTED),
            )),
        ]
    } else {
        vec![
            Line::from(vec![
                Span::styled("  [>_]  ", Style::default().fg(ACCENT)),
                Span::styled("YourAI", Style::default().fg(TEXT).bold()),
            ]),
            Line::from(Span::styled(
                "  What shall we build?",
                Style::default().fg(MUTED),
            )),
            Line::from(Span::styled(
                "  /help for commands",
                Style::default().fg(MUTED),
            )),
        ]
    };
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_editor(f: &mut Frame<'_>, e: &Editor, area: Rect, title: &str, focus: bool, color: Color) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focus { color } else { BORDER }))
        .style(Style::default().bg(PANEL))
        .title(format!(" {title} "));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let (lines, row, col) = e.layout(inner.width as usize);
    let top = row.saturating_sub(inner.height.saturating_sub(1) as usize);
    f.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(top)
                .take(inner.height as usize)
                .map(Line::from)
                .collect::<Vec<_>>(),
        ),
        inner,
    );
    if focus && inner.width > 0 && inner.height > 0 {
        f.set_cursor_position((
            inner.x + col.min(inner.width.saturating_sub(1) as usize) as u16,
            inner.y + (row - top) as u16,
        ));
    }
}
fn sidebar(f: &mut Frame<'_>, area: Rect, v: &View, m: &Metadata, queued: usize) {
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(PANEL));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let done = v.todos.iter().filter(|t| t.completed).count();
    let mut lines = vec![
        label("SESSION OVERVIEW", ACCENT),
        label("Ctrl-B to hide", MUTED),
        Line::default(),
        label("TASKS", MUTED),
    ];
    if v.todos.is_empty() {
        lines.push(label("No tasks yet", MUTED));
    } else {
        let filled = done * 16 / v.todos.len();
        lines.push(label(
            &format!("{}{}", "━".repeat(filled), "─".repeat(16 - filled)),
            GREEN,
        ));
        lines.push(label(&format!("{done}/{} completed", v.todos.len()), TEXT));
        lines.push(label(&format!("{} remaining", v.todos.len() - done), MUTED));
    }
    if queued > 0 {
        lines.push(label(&format!("{queued} queued inputs"), ACCENT));
    }
    lines.push(Line::default());
    lines.push(label("CONTEXT · ESTIMATED", MUTED));
    if let Some(usage) = &v.context_usage {
        let used = usage.estimated_tokens;
        let pressure = usage
            .input_budget
            .map(|b| used as f64 / b.max(1) as f64)
            .unwrap_or(0.0);
        let color = if pressure >= 1.0 {
            RED
        } else if pressure >= 0.85 {
            ACCENT
        } else {
            GREEN
        };
        if let Some(window) = usage.context_window.filter(|w| *w > 0) {
            let ratio = used as f64 / window as f64;
            lines.push(label(
                &format!(
                    "{} / {} · {:.1}%",
                    tokens(used),
                    tokens(window),
                    ratio * 100.0
                ),
                TEXT,
            ));
            let filled = (ratio.clamp(0.0, 1.0) * 18.0).round() as usize;
            lines.push(label(
                &format!("{}{}", "━".repeat(filled), "─".repeat(18 - filled)),
                color,
            ));
        } else {
            lines.push(label(
                &format!("{} used · limit unknown", tokens(used)),
                TEXT,
            ));
        }
        if let Some(budget) = usage.input_budget {
            lines.push(label(&format!("Input budget {}", tokens(budget)), MUTED));
            lines.push(label(
                &format!("{} input remaining", tokens(budget.saturating_sub(used))),
                color,
            ));
        }
        lines.push(label(
            &format!("Output reserve {}", tokens(usage.output_reserve)),
            MUTED,
        ));
    } else {
        lines.push(label("Estimate unavailable", MUTED));
    }
    let metrics = &v.model_metrics.requests;
    if metrics.cooldown_seconds > 0 {
        lines.push(label(
            &format!("Rate limit · wait {}s", metrics.cooldown_seconds),
            ACCENT,
        ));
    }
    if metrics.journal_errors > 0 {
        lines.push(label("Request log write failed", RED));
    }
    lines.extend([
        Line::default(),
        label("REQUESTS · THIS RUN", MUTED),
        label(
            &format!(
                "{} calls · {}/60s",
                v.model_metrics.calls, metrics.attempts_last_minute
            ),
            TEXT,
        ),
        label(
            &format!(
                "{} failed · {} HTTP 429",
                metrics.failed, metrics.rate_limited
            ),
            if metrics.rate_limited > 0 { RED } else { MUTED },
        ),
        label(
            &format!(
                "{} active · {} cancelled",
                metrics.active, metrics.cancelled
            ),
            MUTED,
        ),
        label(
            &metrics
                .last_output_tokens_per_second
                .map(|n| format!("{n:.1} tok/s · last E2E"))
                .unwrap_or_else(|| "tok/s · awaiting usage".into()),
            TEXT,
        ),
        label(
            &metrics
                .cache_hit_percent()
                .map(|n| format!("Cache hit {n:.1}% · known"))
                .unwrap_or_else(|| "Cache hit · not reported".into()),
            TEXT,
        ),
    ]);
    if metrics.cache_reported_responses > 0 {
        lines.push(label(
            &format!(
                "Cache samples {}/{}",
                metrics.cache_reported_responses, metrics.completed
            ),
            MUTED,
        ));
    }
    lines.extend([
        Line::default(),
        label("SESSION TOKENS · TOTAL", MUTED),
        label(
            &format!("{} recorded responses", v.recorded_responses),
            MUTED,
        ),
        label(
            &format!(
                "{} in / {} out",
                tokens(v.usage.input_tokens),
                tokens(v.usage.output_tokens)
            ),
            TEXT,
        ),
        label(&format!("{} total", tokens(v.usage.total_tokens)), MUTED),
        Line::default(),
        label("PERMISSIONS", MUTED),
        label(
            if m.yolo {
                "YOLO · skip approvals"
            } else if m.trusted_shell {
                "Trusted local execution"
            } else {
                "Ask before commands"
            },
            TEXT,
        ),
    ]);
    if inner.height as usize >= lines.len() + 7 {
        lines.extend([
            Line::default(),
            label("WORKSPACE", MUTED),
            label(&elide(&m.cwd, inner.width.saturating_sub(3) as usize), TEXT),
        ]);
    }
    if inner.height as usize >= lines.len() + 4 {
        lines.extend([
            Line::default(),
            label("SESSION ID", MUTED),
            label(&m.session.chars().take(20).collect::<String>(), TEXT),
        ]);
    }
    if v.scroll > 0 {
        lines.push(label(&format!("{} new updates", v.unseen), ACCENT));
    }
    f.render_widget(Paragraph::new(lines), inner);
}
fn tokens(value: u64) -> String {
    format!("{:.1}K", value as f64 / 1000.0)
}
fn elide(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.into();
    }
    let mut result = String::new();
    for g in text.graphemes(true) {
        if result.width() + g.width() + 1 > width {
            break;
        }
        result.push_str(g);
    }
    result.push('…');
    result
}
fn label(s: &str, color: Color) -> Line<'static> {
    Line::from(Span::styled(format!("  {s}"), Style::default().fg(color)))
}
fn help(f: &mut Frame<'_>, area: Rect) {
    let width = area.width.saturating_sub(4).min(78);
    let height = area.height.saturating_sub(2).min(30);
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    f.render_widget(Clear, rect);
    f.render_widget(Paragraph::new("Enter          Send / steer; confirm reply\nCtrl-J/Alt-Enter  Newline (paste preserves newlines)\nArrows/Home/End  Edit text; Delete/Backspace\nCtrl-P / Ctrl-N  Previous / next prompt, preserve draft\nPgUp / PgDn     Scroll conversation\nCtrl-End        Follow newest output\nClick header    Expand / collapse one block\n/               Command menu · Up/Down · Tab/Enter\nF6 / Shift-F6   Select next / previous block\nCtrl-O          Toggle selected / latest block\nCtrl-R          Toggle selected / latest thinking\nCtrl-T          Expand / collapse TODO\nCtrl-B          Toggle sidebar (wide terminals)\nCtrl-Y          Cycle color theme\nMouse drag      Release to copy automatically\nEsc             Clear selection first\nAlt-PgUp/PgDn   Scroll approval details\nEsc / Ctrl-C    Cancel current execution\nCtrl-Q          Quit\n\n/queue TEXT     Schedule a follow-up turn\n/compact        Compact idle conversation\n/clear          Clear screen only (history unchanged)\n/theme NAME     dark / light / nord / dracula\n/help           This help · Esc closes\n\nApprovals: y/n + Enter. No automatic or permanent grants.").style(Style::default().bg(PANEL).fg(TEXT)).block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(ACCENT)).title(" Help ")),rect);
}
fn draw_todos(f: &mut Frame<'_>, area: Rect, view: &mut View) {
    let done = view.todos.iter().filter(|t| t.completed).count();
    let mut lines = vec![Line::from(Span::styled(
        format!(
            "  {} TODO  {done}/{} · Ctrl-T · scroll",
            if view.todos_expanded { "▾" } else { "▸" },
            view.todos.len()
        ),
        Style::default().fg(ACCENT),
    ))];
    if view.todos_expanded {
        // Pending work stays visible; completed items retain their real backend state.
        let mut todos: Vec<_> = view.todos.iter().collect();
        todos.sort_by_key(|t| t.completed);
        let available = area.height.saturating_sub(1) as usize;
        view.todo_scroll = view.todo_scroll.min(todos.len().saturating_sub(available));
        for todo in todos.iter().skip(view.todo_scroll).take(available) {
            lines.push(label(
                &format!(
                    "{} {}",
                    if todo.completed { "✓" } else { "○" },
                    todo.text.replace('\n', " ")
                ),
                if todo.completed { MUTED } else { TEXT },
            ));
        }
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(PANEL)),
        area,
    );
}
fn timeline(v: &View, width: usize) -> (Vec<Line<'static>>, Vec<(usize, u64)>) {
    let mut lines = Vec::new();
    let mut headers = Vec::new();
    for (index, item) in v.items.iter().enumerate() {
        let id = v.item_id(index);
        let expanded = v.expanded.contains(&id);
        let arrow = if expanded { "▾" } else { "▸" };
        let selected = v.selected == Some(id);
        match item {
            Item::Text {
                role: Role::Thinking,
                text,
            } => {
                headers.push((lines.len(), id));
                let style = Style::default().fg(if selected { ACCENT } else { MUTED });
                lines.push(Line::from(Span::styled(
                    format!("  {arrow} 思考 · {} 字", text.chars().count()),
                    style,
                )));
                if expanded {
                    lines.extend(super::markdown::render(text, width));
                }
            }
            Item::Text {
                role: Role::User,
                text,
            } => {
                for line in text.lines() {
                    lines.extend(wrap(
                        line,
                        Style::default().fg(TEXT).bg(PANEL),
                        width,
                        "  ▎ ",
                    ));
                }
            }
            Item::Text { text, .. } => lines.extend(super::markdown::render(text, width)),
            Item::Notice { level, text } => {
                let color = match level {
                    Level::Error => RED,
                    Level::Warning => ACCENT,
                    _ => MUTED,
                };
                for line in text.lines() {
                    lines.extend(wrap(line, Style::default().fg(color), width, "  · "));
                }
            }
            Item::Tool(t) => {
                headers.push((lines.len(), id));
                let (icon, color) = match t.status {
                    ToolStatus::Running => ("●", ACCENT),
                    ToolStatus::Done => ("✓", GREEN),
                    ToolStatus::Failed => ("!", RED),
                    ToolStatus::Interrupted => ("■", MUTED),
                };
                let summary = t.summary.lines().next().unwrap_or("");
                let seconds = t.seconds.map(|n| format!(" · {n}s")).unwrap_or_default();
                let status = match t.status {
                    ToolStatus::Failed => " · failed",
                    ToolStatus::Interrupted => " · stopped",
                    _ => "",
                };
                // A folded block is exactly one row; raw input/output stays hidden.
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {arrow} {icon} {}{seconds}{status}  ", t.name),
                        Style::default().fg(if selected { ACCENT } else { color }),
                    ),
                    Span::styled(summary.to_owned(), Style::default().fg(MUTED)),
                ]));
                if expanded {
                    for line in t.input.lines() {
                        lines.extend(wrap(line, Style::default().fg(MUTED), width, "  │ "));
                    }
                    let body = if t.status == ToolStatus::Running {
                        &t.progress
                    } else {
                        &t.output
                    };
                    for line in body.lines() {
                        let color = if t.name == "edit" && line.starts_with('+') {
                            GREEN
                        } else if t.name == "edit" && line.starts_with('-') {
                            RED
                        } else {
                            TEXT
                        };
                        lines.extend(wrap(line, Style::default().fg(color), width, "  │ "));
                    }
                }
            }
        }
        lines.push(Line::default());
    }
    (lines, headers)
}
fn wrap(text: &str, style: Style, width: usize, prefix: &str) -> Vec<Line<'static>> {
    let available = width.saturating_sub(prefix.width()).max(2);
    let mut result = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    for g in text.graphemes(true) {
        if used + g.width() > available && !line.is_empty() {
            result.push(Line::from(Span::styled(format!("{prefix}{line}"), style)));
            line.clear();
            used = 0;
        }
        line.push_str(g);
        used += g.width();
    }
    result.push(Line::from(Span::styled(format!("{prefix}{line}"), style)));
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use serde_json::json;
    #[test]
    fn palettes_context_and_toolbar_are_consistent() {
        use super::super::theme::Theme;
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
        let mut renderer = Renderer::default();
        let mut v = View::default();
        v.user("Review parser", false);
        v.event(Out::Message {
            text: "## Result\n**Done**, with `code`.".into(),
        });
        v.context_usage = Some(yourai_harness::runtime::ContextUsage {
            estimated_tokens: 32_000,
            context_window: Some(128_000),
            input_budget: Some(119_000),
            output_reserve: 8_000,
        });
        v.model_metrics.calls = 8;
        v.model_metrics.requests.failed = 3;
        v.model_metrics.requests.rate_limited = 3;
        v.model_metrics.requests.completed = 5;
        v.model_metrics.requests.last_output_tokens_per_second = Some(47.5);
        v.model_metrics.requests.cache_read_tokens = 80;
        v.model_metrics.requests.cache_known_input_tokens = 100;
        v.model_metrics.requests.cache_reported_responses = 1;
        v.usage.input_tokens = 125_000;
        v.usage.output_tokens = 6_000;
        v.usage.total_tokens = 131_000;
        let m = Metadata {
            model: "coding-model".into(),
            session: "session-123".into(),
            cwd: "~/projects/yourai".into(),
            trusted_shell: false,
            yolo: true,
        };
        for theme in [Theme::Dark, Theme::Light, Theme::Nord, Theme::Dracula] {
            v.theme = theme;
            terminal
                .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
                .unwrap();
            assert_eq!(terminal.backend().buffer()[(0, 0)].bg, theme.color(BG));
            let text = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(text.contains("32.0K / 128.0K · 25.0%"));
            assert!(text.contains("125.0K in / 6.0K out"));
            assert!(text.contains("8 calls"));
            assert!(text.contains("3 failed · 3 HTTP 429"));
            assert!(text.contains("47.5 tok/s"));
            assert!(text.contains("Cache hit 80.0%"));
            assert!(text.contains("Cache samples 1/5"));
            let (rect, _) = renderer
                .footer_hits
                .iter()
                .find(|(_, a)| matches!(a, FooterAction::Theme))
                .unwrap();
            renderer.click(&mut v, rect.x, rect.y);
            assert_eq!(v.theme, theme.next());
            if let Ok(path) = std::env::var("YOURAI_THEME_SNAPSHOT") {
                let cells = terminal.backend().buffer().content().iter().map(|c|json!({"text":c.symbol(),"fg":format!("{:?}",c.fg),"bg":format!("{:?}",c.bg)})).collect::<Vec<_>>();
                std::fs::write(
                    format!("{path}-{}.json", theme.name()),
                    serde_json::to_vec(&json!({"width":120,"height":36,"cells":cells})).unwrap(),
                )
                .unwrap();
            }
        }
        v.editor.insert("/");
        v.toast = Some(("✓ 已复制".into(), std::time::Instant::now()));
        terminal
            .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("Commands"));
        assert!(text.contains("/continue"));
        assert!(text.contains('已') && text.contains('复') && text.contains('制'));
        let (rect, _) = renderer
            .command_hits
            .iter()
            .find(|(_, c)| c.text == "/queue")
            .unwrap();
        renderer.click(&mut v, rect.x, rect.y);
        assert_eq!(v.editor.text, "/queue ");
        v.editor.take();
        v.toast = None;
        v.context_usage.as_mut().unwrap().context_window = None;
        terminal
            .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("limit unknown"));
    }

    #[test]
    fn welcome_footer_and_live_activity() {
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        let mut renderer = Renderer::default();
        let mut v = View::default();
        let m = Metadata {
            model: "test-model".into(),
            session: "session-123".into(),
            cwd: "/workspace".into(),
            trusted_shell: false,
            yolo: false,
        };
        terminal
            .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
            .unwrap();
        let rows = |t: &Terminal<TestBackend>| {
            t.backend()
                .buffer()
                .content()
                .chunks(120)
                .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
                .collect::<Vec<_>>()
        };
        let screen = rows(&terminal);
        assert!(screen.iter().any(|r| r.contains(".--------.")));
        assert!(screen[31].contains("test-model"));

        assert!(v.sidebar);
        if let Ok(path) = std::env::var("YOURAI_WELCOME_SNAPSHOT") {
            let cells = terminal.backend().buffer().content().iter().map(|c| json!({"text":c.symbol(),"fg":format!("{:?}",c.fg),"bg":format!("{:?}",c.bg)})).collect::<Vec<_>>();
            std::fs::write(
                path,
                serde_json::to_vec(&json!({"width":120,"height":32,"cells":cells})).unwrap(),
            )
            .unwrap();
        }
        v.user("Review code", false);
        v.active = true;
        v.sidebar = true;
        v.event(Out::Reasoning {
            text: "checking".into(),
        });
        assert_eq!(activity(&v, false), "Thinking");
        v.event(Out::ToolStarted {
            id: "t".into(),
            name: "shell".into(),
            input: json!({"command":"cargo test"}),
        });
        assert_eq!(activity(&v, false), "Running shell");
        terminal
            .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 2, false))
            .unwrap();
        let screen = rows(&terminal).join("\n");
        assert!(!screen.contains(".--------."));
        assert!(screen.contains("Running shell"));
        assert!(screen.contains("2 queued inputs"));
        assert!(screen.contains("SESSION OVERVIEW"));
        v.event(Out::Ask {
            id: "approval".into(),
            payload: json!({"kind":"permission"}),
        });
        assert_eq!(activity(&v, false), "Waiting for approval");
        v.settle();
        terminal
            .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
            .unwrap();
        assert!(renderer.animation_start.is_none());
        renderer.anchor = Some((0, 0));
        renderer.reveal(Some(0));
        renderer.follow(&mut v);
        assert!(renderer.anchor.is_none() && renderer.reveal.is_none());
        assert_eq!(v.scroll, 0);
        assert_ne!(
            pulse_color(0.0, super::super::theme::Theme::Dark),
            pulse_color(1.0, super::super::theme::Theme::Dark)
        );
        assert_eq!(
            pulse_color(0.0, super::super::theme::Theme::Dark),
            pulse_color(2.0, super::super::theme::Theme::Dark)
        );
    }

    #[test]
    fn mouse_targets_only_one_block_and_stays_correct_after_resize() {
        let mut terminal = Terminal::new(TestBackend::new(90, 32)).unwrap();
        let mut renderer = Renderer::default();
        let mut view = View::default();
        view.user("检查实现", false);
        view.event(Out::Reasoning {
            text: "private-thought".into(),
        });
        view.event(Out::ToolStarted {
            id: "one".into(),
            name: "shell".into(),
            input: json!({"command":"cargo test"}),
        });
        view.event(Out::ToolDone {
            id: "one".into(),
            name: "shell".into(),
            output: json!({"stdout":"tool-secret","ok":true}),
            is_error: false,
        });
        view.event(Out::Message {
            text: "**完成**".into(),
        });
        let m = Metadata {
            model: "mock".into(),
            session: "session".into(),
            cwd: "/tmp".into(),
            trusted_shell: false,
            yolo: false,
        };
        terminal
            .draw(|f| renderer.draw(f, &mut view, &m, &SessionStatus::Idle, 0, false))
            .unwrap();
        assert_eq!(renderer.hits.len(), 2);
        let (rect, id) = renderer.hits[1];
        assert!(!renderer
            .lines
            .iter()
            .any(|l| l.to_string().contains("tool-secret")));
        renderer.click(&mut view, rect.x + 2, rect.y);
        terminal
            .draw(|f| renderer.draw(f, &mut view, &m, &SessionStatus::Idle, 0, false))
            .unwrap();
        assert!(renderer
            .lines
            .iter()
            .any(|l| l.to_string().contains("tool-secret")));
        assert!(!renderer
            .lines
            .iter()
            .any(|l| l.to_string().contains("private-thought")));
        terminal.backend_mut().resize(50, 20);
        terminal.resize(Rect::new(0, 0, 50, 20)).unwrap();
        terminal
            .draw(|f| renderer.draw(f, &mut view, &m, &SessionStatus::Idle, 0, false))
            .unwrap();
        let (rect, _) = *renderer.hits.iter().find(|(_, key)| *key == id).unwrap();
        renderer.click(&mut view, rect.x + 2, rect.y);
        assert!(!view.expanded.contains(&id));
    }

    #[test]
    fn layouts_fit_narrow_and_wide_terminals() {
        for (w, h) in [(20, 6), (50, 16), (120, 36)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            let mut renderer = Renderer::default();
            let mut v = View::default();
            v.user("Fix the parser and run tests", false);
            v.event(Out::Reasoning {
                text: "Inspect the parser boundary and preserve existing behavior.".into(),
            });
            v.set_todos(vec![
                super::super::state::Todo {
                    id: "a".into(),
                    text: "Inspect parser".into(),
                    completed: true,
                },
                super::super::state::Todo {
                    id: "b".into(),
                    text: "Fix boundary conditions".into(),
                    completed: true,
                },
                super::super::state::Todo {
                    id: "c".into(),
                    text: "Run regression tests".into(),
                    completed: false,
                },
            ]);
            v.event(Out::Message{text:"## Plan\nRead the parser, fix the boundary, then test.\n```rust\nlet end = input.len();\n```".into()});
            v.event(Out::ToolStarted {
                id: "c1".into(),
                name: "shell".into(),
                input: json!({"command":"cargo test --workspace"}),
            });
            v.event(Out::ToolDone{id:"c1".into(),name:"shell".into(),output:json!({"ok":true,"exit_code":0,"termination":"exit","stdout":"171 tests passed","output_complete":true}),is_error:false});
            v.editor
                .insert("再检查一下边界条件\nKeep the public API unchanged.");
            let m = Metadata {
                model: "coding-model".into(),
                session: "12345678-session".into(),
                cwd: "~/projects/yourai".into(),
                trusted_shell: false,
                yolo: false,
            };
            terminal
                .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
                .unwrap();
            let content = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(content.contains(if w < 30 { "YourAI" } else { "ready" }));
            if w == 120 {
                assert!(!content.contains("171 tests passed"));
                assert!(!content.contains("YOURAI"));
                assert!(!content.contains("YOU  "));
                if let Ok(path) = std::env::var("YOURAI_TUI_SNAPSHOT") {
                    let cells=terminal.backend().buffer().content().iter().map(|c|json!({"text":c.symbol(),"fg":format!("{:?}",c.fg),"bg":format!("{:?}",c.bg)})).collect::<Vec<_>>();
                    std::fs::write(
                        path,
                        serde_json::to_vec(&json!({"width":w,"height":h,"cells":cells})).unwrap(),
                    )
                    .unwrap();
                }
            }
            v.event(Out::Ask{id:"approval".into(),payload:json!({"kind":"permission","tool_name":"shell","input":{"command":"cargo test"}})});
            terminal
                .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
                .unwrap();
        }
    }
}
