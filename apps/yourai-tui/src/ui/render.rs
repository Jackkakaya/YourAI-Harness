mod overlays;
use overlays::*;
mod cards;
mod results;
mod timeline;
use super::overlay::Overlay;
use super::{
    editor::Editor,
    state::{DiffRow, Item, Role, ToolStatus, View},
    theme::{
        lerp_color, mix, Theme, ACCENT, BG, BLUE, BORDER, DIFF_ADD_BG, DIFF_DEL_BG, FAINT,
        FOCUS_SURFACE, GREEN, MUTED, PANEL, RED, TEXT, USER_SURFACE, YELLOW,
    },
};
use cards::*;
use ratatui::{
    prelude::*,
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
#[cfg(test)]
use yourai_core::prelude::Out;
use yourai_core::prelude::{Level, SessionStatus};

pub struct Metadata {
    pub session: String,
    pub cwd: String,
    pub trusted_shell: bool,
    pub yolo: bool,
}
#[derive(Default)]
pub struct Renderer {
    pub selection: super::selection::Selection,
    key: Option<(u64, u16, u16, Theme, bool)>,
    timeline: timeline::TimelineCache,
    lines: timeline::LayoutLines,
    headers: Vec<(usize, u64)>,
    hits: Vec<(Rect, u64)>,
    result_hits: Vec<(Rect, u64)>,
    command_hits: Vec<(Rect, super::commands::Command)>,
    command_area: Option<Rect>,
    follow_hit: Option<Rect>,
    question_hit: Option<(Rect, u64)>,
    todo_hit: Option<Rect>,
    todo_area: Option<Rect>,
    transcript: Rect,
    panel: Option<Rect>,
    anchor: Option<(u64, usize)>,
    reveal: Option<u64>,
    turn_target: Option<u64>,
    result_target: Option<u64>,
    /// 40ms tick frame index shared by the breathing bar and card spinners.
    tick: u64,
}
impl Renderer {
    pub fn begin_selection(&mut self, x: u16, y: u16) {
        let point = Position::new(x, y);
        let region = if self.transcript.contains(point) {
            Some(self.transcript)
        } else if self.panel.is_some_and(|r| r.contains(point)) {
            self.panel
        } else {
            // Title/border rows and the footer: select from the whole screen.
            self.selection.screen.as_ref().map(|b| b.area)
        };
        self.selection.begin(point, region.unwrap_or_default());
    }

    pub fn click(&mut self, view: &mut View, x: u16, y: u16) {
        let point = Position::new(x, y);
        if let Some((rect, id)) = self.question_hit {
            if rect.contains(point) {
                self.turn_target = Some(id);
                return;
            }
        }
        if self.follow_hit.is_some_and(|rect| rect.contains(point)) {
            self.follow(view);
            return;
        }
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
        if self.todo_hit.is_some_and(|r| r.contains(point)) {
            view.todo_panel = !view.todo_panel;
            return;
        }
        if let Some((_, id)) = self.result_hits.iter().find(|(r, _)| r.contains(point)) {
            if !view.expanded(*id) {
                view.toggle(*id);
            }
            self.reveal = Some(*id);
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
            && view.todo_panel
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
    pub fn latest_results(&mut self, view: &mut View) {
        if let Some(&(_, id)) = self.timeline.result_starts.last() {
            self.result_target = Some(id);
        } else {
            view.toast = Some(("No recorded actions yet".into(), std::time::Instant::now()));
        }
    }
    pub fn latest_turn(&mut self) {
        self.turn_target = self.timeline.turns.last().map(|(_, id)| *id);
    }
    pub fn jump_turn(&mut self, view: &mut View, previous: bool) {
        let start = self
            .lines
            .len()
            .saturating_sub(view.scroll + self.transcript.height as usize);
        self.turn_target = if previous {
            self.timeline
                .turns
                .iter()
                .rev()
                .find(|(line, _)| *line < start)
                .or_else(|| self.timeline.turns.first())
                .map(|(_, id)| *id)
        } else {
            self.timeline
                .turns
                .iter()
                .find(|(line, _)| *line > start)
                .map(|(_, id)| *id)
        };
        if !previous && self.turn_target.is_none() {
            self.follow(view);
        }
    }
    pub fn follow(&mut self, view: &mut View) {
        self.result_target = None;
        self.turn_target = None;
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
        _status: &SessionStatus,
        queued: usize,
        compact: bool,
    ) {
        let area = f.area();
        self.hits.clear();
        self.result_hits.clear();
        self.command_hits.clear();
        self.command_area = None;
        self.follow_hit = None;
        self.question_hit = None;
        self.todo_hit = None;
        self.todo_area = None;
        self.tick = self.tick.wrapping_add(1);
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
        let content_area = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
        // Todo panel appears only when there are tasks to inspect.
        // Below 80 columns use a one-line dock above the input.
        let sidebar_visible = v.todo_panel && !v.todos.is_empty() && area.width >= 80;
        let panel_w = (area.width / 3).clamp(28, 40);
        // The composer owns the whole window width, independently of Todo.
        let width = content_area.width.saturating_sub(4).max(2) as usize;
        let (editor_lines, _, _) = v.editor.layout(width);
        // Leave three editable rows at rest; shrink gracefully on short terminals.
        let input_height = editor_lines
            .len()
            .saturating_add(2)
            .max(5)
            .min(usize::from((area.height / 3).clamp(3, 10)))
            .min(usize::from(area.height.saturating_sub(6))) as u16;
        let input_height = if v.asks_empty() { input_height } else { 0 };
        let footer_lines = footer_lines(area.width as usize, v, m, queued);
        let busy = v.active || compact || !v.asks_empty();
        let activity_height = if busy { 1 } else { 0 };
        let narrow_dock = if !sidebar_visible && !v.todos.is_empty() && v.todo_panel {
            1
        } else {
            0
        };
        let ask_height = if v.asks_empty() {
            0
        } else {
            (area.height / 2).clamp(4, 13)
        };
        let ask_height = ask_height.min(
            content_area
                .height
                .saturating_sub(input_height + activity_height + narrow_dock + 1),
        );
        let rows = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(ask_height),
            Constraint::Length(activity_height),
            Constraint::Length(narrow_dock),
            Constraint::Length(input_height),
        ])
        .split(content_area);
        let cols = if sidebar_visible {
            Layout::horizontal([Constraint::Min(50), Constraint::Length(panel_w)]).split(rows[0])
        } else {
            Layout::horizontal([Constraint::Percentage(100)]).split(rows[0])
        };
        let inner = cols[0];
        self.transcript = inner;
        let preview_rows = edit_preview_quota(inner.height);
        let key = (v.revision, inner.width, inner.height, v.theme, v.active);
        if self.key != Some(key) {
            let old = self.lines.len();
            (self.lines, self.headers) = self.timeline.layout(
                v,
                inner.width.saturating_sub(2) as usize,
                preview_rows,
                self.tick,
            );
            if self.key.is_some_and(|k| k.1 == key.1) && v.scroll > 0 {
                v.scroll = if self.lines.len() >= old {
                    v.scroll.saturating_add(self.lines.len() - old)
                } else {
                    v.scroll.saturating_sub(old - self.lines.len())
                };
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
        if let Some(id) = self.turn_target.take() {
            if let Some((line, _)) = self.timeline.turns.iter().find(|(_, key)| *key == id) {
                v.scroll = self.lines.len().saturating_sub(*line + height);
            }
        }
        if let Some(id) = self.result_target.take() {
            if let Some((line, _)) = self
                .timeline
                .result_starts
                .iter()
                .find(|(_, key)| *key == id)
            {
                v.scroll = self.lines.len().saturating_sub(*line + height);
            }
        }
        v.scroll = v.scroll.min(self.lines.len().saturating_sub(height));
        let end = self.lines.len().saturating_sub(v.scroll);
        let start = end.saturating_sub(height);
        let mut visible = self.lines.viewport(start..end);
        // Only visible tool headers need animation or mouse hit regions.
        let first_header = self.headers.partition_point(|(line, _)| *line < start);
        for &(line, id) in &self.headers[first_header..] {
            if line >= end {
                break;
            }
            if let Some(Item::Tool(tool)) = v.items().get(id.saturating_sub(v.item_id(0)) as usize)
            {
                if tool.status == ToolStatus::Running {
                    visible[line - start] = tool_title(
                        tool,
                        v.selected() == Some(id),
                        inner.width.saturating_sub(2) as usize,
                        self.tick,
                    );
                }
            }
            self.hits.push((
                Rect::new(inner.x, inner.y + (line - start) as u16, inner.width, 1),
                id,
            ));
        }
        let first_result = self
            .timeline
            .result_links
            .partition_point(|(line, _)| *line < start);
        for &(line, id) in &self.timeline.result_links[first_result..] {
            if line >= end {
                break;
            }
            self.result_hits.push((
                Rect::new(inner.x, inner.y + (line - start) as u16, inner.width, 1),
                id,
            ));
        }
        f.render_widget(Paragraph::new(visible), inner);
        if v.items().is_empty() {
            welcome(f, inner);
        }
        draw_editor(
            f,
            &v.editor,
            rows[4],
            v.asks_empty() && !v.overlay.is_open(),
            if v.active {
                "Add guidance · Enter to steer · /queue for next"
            } else {
                "Ask anything · / commands"
            },
        );
        // Contextual navigation lives in composer padding, not a second status bar.
        if rows[4].height >= 3 && !v.overlay.is_open() {
            let question = if v.scroll == 0 {
                self.timeline.turns.last()
            } else {
                self.timeline
                    .turns
                    .iter()
                    .rev()
                    .find(|(line, _)| *line <= start)
                    .or_else(|| self.timeline.turns.first())
            };
            let mut x = rows[4].x + 2;
            let y = rows[4].bottom() - 1;
            if let Some(&(_, id)) = question {
                let label = if rows[4].width >= 70 && v.scroll == 0 {
                    "↑ Question · Ctrl-Home"
                } else {
                    "↑ Question"
                };
                let rect = Rect::new(x, y, label.width() as u16, 1);
                f.render_widget(
                    Paragraph::new(label).style(Style::default().fg(MUTED).bg(PANEL)),
                    rect,
                );
                self.question_hit = Some((rect, id));
                x += rect.width + 3;
            }
            if v.scroll > 0 {
                let label = if rows[4].right().saturating_sub(x) >= 38 {
                    "↓ Latest · Ctrl-End · reading history"
                } else {
                    "↓ Latest"
                };
                let label = elide(label, rows[4].right().saturating_sub(x + 1) as usize);
                let rect = Rect::new(x, y, label.width() as u16, 1);
                f.render_widget(
                    Paragraph::new(label).style(Style::default().fg(ACCENT).bg(PANEL)),
                    rect,
                );
                self.follow_hit = Some(rect);
            }
        }
        if let Some(ask) = v.ask() {
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
            let inner = block.inner(rows[1]);
            f.render_widget(block, rows[1]);
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
            if parts[2].width > 0 && parts[2].height > 0 && !v.overlay.is_open() {
                f.set_cursor_position((
                    parts[2].x + col.min(parts[2].width.saturating_sub(1) as usize) as u16,
                    parts[2].y,
                ));
            }
        }
        // Breathing bar: one line above the input, visible only while busy.
        if activity_height > 0 {
            draw_activity_bar(f, rows[2], v, compact, self.tick);
        }
        // Narrow-screen TODO dock (single line, only when panel won't fit).
        if narrow_dock > 0 {
            draw_narrow_todo_dock(f, rows[3], v);
            self.todo_hit = Some(rows[3]);
        }
        let footer = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
        f.render_widget(Paragraph::new(footer_lines), footer);
        v.commands
            .sync(&v.editor.text, v.asks_empty() && !v.overlay.is_open());
        let commands = v.commands.items();
        if !commands.is_empty() {
            let height = (commands.len() as u16 + 2).min(rows[4].y.saturating_sub(area.y));
            let rect = Rect::new(
                rows[4].x,
                rows[4].y.saturating_sub(height),
                rows[4].width.min(62),
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
                            .add_modifier(if i == v.commands.selected {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ))
                    .style(Style::default().bg(if i == v.commands.selected {
                        FOCUS_SURFACE
                    } else {
                        PANEL
                    }))
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
        if let Some(side) = cols.get(1) {
            let hits = sidebar(f, *side, v);
            self.panel = Some(*side);
            self.todo_hit = hits.0;
            self.todo_area = hits.1;
        } else {
            self.panel = None;
        }
        match &v.overlay {
            Overlay::None => {}
            Overlay::Stats { .. } => stats_overlay(f, area, v, m, queued),
            Overlay::Models(_) => model_picker_overlay(f, area, v),
            Overlay::Sessions(_) => sessions_overlay(f, area, v),
            Overlay::Themes(_) => theme_picker_overlay(f, area, v),
            Overlay::Help { scroll } => help(f, area, *scroll),
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

fn pulse_color(seconds: f32, theme: super::theme::Theme) -> Color {
    let intensity = (1.0 - (seconds * std::f32::consts::PI).cos()) * 0.5;
    lerp_color(theme.color(MUTED), theme.color(ACCENT), intensity)
}
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
fn spinner_frame(tick: u64) -> char {
    SPINNER[(tick as usize) % SPINNER.len()]
}
fn activity(v: &View, compact: bool) -> String {
    if let Some(ask) = v.ask() {
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
    if let Some(retry) = &v.retry {
        let seconds = retry
            .until
            .saturating_duration_since(std::time::Instant::now())
            .as_secs_f64()
            .ceil() as u64;
        return format!(
            "{} · {}s 后重试 ({}/{})",
            retry.reason, seconds, retry.attempt, retry.max
        );
    }
    if let Some(Item::Tool(tool)) = v
        .items()
        .iter()
        .rev()
        .find(|item| matches!(item, Item::Tool(t) if t.status == ToolStatus::Running))
    {
        let summary = tool.summary.lines().next().unwrap_or("");
        if summary.trim().is_empty() {
            return format!("Running {}", tool.name);
        }
        let summary: String = summary.chars().take(60).collect();
        return format!("Running {summary}");
    }
    v.model_activity().into()
}
fn elapsed_str(v: &View) -> String {
    v.since
        .map(|t| {
            let s = t.elapsed().as_secs();
            if s >= 60 {
                format!("{}m {}s", s / 60, s % 60)
            } else {
                format!("{s}s")
            }
        })
        .unwrap_or_default()
}
/// Context pressure ratio for color thresholds.
fn ctx_pressure(v: &View) -> Option<f64> {
    let usage = v.context_usage.as_ref()?;
    let used = usage.estimated_tokens;
    usage
        .context_window
        .filter(|w| *w > 0)
        .map(|w| used as f64 / w as f64)
}
fn ctx_color(ratio: f64) -> Color {
    if ratio >= 0.85 {
        RED
    } else if ratio >= 0.70 {
        YELLOW
    } else {
        GREEN
    }
}
fn ctx_bar(ratio: f64) -> String {
    let filled = (ratio.clamp(0.0, 1.0) * 10.0).round() as usize;
    format!("{}{}", "▓".repeat(filled), "░".repeat(10 - filled))
}
fn permission_label(yolo: bool, trusted: bool) -> &'static str {
    if yolo {
        "YOLO"
    } else if trusted {
        "trusted"
    } else {
        "ask"
    }
}
fn welcome(f: &mut Frame<'_>, area: Rect) {
    // A task-oriented empty state; no terminal banner or persistent title bar.
    let wide = area.width >= 60 && area.height >= 10;
    let content = if wide {
        vec![
            ("YourAI", Style::default().fg(TEXT).bold()),
            ("", Style::default()),
            (
                "Build something. Solve a problem.",
                Style::default().fg(TEXT).bold(),
            ),
            (
                "Describe a change, debug an issue, or review code.",
                Style::default().fg(MUTED),
            ),
            ("", Style::default()),
            (
                "/sessions  Resume work     /models  Choose model",
                Style::default().fg(MUTED),
            ),
        ]
    } else {
        vec![
            ("YourAI", Style::default().fg(TEXT).bold()),
            ("Build · Debug · Review", Style::default().fg(TEXT)),
            ("/ commands · F1 help", Style::default().fg(MUTED)),
        ]
    };
    let height = (content.len() as u16).min(area.height);
    let width = area.width.saturating_sub(4).min(52);
    let rect = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 3,
        width,
        height,
    );
    let lines = content
        .into_iter()
        .map(|(text, style)| Line::from(Span::styled(elide(text, width as usize), style)))
        .collect::<Vec<_>>();
    f.render_widget(Paragraph::new(lines), rect);
}

fn draw_editor(f: &mut Frame<'_>, e: &Editor, area: Rect, focus: bool, placeholder: &str) {
    if area.is_empty() {
        return;
    }
    let block = Block::default()
        .style(Style::default().bg(PANEL).fg(TEXT))
        .padding(ratatui::widgets::Padding::new(3, 1, 1, 1));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if !inner.is_empty() {
        f.render_widget(
            Paragraph::new("›").style(
                Style::default()
                    .fg(if focus { ACCENT } else { MUTED })
                    .bold(),
            ),
            Rect::new(area.x + 1, inner.y, 1, 1),
        );
        if e.text.is_empty() {
            f.render_widget(
                Paragraph::new(elide(placeholder, inner.width as usize))
                    .style(Style::default().fg(MUTED)),
                inner,
            );
        }
    }
    let (lines, row, col) = e.layout(inner.width as usize);
    let top = row.saturating_sub(inner.height.saturating_sub(1) as usize);
    if !e.text.is_empty() {
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
    }
    if focus && inner.width > 0 && inner.height > 0 {
        f.set_cursor_position((
            inner.x + col.min(inner.width.saturating_sub(1) as usize) as u16,
            inner.y + (row - top) as u16,
        ));
    }
}
/// Todo-only panel; diagnostics are available in the dashboard.
fn sidebar(f: &mut Frame<'_>, area: Rect, v: &View) -> (Option<Rect>, Option<Rect>) {
    let block = Block::default()
        .padding(ratatui::widgets::Padding::new(2, 0, 0, 0))
        .style(Style::default().bg(BG));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let done = v.todos.iter().filter(|t| t.completed).count();
    f.render_widget(
        Paragraph::new(format!(" Todo · {done}/{} · ^T", v.todos.len()))
            .style(Style::default().fg(TEXT).bold()),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let body = Rect::new(
        inner.x,
        inner.y.saturating_add(1),
        inner.width,
        inner.height.saturating_sub(1),
    );
    let current = v.todos.iter().position(|t| !t.completed);
    let mut lines = Vec::new();
    for (i, todo) in v.todos.iter().enumerate() {
        let (mark, color) = if todo.completed {
            ("✓", GREEN)
        } else if current == Some(i) {
            ("•", ACCENT)
        } else {
            (" ", MUTED)
        };
        for (row, text) in wrap_todo_text(
            &todo.text.replace('\n', " "),
            inner.width.saturating_sub(4) as usize,
        )
        .into_iter()
        .enumerate()
        {
            lines.push(Line::from(vec![
                Span::styled(
                    if row == 0 {
                        format!("[{mark}] ")
                    } else {
                        "    ".into()
                    },
                    Style::default().fg(color),
                ),
                Span::styled(
                    text,
                    if current == Some(i) {
                        Style::default().fg(TEXT).bold()
                    } else {
                        Style::default().fg(if todo.completed { MUTED } else { TEXT })
                    },
                ),
            ]));
        }
    }
    let scroll = v
        .todo_scroll
        .min(lines.len().saturating_sub(body.height as usize));
    f.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(scroll)
                .take(body.height as usize)
                .collect::<Vec<_>>(),
        ),
        body,
    );
    (
        Some(Rect::new(inner.x, inner.y, inner.width, 1)),
        Some(body),
    )
}

fn wrap_todo_text(text: &str, width: usize) -> Vec<String> {
    if width < 2 {
        return vec![text.to_string()];
    }
    let mut result = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    for g in text.graphemes(true) {
        let gw = g.width();
        if used + gw > width && !line.is_empty() {
            // Trailing whitespace at the break point is invisible padding;
            // drop it so wrapped lines carry only their real text.
            result.push(std::mem::take(&mut line).trim_end().to_string());
            used = 0;
            // The whitespace that triggered the break belongs to the end of
            // the previous line; keeping it would shift the continuation one
            // column past the 4-space indent and misalign wrapped rows.
            if g.chars().all(char::is_whitespace) {
                continue;
            }
        }
        line.push_str(g);
        used += gw;
    }
    result.push(line);
    result
}

/// Narrow-screen single-line TODO dock (between ask and input).
fn draw_narrow_todo_dock(f: &mut Frame<'_>, area: Rect, v: &View) {
    let done = v.todos.iter().filter(|t| t.completed).count();
    let total = v.todos.len();
    let next = v
        .todos
        .iter()
        .find(|t| !t.completed)
        .map(|t| t.text.replace('\n', " "))
        .unwrap_or_default();
    let text = format!(" Todo {done}/{total} · ^T · ► {}", next);
    f.render_widget(
        Paragraph::new(elide(&text, area.width as usize)).style(Style::default().fg(MUTED)),
        area,
    );
}

fn draw_activity_bar(f: &mut Frame<'_>, area: Rect, v: &View, compact: bool, tick: u64) {
    let spinner = spinner_frame(tick);
    let act = activity(v, compact);
    let elapsed = elapsed_str(v);
    let text = if elapsed.is_empty() {
        format!("{spinner} {act} · Esc 打断")
    } else {
        format!("{spinner} {act} · {elapsed} · Esc 打断")
    };
    let color = pulse_color(tick as f32 * 0.04, v.theme);
    f.render_widget(
        Paragraph::new(elide(&text, area.width as usize)).style(Style::default().fg(color)),
        area,
    );
}

/// One full-width row. Drop optional metrics before clipping a value or its unit.
/// Truncated labels and the overflow mark point to the full details in Ctrl-B.
fn footer_lines(width: usize, v: &View, m: &Metadata, queued: usize) -> Vec<Line<'static>> {
    let muted = Style::default().fg(MUTED);
    let permission = permission_label(m.yolo, m.trusted_shell);
    let context = ctx_pressure(v)
        .map(|n| {
            if n * 100.0 > 999.0 {
                ">999%".into()
            } else {
                format!("{:.0}%", n * 100.0)
            }
        })
        .unwrap_or_else(|| "—".into());
    let rate = v
        .model_metrics
        .requests
        .last_output_tokens_per_second
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(compact_number)
        .unwrap_or_else(|| "—".into());
    let mut fields = vec![(1, format!("ctx {context}"))];
    let label_reserve = if width >= 60 { 16 } else { 8 };
    let metrics_budget = width.saturating_sub(label_reserve + permission.width() + 6);
    let mut omitted = false;
    let mut optional = vec![
        (0, format!("tok {}", tokens(v.usage.total_tokens))),
        (2, format!("{rate} tok/s")),
    ];
    if queued > 0 {
        optional.push((
            3,
            format!(
                "{} queued",
                if queued < 1000 {
                    queued.to_string()
                } else {
                    compact_number(queued as f64)
                }
            ),
        ));
    }
    for field in optional {
        let used: usize = fields.iter().map(|(_, text)| text.width() + 3).sum();
        if used + field.1.width() <= metrics_budget {
            fields.push(field);
        } else {
            omitted = true;
        }
    }
    fields.sort_by_key(|(order, _)| *order);
    let metrics = fields
        .into_iter()
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join(" · ");
    let overflow = if omitted { " …" } else { "" };
    let right_width = metrics.width() + overflow.width() + 3 + permission.width();
    let left_width = width.saturating_sub(right_width + 2);
    let title = v.title.as_deref().unwrap_or("New session");
    let title_budget = if title.width() + 3 + m.cwd.width() <= left_width {
        title.width()
    } else {
        title.width().min(left_width.saturating_sub(3) / 2)
    };
    let left = format!(
        "{} · {}",
        elide(title, title_budget),
        elide_tail(&m.cwd, left_width.saturating_sub(title_budget + 3))
    );
    let gap = width.saturating_sub(left.width() + right_width);
    vec![Line::from(vec![
        Span::styled(elide(title, title_budget), Style::default().fg(TEXT).bold()),
        Span::styled(
            format!(
                " · {}",
                elide_tail(&m.cwd, left_width.saturating_sub(title_budget + 3))
            ),
            muted,
        ),
        Span::raw(" ".repeat(gap)),
        Span::styled(metrics, Style::default().fg(TEXT)),
        Span::styled(overflow, muted),
        Span::styled(" · ", Style::default().fg(BORDER)),
        Span::styled(
            permission,
            if m.yolo {
                Style::default().fg(YELLOW).bold()
            } else {
                muted
            },
        ),
    ])]
}

fn compact_number(value: f64) -> String {
    if value >= 1e21 {
        return format!("{value:.1e}");
    }
    for (scale, suffix) in [
        (1e18, "E"),
        (1e15, "P"),
        (1e12, "T"),
        (1e9, "B"),
        (1e6, "M"),
        (1e3, "K"),
    ] {
        if value >= scale {
            return format!("{:.1}{suffix}", value / scale);
        }
    }
    format!("{value:.1}")
}
fn tokens(value: u64) -> String {
    if value < 1_000_000 {
        format!("{:.1}K", value as f64 / 1000.0)
    } else {
        compact_number(value as f64)
    }
}
fn elide_tail(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.into();
    }
    if width == 0 {
        return String::new();
    }
    let mut tail = Vec::new();
    let mut used = 1;
    for g in text.graphemes(true).rev() {
        if used + g.width() > width {
            break;
        }
        used += g.width();
        tail.push(g);
    }
    format!("…{}", tail.into_iter().rev().collect::<String>())
}

fn elide(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
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
fn help(f: &mut Frame<'_>, area: Rect, scroll: u16) {
    let rect = crate::picker::centered(area, 78, area.height.saturating_sub(2) as usize);
    let text="Enter          Send / steer; confirm reply\nCtrl-J/Alt-Enter  Newline (paste preserves newlines)\nArrows/Home/End  Move cursor; Backspace/Delete\nCtrl-A/E/B/F   Line start/end · char back/fwd\nCtrl-W/U/K     Del word · to line start/end\nAlt-B/F/D·Ctrl-Left/Right  Word move · del word\nUp/Down·Ctrl-P/N  History (or row move in multiline)\nPgUp / PgDn     Scroll conversation\nCtrl-End        Follow newest output\nCtrl-Home       Jump to latest question\nCtrl-Up/Down    Previous / next question\nCtrl-G          Toggle YOLO between turns\n/               Command menu · Up/Down · Tab/Enter\nF6/Shift-F6·Click  Select next/prev · expand block\nCtrl-O / Ctrl-R  Toggle selected block / thinking\nCtrl-T          Toggle Todo panel\nCtrl-B          Toggle stats dashboard overlay\nCtrl-Y          Cycle color theme\nMouse drag      Release to copy automatically\nEsc / Ctrl-C    Cancel exec / clear selection / close\nAlt-PgUp/PgDn   Scroll approval details\nCtrl-Q          Quit\n\n/queue TEXT     Schedule a follow-up turn\n/compact        Compact idle conversation\n/new · /clear   Fresh context; previous session saved\n/yolo [on|off]   Change permissions between turns\n/theme          Theme picker (or /theme NAME)\n/models         Switch model (picker or /models p/m [variant])\n/sessions       Switch sessions (Ctrl-D asks to delete)\n/status         Same as Ctrl-B dashboard\n/results        Jump to latest recorded file/command actions\n/help           This help · Esc closes\n\nApprovals: y/n + Enter (YOLO skips approvals).";
    let lines: Vec<_> = text
        .lines()
        .flat_map(|line| {
            super::markdown::wrap_spans(
                vec![Span::raw(line.to_owned())],
                rect.width.saturating_sub(2) as usize,
                "",
            )
        })
        .collect();
    let offset = (scroll as usize).min(
        lines
            .len()
            .saturating_sub(rect.height.saturating_sub(2) as usize),
    ) as u16;
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines)
            .scroll((offset, 0))
            .style(Style::default().bg(PANEL).fg(TEXT))
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(ACCENT))
                    .title(" Help · Esc · ↑↓ scroll "),
            ),
        rect,
    );
}
fn edit_preview_quota(height: u16) -> usize {
    match height {
        0..=14 => 1,
        15..=22 => 2,
        23..=34 => 3,
        35..=49 => 4,
        _ => 5,
    }
}

#[cfg(test)]
fn timeline(v: &View, width: usize) -> (Vec<Line<'static>>, Vec<(usize, u64)>) {
    let (lines, headers) = timeline::TimelineCache::default().layout(v, width, 3, 0);
    (lines.viewport(0..lines.len()), headers)
}

#[cfg(test)]
mod tests {
    /// Repeat with --release --ignored --nocapture for renderer-only timings.
    #[test]
    #[ignore = "manual performance measurement; no machine-dependent pass threshold"]
    fn long_conversation_render_benchmark() {
        use std::time::Instant;
        let mut v = View::default();
        v.theme = Theme::Dark;
        for _ in 0..500 {
            v.user("Review the parser and preserve compatibility.", false);
            v.event(Out::Message { text: "## Changes\n\nUpdated the parser and its error handling.\n\n```rust\nfn parse(input: &str) -> bool {\n    !input.is_empty()\n}\n```\n\n- Tests passed\n- Public API unchanged".into() });
            v.settle();
        }
        let m = Metadata {
            session: "bench".into(),
            cwd: "/workspace".into(),
            trusted_shell: false,
            yolo: false,
        };
        let mut terminal = Terminal::new(TestBackend::new(160, 48)).unwrap();
        let mut renderer = Renderer::default();
        let draw = |terminal: &mut Terminal<TestBackend>, renderer: &mut Renderer, v: &mut View| {
            terminal
                .draw(|f| renderer.draw(f, v, &m, &SessionStatus::Idle, 0, false))
                .unwrap();
        };
        draw(&mut terminal, &mut renderer, &mut v);
        for stream in [false, true] {
            let mut samples = vec![];
            for i in 0..120 {
                v.scroll = 100 + (i % 30) * 3;
                if stream {
                    v.event(Out::Chunk {
                        text: "More output. ".into(),
                    });
                }
                let start = Instant::now();
                draw(&mut terminal, &mut renderer, &mut v);
                samples.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            samples.sort_by(f64::total_cmp);
            eprintln!(
                "render_bench stream={stream} p50={:.3}ms p95={:.3}ms rows={} items={}",
                samples[60],
                samples[114],
                renderer.lines.len(),
                v.items().len()
            );
        }
    }

    #[test]
    fn pickers_fit_small_terminals_and_scroll_to_selected_rows() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut v = View::default();
        v.model_choices = (0..100).map(|i| format!("model-{i:03}")).collect();
        v.overlay = Overlay::Sessions(super::super::state::SessionPickerState {
            pending_delete: None,
            rows: (0..100)
                .map(|i| crate::sessions::SessionRow {
                    id: yourai_core::prelude::SessionId(format!("session-{i:03}")),
                    title: format!("row-{i:03}"),
                    model: String::new(),
                    updated_at: 0,
                    is_current: false,
                })
                .collect(),
            query: String::new(),
            selected: 99,
        });
        for width in [1, 20, 30, 40, 48, 80] {
            for height in [1, 2, 3, 8, 24] {
                let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
                for draw in [model_picker_overlay, sessions_overlay, theme_picker_overlay] {
                    t.draw(|f| draw(f, f.area(), &v)).unwrap();
                }
            }
        }
        let mut t = Terminal::new(TestBackend::new(80, 12)).unwrap();
        for (draw, expected) in [
            (
                sessions_overlay as fn(&mut ratatui::Frame<'_>, Rect, &View),
                "row-099",
            ),
            (model_picker_overlay, "model-099"),
        ] {
            let previous = std::mem::replace(&mut v.overlay, Overlay::Models(99));
            if expected == "row-099" {
                v.overlay = previous;
            }
            t.draw(|f| draw(f, f.area(), &v)).unwrap();
            let text: String = t
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(
                text.contains(expected),
                "selected row is not visible: {expected}"
            );
        }
    }

    fn timeline_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn tool_title_right_aligns_meta_and_elides_long_subject() {
        let mut v = View::default();
        let long = "cargo test --workspace --all-targets ".repeat(8);
        v.event(Out::ToolStarted {
            id: "t".into(),
            name: "shell".into(),
            input: json!({ "command": long }),
        });
        v.event(Out::ToolDone {
            id: "t".into(),
            name: "shell".into(),
            output: json!({"ok":true,"exit_code":0,"termination":"exit","output_complete":true,"stdout":"done\n","stderr":""}),
            is_error: false,
        });
        let (lines, _) = timeline(&v, 80);
        let title = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains('✓')))
            .expect("title line");
        let text: String = title.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains('…'), "long subject elided: {text}");
        assert!(text.contains("exit 0"), "meta survives elision: {text}");
        // Sub-second durations are silent; the meta ends with the exit code.
        assert!(text.trim_end().ends_with("exit 0"), "meta last: {text}");
        assert!(title.width() <= 80, "title fits width: {}", title.width());
    }

    #[test]
    fn failed_shell_preview_prioritizes_stderr_tail_over_stdout_progress() {
        let mut v = View::default();
        v.event(Out::ToolStarted {
            id: "build".into(),
            name: "shell".into(),
            input: json!({"command": "cargo build"}),
        });
        v.event(Out::ToolDone {
            id: "build".into(), name: "shell".into(),
            output: json!({"ok":false,"exit_code":1,"stdout":"Compiling dependencies", "stderr":"warning 1\nwarning 2\nwarning 3\nwarning 4\nwarning 5\nerror: unresolved import"}),
            is_error: true,
        });
        let tool = v
            .items()
            .iter()
            .find_map(|item| {
                if let Item::Tool(t) = item {
                    Some(t)
                } else {
                    None
                }
            })
            .unwrap();
        let mut lines = vec![];
        tool_preview(tool, &mut lines, 80, Theme::Dark, 3);
        let text = lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("error: unresolved import"));
        assert!(text.contains("stderr") && text.contains("^O full output"));
        assert!(!text.contains("Compiling dependencies") && !text.contains("warning 1"));
        lines.clear();
        tool_expanded(tool, &mut lines, 80, Theme::Dark);
        let full = lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(full.contains("Compiling dependencies") && full.contains("warning 1"));
    }

    #[test]
    fn card_previews_follow_per_tool_quotas() {
        let mut v = View::default();
        // read: no code teaser at all.
        v.event(Out::ToolStarted {
            id: "r".into(),
            name: "read".into(),
            input: json!({"path":"src/main.rs"}),
        });
        v.event(Out::ToolDone {
            id: "r".into(),
            name: "read".into(),
            output: json!({"ok":true,"path":"src/main.rs","offset":1,"content":"1|fn main() {}\n"}),
            is_error: false,
        });
        // shell done: exactly one tail line of stdout.
        v.event(Out::ToolStarted {
            id: "s".into(),
            name: "shell".into(),
            input: json!({"command":"seq"}),
        });
        v.event(Out::ToolDone {
            id: "s".into(),
            name: "shell".into(),
            output: json!({"ok":true,"exit_code":0,"termination":"exit","output_complete":true,"stdout":"alpha\nbeta\ngamma\n","stderr":""}),
            is_error: false,
        });
        // write: line count + confirmed "new" marker in the title.
        v.event(Out::ToolStarted {
            id: "w".into(),
            name: "write".into(),
            input: json!({"path":"docs/design.md","content":"# T\n\nbody\n"}),
        });
        v.event(Out::ToolDone {
            id: "w".into(),
            name: "write".into(),
            output: json!({"ok":true,"path":"docs/design.md","created":true,"bytes_written":8}),
            is_error: false,
        });
        // unknown tool: a single summarized line, never raw JSON.
        v.event(Out::ToolDone {
            id: "x".into(),
            name: "mcp__jira".into(),
            output: json!({"result":"Created YOUR-9","ok":true}),
            is_error: false,
        });
        let (lines, _) = timeline(&v, 80);
        let text = timeline_text(&lines);
        assert!(text.contains("1 lines"), "read meta: {text}");
        assert!(text.contains("+3 · new"), "write meta: {text}");
        assert!(text.contains("# T"), "write ghost-diff preview: {text}");
        assert!(!text.contains("fn main"), "read teaser hidden: {text}");
        assert!(text.contains("gamma"), "shell tail line: {text}");
        assert!(!text.contains("alpha"), "shell older lines hidden: {text}");
        assert!(text.contains("Created YOUR-9"), "summarized body: {text}");
        assert!(!text.contains('{'), "no raw json: {text}");
    }

    fn edit_view() -> View {
        let mut v = View::default();
        v.event(Out::ToolStarted {
            id: "e".into(),
            name: "edit".into(),
            input: json!({"path":"src/main.rs","old_text":"    old();","new_text":"    new();\n    more();"}),
        });
        v.event(Out::ToolDone {
            id: "e".into(),
            name: "edit".into(),
            output: json!({"ok":true,"path":"src/main.rs","changed":true,"diff":"--- src/main.rs\n+++ src/main.rs\n@@ -40,3 +40,4 @@\n fn main() {\n-    old();\n+    new();\n+    more();\n }\n"}),
            is_error: false,
        });
        v
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn edit_card_splits_side_by_side_when_wide() {
        let (lines, _) = timeline(&edit_view(), 140);
        let text = timeline_text(&lines);
        assert!(text.contains(" │ "), "split separator: {text}");
        // Old and new versions of the changed line sit on the same row.
        let pair = lines
            .iter()
            .map(line_text)
            .find(|t| t.contains("old();") && t.contains("new();"))
            .expect("paired row: {text}");
        assert!(
            pair.contains("41"),
            "cell gutter carries line numbers: {pair}"
        );
        // Right-aligned meta and full-width, padded cells.
        assert!(text.contains("+2 −1"), "title meta: {text}");
        let row = lines
            .iter()
            .find(|l| line_text(l).contains("old();") && line_text(l).contains("new();"))
            .unwrap();
        assert_eq!(row.width(), 139, "cells pad the full row");
    }

    #[test]
    fn edit_card_unifies_below_split_threshold() {
        let (lines, _) = timeline(&edit_view(), 100);
        let text = timeline_text(&lines);
        assert!(
            !text.contains(" │ "),
            "no split separator when narrow: {text}"
        );
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            texts
                .iter()
                .any(|t| t.starts_with("  - ") && t.contains("old();")),
            "old line carries the '-' gutter: {texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|t| t.starts_with("  + ") && t.contains("new();")),
            "new line carries the '+' gutter: {texts:?}"
        );
    }

    #[test]
    fn websearch_preview_lists_sources_with_more_hint() {
        let mut v = View::default();
        let content = (1..=4)
            .map(|i| format!("Title: Result {i}\nURL: https://host{i}.example/page"))
            .collect::<Vec<_>>()
            .join("\n\n");
        v.event(Out::ToolDone {
            id: "w".into(),
            name: "websearch".into(),
            output: json!({"provider":"exa","query":"q","content":content}),
            is_error: false,
        });
        let (lines, _) = timeline(&v, 80);
        let text = timeline_text(&lines);
        assert!(text.contains("Result 1 — host1.example"), "{text}");
        assert!(text.contains("Result 3 — host3.example"), "{text}");
        assert!(!text.contains("Result 4"), "fourth row hidden: {text}");
        assert!(text.contains("⋯ 1 more · ^O"), "more hint: {text}");
    }

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
            session: "session-123".into(),
            cwd: "~/projects/yourai".into(),
            trusted_shell: false,
            yolo: true,
        };
        for theme in [Theme::Dark, Theme::Light, Theme::Nord, Theme::Dracula] {
            v.theme = theme;
            v.overlay = Overlay::Stats { scroll: 0 };
            terminal
                .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
                .unwrap();
            assert_eq!(
                terminal.backend().buffer()[(0, 0)].bg,
                theme.color(USER_SURFACE)
            );
            let text = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(text.contains("32.0K / 128.0K"));
            assert!(text.contains("8 calls"));
            assert!(text.contains("3 failed"));
            assert!(text.contains("47.5 tok/s"));
            assert!(text.contains("cache hit"));
            v.overlay = Overlay::None;
            // Theme cycling via ^Y still works (footer button removed).
            v.theme = v.theme.next();
            assert_ne!(v.theme, theme);
            if let Ok(path) = std::env::var("YOURAI_THEME_SNAPSHOT") {
                let cells = terminal.backend().buffer().content().iter().map(|c|json!({"text":c.symbol(),"fg":format!("{:?}",c.fg),"bg":format!("{:?}",c.bg),"bold":c.modifier.contains(Modifier::BOLD)})).collect::<Vec<_>>();
                std::fs::write(
                    format!("{path}-{}.json", theme.name()),
                    serde_json::to_vec(&json!({"width":120,"height":36,"cells":cells})).unwrap(),
                )
                .unwrap();
            }
        }
        v.editor.insert("/");
        v.toast = Some(("✓ Copied".into(), std::time::Instant::now()));
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
        assert!(text.contains("Copied"));
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
        v.overlay = Overlay::Stats { scroll: 0 };
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
        v.model_label = "test-model".into();
        let m = Metadata {
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
        // Empty state explains useful tasks and exposes existing command entry points.
        assert!(screen.iter().any(|r| r.contains("YourAI")));
        assert!(screen.iter().any(|r| r.contains("/sessions")));
        assert!(screen[31].contains("New session"));

        assert!(!v.overlay.is_open());
        if let Ok(path) = std::env::var("YOURAI_WELCOME_SNAPSHOT") {
            let cells = terminal.backend().buffer().content().iter().map(|c| json!({"text":c.symbol(),"fg":format!("{:?}",c.fg),"bg":format!("{:?}",c.bg),"bold":c.modifier.contains(Modifier::BOLD)})).collect::<Vec<_>>();
            std::fs::write(
                path,
                serde_json::to_vec(&json!({"width":120,"height":32,"cells":cells})).unwrap(),
            )
            .unwrap();
        }
        v.user("Review code", false);
        v.active = true;
        v.event(Out::Reasoning {
            text: "checking".into(),
        });
        assert_eq!(activity(&v, false), "Thinking");
        v.event(Out::ToolStarted {
            id: "t".into(),
            name: "shell".into(),
            input: json!({"command":"cargo test"}),
        });
        assert_eq!(activity(&v, false), "Running cargo test");
        terminal
            .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 2, false))
            .unwrap();
        let screen = rows(&terminal).join("\n");
        assert!(!screen.contains("yourai · your"));
        assert!(screen.contains("Running cargo"));
        assert!(screen.contains("Enter to steer"));
        // Queued count is now in the footer left slot.
        assert!(screen.contains("2 queued"));
        v.event(Out::Ask {
            id: "approval".into(),
            payload: json!({"kind":"permission"}),
        });
        assert_eq!(activity(&v, false), "Waiting for approval");
        v.settle();
        terminal
            .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
            .unwrap();
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
        // Card preview shows output but NOT the input JSON (only expanded shows input).
        assert!(!renderer
            .lines
            .iter()
            .any(|l| l.to_string().contains("\"command\"")));
        renderer.click(&mut view, rect.x + 2, rect.y);
        terminal
            .draw(|f| renderer.draw(f, &mut view, &m, &SessionStatus::Idle, 0, false))
            .unwrap();
        // Expanded shows the input JSON.
        assert!(renderer
            .lines
            .iter()
            .any(|l| l.to_string().contains("\"command\"")));
        // Expanded thinking shows full text (card title already has a summary).
        assert!(renderer
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
        assert!(!view.expanded(id));
    }

    #[test]
    fn todo_sidebar_title_and_process_colors() {
        use super::super::state::Todo;
        use ratatui::style::Modifier;
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let mut renderer = Renderer::default();
        let mut v = View::default();
        v.title = Some("Fix parser crash".into());
        let mut todos: Vec<Todo> = (0..9)
            .map(|i| Todo {
                id: format!("t{i}"),
                text: format!("Task number {i}"),
                completed: i == 0,
            })
            .collect();
        v.set_todos(todos.clone());
        v.event(Out::Reasoning {
            text: "a thought".into(),
        });
        v.event(Out::ToolStarted {
            id: "c1".into(),
            name: "shell".into(),
            input: json!({"command":"cargo test"}),
        });
        v.event(Out::ToolDone {
            id: "c1".into(),
            name: "shell".into(),
            output: json!({"stdout":"171 tests passed","ok":true}),
            is_error: false,
        });
        let m = Metadata {
            session: "12345678-session".into(),
            cwd: "~/projects/yourai".into(),
            trusted_shell: false,
            yolo: false,
        };
        let draw = |t: &mut Terminal<TestBackend>, r: &mut Renderer, v: &mut View| {
            t.draw(|f| r.draw(f, v, &m, &SessionStatus::Idle, 0, false))
                .unwrap();
            t.backend()
                .buffer()
                .content()
                .chunks(120)
                .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
                .collect::<Vec<_>>()
        };
        let screen = draw(&mut terminal, &mut renderer, &mut v);
        let text = screen.join("\n");
        assert!(text.contains("Fix parser crash"));
        assert!(!screen[0].contains("Fix parser crash"), "no top title");
        assert_eq!(text.matches("Fix parser crash").count(), 1);
        // Panel title is "Todo · 1/9" (not "TODO 1/9").
        assert!(text.contains("Todo · 1/9"));
        // Completed items use [✓] marker.
        assert!(text.contains("[✓] Task number 0"));
        // Completed items keep their board position instead of jumping around.
        let list: Vec<usize> = (1..screen.len())
            .filter(|&y| screen[y].contains("Task number"))
            .collect();
        let first = screen[list[0]]
            .find("Task number")
            .map(|_| list[0])
            .unwrap();
        let last = *list.last().unwrap();
        assert!(first < last);
        // Clicking the Todo title toggles the panel off.
        let hit = renderer.todo_hit.unwrap();
        renderer.click(&mut v, hit.x, hit.y);
        assert!(!v.todo_panel);
        let text = draw(&mut terminal, &mut renderer, &mut v).join("\n");
        // With panel off, Todo items are not visible.
        assert!(!text.contains("Task number 0"));
        // Re-open via the field directly (wide screen has no dock to click).
        v.todo_panel = true;
        draw(&mut terminal, &mut renderer, &mut v);
        // Wheel over the TODO list scrolls it, not the transcript.
        let area = renderer.todo_area.unwrap();
        renderer.scroll(&mut v, area.x, area.y, false);
        assert_eq!(v.todo_scroll, 3);
        todos.retain(|t| t.id != "t8");
        v.set_todos(todos);
        assert_eq!(v.todo_scroll, 3);
        // Reasoning renders italic + dim; tool output uses its own color.
        let thinking_id = (0..v.items().len())
            .map(|i| v.item_id(i))
            .find(|id| {
                matches!(
                    v.items()[(*id) as usize],
                    Item::Text {
                        role: Role::Thinking,
                        ..
                    }
                )
            })
            .unwrap();
        v.toggle(thinking_id);
        let tool_id = v.item_id(1);
        v.toggle(tool_id);
        draw(&mut terminal, &mut renderer, &mut v);
        let buffer = terminal.backend().buffer();
        let muted = v.theme.color(super::super::theme::MUTED);
        let tool = v.theme.color(TEXT);
        let thought = buffer
            .content()
            .iter()
            .find(|c| c.symbol() == "a" && c.modifier.contains(Modifier::ITALIC))
            .expect("thinking renders italic");
        assert_eq!(thought.fg, muted);
        assert!(
            buffer
                .content()
                .iter()
                .any(|c| c.symbol() == "1" && c.fg == tool),
            "shell stdout renders in TEXT when expanded"
        );
    }

    #[test]
    fn todo_wrap_keeps_continuations_aligned_with_indent() {
        // 120-col layout -> 40-col panel -> 35 columns of todo text.
        let width = 35;
        let lines = wrap_todo_text(
            "Update documentation with examples that are long enough to wrap inside the todo sidebar",
            width,
        );
        assert_eq!(
            lines,
            vec![
                "Update documentation with examples",
                "that are long enough to wrap inside",
                "the todo sidebar",
            ]
        );
        // The space that triggers a break must not leak into the next line...
        assert!(!lines[1..].iter().any(|l| l.starts_with(' ')));
        // ...and every line still fits the panel text width.
        assert!(lines.iter().all(|l| l.width() <= width));
        // Exact-fit and no-space cases keep their text.
        assert_eq!(wrap_todo_text("exact fit", 9), vec!["exact fit"]);
        assert_eq!(wrap_todo_text("nospaces", 3), vec!["nos", "pac", "es"]);
        // Leading whitespace of the original text is preserved.
        assert_eq!(wrap_todo_text("  keep", 4), vec!["  ke", "ep"]);
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
            for id in ["read-parser", "read-tests"] {
                v.event(Out::ToolStarted {
                    id: id.into(),
                    name: "read".into(),
                    input: json!({"path":id}),
                });
                v.event(Out::ToolDone {
                    id: id.into(),
                    name: "read".into(),
                    output: json!({"ok":true,"content":"source"}),
                    is_error: false,
                });
            }
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
            assert!(content.contains(if w < 30 { "YourAI" } else { "ask" }));
            if w == 120 {
                // Card preview shows output by design (§4.3); full input only when expanded.
                assert!(!content.contains("YOURAI"));
                assert!(!content.contains("AGENT"));
                assert!(!content.contains("YOU"));
                assert!(content.contains("Fix the parser and run tests"));
                assert!(content.contains("›"));
                assert!(!content.contains("╭"), "composer has no rounded frame");
                if let Ok(path) = std::env::var("YOURAI_TUI_SNAPSHOT") {
                    let cells=terminal.backend().buffer().content().iter().map(|c|json!({"text":c.symbol(),"fg":format!("{:?}",c.fg),"bg":format!("{:?}",c.bg),"bold":c.modifier.contains(Modifier::BOLD)})).collect::<Vec<_>>();
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

    #[test]
    fn completed_turn_shows_action_summary_only_after_live_work_finishes() {
        let mut v = View::default();
        v.theme = Theme::Dark;
        v.user("Fix the parser boundary and verify the change.", false);
        v.active = true;
        for (id, name, input, output, error) in [
            (
                "read-a",
                "read",
                json!({"path":"src/parser.rs"}),
                json!({"ok":true,"content":"source"}),
                false,
            ),
            (
                "read-b",
                "read",
                json!({"path":"tests/parser.rs"}),
                json!({"ok":true,"content":"tests"}),
                false,
            ),
            (
                "edit",
                "edit",
                json!({"path":"src/parser.rs"}),
                json!({"ok":true,"diff":"--- src/parser.rs\n+++ src/parser.rs\n@@ -1 +1 @@\n-let end = len - 1;\n+let end = len.saturating_sub(1);"}),
                false,
            ),
            (
                "check",
                "shell",
                json!({"command":"cargo check"}),
                json!({"ok":true,"exit_code":0,"stdout":"Finished dev profile"}),
                false,
            ),
            (
                "test",
                "shell",
                json!({"command":"cargo test parser"}),
                json!({"ok":false,"exit_code":101,"stderr":"parser_empty_input: assertion failed"}),
                true,
            ),
        ] {
            v.event(Out::ToolStarted {
                id: id.into(),
                name: name.into(),
                input,
            });
            v.event(Out::ToolDone {
                id: id.into(),
                name: name.into(),
                output,
                is_error: error,
            });
        }
        let mut terminal = Terminal::new(TestBackend::new(120, 48)).unwrap();
        let mut renderer = Renderer::default();
        let m = Metadata {
            session: "Parser boundary".into(),
            cwd: "~/projects/parser".into(),
            trusted_shell: false,
            yolo: false,
        };
        terminal
            .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
            .unwrap();
        assert!(renderer.timeline.result_starts.is_empty());
        v.event(Out::Message { text:"## Parser boundary updated\n\nThe empty-input case still needs a follow-up fix.\n\n- Build command exited successfully.\n- Parser test command failed; inspect the diagnostic below.".into() });
        v.settle();
        terminal
            .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
            .unwrap();
        assert_eq!(renderer.timeline.result_starts.len(), 1);
        assert_eq!(renderer.result_hits.len(), 3);
        if let Ok(path) = std::env::var("YOURAI_RESULTS_SNAPSHOT") {
            let cells = terminal.backend().buffer().content().iter().map(|c|json!({"text":c.symbol(),"fg":format!("{:?}",c.fg),"bg":format!("{:?}",c.bg),"bold":c.modifier.contains(Modifier::BOLD)})).collect::<Vec<_>>();
            std::fs::write(
                path,
                serde_json::to_vec(&json!({"width":120,"height":48,"cells":cells})).unwrap(),
            )
            .unwrap();
        }
    }

    #[test]
    fn result_rows_open_their_original_tool_and_keep_footer_single_line() {
        let mut v = View::default();
        v.user("Fix parser", false);
        v.event(Out::ToolStarted {
            id: "edit".into(),
            name: "edit".into(),
            input: json!({"path":"src/parser.rs"}),
        });
        v.event(Out::ToolDone {
            id: "edit".into(),
            name: "edit".into(),
            output: json!({"ok":true,"diff":"-old\n+new"}),
            is_error: false,
        });
        let tool = v.item_id(1);
        v.event(Out::Message {
            text: "Explanation.\n\n".repeat(40),
        });
        v.settle();
        let m = Metadata {
            session: "result".into(),
            cwd: "/workspace".into(),
            trusted_shell: false,
            yolo: false,
        };
        for width in [30, 80, 120] {
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            let mut renderer = Renderer::default();
            renderer.follow(&mut v);
            terminal
                .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
                .unwrap();
            let &(rect, id) = renderer
                .result_hits
                .iter()
                .find(|(_, id)| *id == tool)
                .expect("result action visible");
            renderer.click(&mut v, rect.x, rect.y);
            assert!(v.expanded(id));
            terminal
                .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
                .unwrap();
            assert!(
                renderer.hits.iter().any(|(_, id)| *id == tool),
                "target tool revealed"
            );
            renderer.latest_results(&mut v);
            terminal
                .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
                .unwrap();
            assert!(renderer.result_hits.iter().any(|(_, id)| *id == tool));
            assert_eq!(footer_lines(width as usize, &v, &m, 0).len(), 1);
        }
    }

    #[test]
    fn scrolling_long_conversation_keeps_right_edge_clear() {
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let mut renderer = Renderer::default();
        let mut v = View::default();
        v.user("Review this change", false);
        v.event(Out::Message {
            text: "A readable paragraph.\n\n".repeat(80),
        });
        let m = Metadata {
            session: "test".into(),
            cwd: "/workspace".into(),
            trusted_shell: false,
            yolo: false,
        };
        for scroll in [0, 20, 100] {
            v.scroll = scroll;
            terminal
                .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
                .unwrap();
            assert!(renderer.lines.len() > renderer.transcript.height as usize);
            if scroll > 0 {
                assert!(renderer.follow_hit.is_some(), "history has a return action");
            } else {
                assert!(renderer.follow_hit.is_none(), "live view stays quiet");
            }
            for y in 0..renderer.transcript.bottom() {
                assert_eq!(terminal.backend().buffer()[(79, y)].symbol(), " ");
            }
        }
        let hit = renderer.follow_hit.unwrap();
        renderer.click(&mut v, hit.x, hit.y);
        assert_eq!(v.scroll, 0);
        terminal
            .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
            .unwrap();
        assert!(renderer.follow_hit.is_none());
        let (question, id) = renderer
            .question_hit
            .expect("question navigation is discoverable");
        renderer.click(&mut v, question.x, question.y);
        assert_eq!(renderer.turn_target, Some(id));
        terminal
            .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
            .unwrap();
        let start = renderer.lines.len() - v.scroll - renderer.transcript.height as usize;
        assert_eq!(start, renderer.timeline.turns[0].0);
    }

    #[test]
    fn composer_grows_across_full_width_and_keeps_cursor_above_single_footer() {
        let mut v = View::default();
        v.theme = Theme::Dark;
        v.set_todos(vec![super::super::state::Todo {
            id: "task".into(),
            text: "A task should never narrow the composer".into(),
            completed: false,
        }]);
        let m = Metadata {
            session: "test".into(),
            cwd: "/workspace".into(),
            trusted_shell: false,
            yolo: true,
        };
        for (width, height) in [(120, 36), (80, 20), (35, 10)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut renderer = Renderer::default();
            for text in [
                "",
                "one\ntwo\nthree",
                "long line 中文🙂 ".repeat(100).as_str(),
            ] {
                v.editor = Editor::default();
                v.editor.insert(text);
                terminal
                    .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
                    .unwrap();
                let cursor = terminal.get_cursor_position().unwrap();
                let buffer = terminal.backend().buffer();
                let prompt_y = (0..height)
                    .find(|&y| buffer[(1, y)].symbol() == "›")
                    .expect("borderless input prompt remains visible");
                // Surface extends under the sidebar, including its right edge.
                assert_eq!(buffer[(width - 1, prompt_y)].bg, PANEL);
                assert_eq!(buffer[(width - 1, height - 2)].bg, PANEL);
                if let Some(panel) = renderer.panel {
                    assert!(panel.bottom() < prompt_y);
                }
                assert!(cursor.x >= 3 && cursor.x < width - 1);
                assert!(cursor.y >= prompt_y && cursor.y < height - 2);
                let footer = (0..width)
                    .map(|x| buffer[(x, height - 1)].symbol())
                    .collect::<String>();
                assert!(footer.contains("YOLO"));
                if text.is_empty() {
                    let input_height = 5.min((height / 3).clamp(3, 10));
                    assert_eq!(
                        prompt_y,
                        height - input_height,
                        "roomier input adapts to short windows"
                    );
                }
            }
        }
    }

    #[test]
    fn sessions_overlay_renders_rows_and_filters() {
        use super::super::state::SessionPickerState;
        use crate::sessions::SessionRow;
        use yourai_core::prelude::SessionId;
        let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
        let mut renderer = Renderer::default();
        let mut v = View::default();
        let m = Metadata {
            session: "current-id".into(),
            cwd: "/tmp".into(),
            trusted_shell: false,
            yolo: false,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        v.overlay = Overlay::Sessions(SessionPickerState {
            pending_delete: None,
            rows: vec![
                SessionRow {
                    id: SessionId("d3f40178deadbeef".into()),
                    title: "Fix parser off-by-one".into(),
                    model: "kimi-k3".into(),
                    updated_at: now - 2 * 3600,
                    is_current: true,
                },
                SessionRow {
                    id: SessionId("9a1b2c3ddeadd00d".into()),
                    title: "Fix TUI sidebar".into(),
                    model: "glm-4.6".into(),
                    updated_at: now - 3 * 86_400,
                    is_current: false,
                },
            ],
            query: String::new(),
            selected: 0,
        });
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
        assert!(text.contains("Sessions"));
        assert!(text.contains("Fix parser off-by-one"));
        assert!(text.contains("d3f40178"));
        assert!(text.contains("Fix TUI sidebar"));
        // Current session marker visible.
        assert!(text.contains("●"));
        // Filter: type "parser".
        v.overlay.sessions_mut().unwrap().query = "parser".into();
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
        assert!(text.contains("Fix parser off-by-one"));
        assert!(!text.contains("Fix TUI sidebar"));
    }

    #[test]
    fn theme_picker_lists_all_themes_and_marks_current() {
        use super::super::theme::Theme;
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        let mut renderer = Renderer::default();
        let mut v = View::default();
        v.theme = Theme::Nord;
        v.overlay = Overlay::Themes(0);
        let m = Metadata {
            session: "id".into(),
            cwd: "/tmp".into(),
            trusted_shell: false,
            yolo: false,
        };
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
        assert!(text.contains("Themes"));
        // All theme names appear.
        for t in Theme::ALL {
            assert!(text.contains(t.name()), "missing theme {}", t.name());
        }
        // Current theme (Nord) is marked with ●.
        let nord_line = text.lines().find(|l| l.contains("nord")).unwrap();
        assert!(nord_line.contains("●"));
    }

    /// Diagnostic: report how many cells change per frame in common scenarios,
    /// matching what ratatui's double-buffer diff would write to a real terminal.
    #[test]
    fn frame_diff_report() {
        let (w, h) = (120u16, 36u16);
        let cells = (w as usize) * (h as usize);
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut renderer = Renderer::default();
        let mut v = View::default();
        let m = Metadata {
            session: "session-123".into(),
            cwd: "/workspace".into(),
            trusted_shell: false,
            yolo: false,
        };
        let mut prev: Option<ratatui::buffer::Buffer> = None;
        macro_rules! frame {
            ($label:expr, $status:expr) => {{
                terminal
                    .draw(|f| renderer.draw(f, &mut v, &m, &$status, 0, false))
                    .unwrap();
                let buf = terminal.backend().buffer().clone();
                let changed = prev
                    .as_ref()
                    .map(|p| p.diff(&buf).len())
                    .unwrap_or(usize::MAX);
                prev = Some(buf);
                if changed == usize::MAX {
                    println!("{:>6} cells  FIRST FRAME  {}", cells, $label);
                } else {
                    println!(
                        "{:>6} cells ({:>5.1}%)  {}",
                        changed,
                        changed as f64 * 100.0 / cells as f64,
                        $label
                    );
                }
            }};
        }

        frame!("first frame", SessionStatus::Idle);
        // Idle heartbeat: nothing changes except the draw call itself.
        for _ in 0..3 {
            frame!("idle tick (no input, nothing new)", SessionStatus::Idle);
        }

        // A turn starts: busy indicator with the pulse animation.
        v.user("Fix the parser boundary conditions", false);
        v.active = true;
        v.since = Some(std::time::Instant::now());
        frame!("user prompt submitted", SessionStatus::Idle);
        std::thread::sleep(std::time::Duration::from_millis(120));
        frame!("busy tick +120ms (pulse color only)", SessionStatus::Idle);
        std::thread::sleep(std::time::Duration::from_millis(300));
        frame!("busy tick +300ms (pulse color only)", SessionStatus::Idle);

        // Streaming: small chunks arriving one per frame, following the tail.
        let body = "Let me read the parser boundary logic and check the off-by-one. \
                    The issue is at the end-of-input handling where the cursor walks \
                    past the final token. I will patch it and add a regression. ";
        for i in 0..12 {
            v.event(Out::Chunk {
                text: format!("{body} "),
            });
            frame!(
                format!("stream chunk #{i} (follow mode)"),
                SessionStatus::Idle
            );
        }
        // Tool runs while expanded progress streams in.
        v.event(Out::ToolStarted {
            id: "t1".into(),
            name: "shell".into(),
            input: json!({"command":"cargo test --workspace"}),
        });
        frame!("tool started", SessionStatus::Idle);
        let tool_id = v.items().len() as u64 - 1;
        v.toggle(tool_id); // expand live progress
        for i in 0..6 {
            v.event(Out::ToolProgress {
                id: "t1".into(),
                payload: json!({"stdout":"test parser::edge_cases ... ok\n"}),
            });
            frame!(
                format!("tool progress #{i} (expanded, follow mode)"),
                SessionStatus::Idle
            );
        }
        // Scrollback: user scrolls up one wheel notch at a time.
        for i in 0..3 {
            renderer.selection.clear();
            renderer.scroll(&mut v, 10, 10, true);
            frame!(format!("wheel scroll up #{i}"), SessionStatus::Idle);
        }
        renderer.follow(&mut v);
        // Approval overlay appears: layout shifts, transcript shrinks.
        v.event(Out::Ask {
            id: "approval".into(),
            payload: json!({"kind":"permission","tool_name":"shell","input":{"command":"cargo test"}}),
        });
        frame!("approval overlay opens", SessionStatus::Idle);
        v.dismiss_asks();
        frame!("approval overlay closes", SessionStatus::Idle);
        // Help overlay.
        v.overlay = Overlay::Help { scroll: 0 };
        frame!("help overlay opens", SessionStatus::Idle);
        v.overlay = Overlay::None;
        frame!("help overlay closes", SessionStatus::Idle);
        assert_eq!(prev.unwrap().area, Rect::new(0, 0, w, h));
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;
    use ratatui::backend::TestBackend;
    #[test]
    fn questions_remain_reachable_after_long_replies_and_resize() {
        let mut view = View::default();
        let meta = Metadata {
            session: "test".into(),
            cwd: "/workspace".into(),
            trusted_shell: false,
            yolo: false,
        };
        for question in ["First short question", "Second short question"] {
            view.user(question, false);
            view.event(Out::Message {
                text: "A long response paragraph.\n\n".repeat(80),
            });
            view.settle();
        }
        let mut renderer = Renderer::default();
        let mut terminal = Terminal::new(TestBackend::new(90, 30)).unwrap();
        let draw =
            |terminal: &mut Terminal<TestBackend>, renderer: &mut Renderer, view: &mut View| {
                terminal
                    .draw(|f| renderer.draw(f, view, &meta, &SessionStatus::Idle, 0, false))
                    .unwrap();
            };
        draw(&mut terminal, &mut renderer, &mut view);

        renderer.latest_turn();
        draw(&mut terminal, &mut renderer, &mut view);
        let start = renderer.lines.len() - view.scroll - renderer.transcript.height as usize;
        assert_eq!(start, renderer.timeline.turns[1].0);
        renderer.jump_turn(&mut view, true);
        draw(&mut terminal, &mut renderer, &mut view);
        let start = renderer.lines.len() - view.scroll - renderer.transcript.height as usize;
        assert_eq!(start, renderer.timeline.turns[0].0);
        renderer.jump_turn(&mut view, false);
        draw(&mut terminal, &mut renderer, &mut view);
        assert_eq!(
            renderer.lines.len() - view.scroll - renderer.transcript.height as usize,
            renderer.timeline.turns[1].0
        );
        terminal.backend_mut().resize(40, 20);
        terminal.autoresize().unwrap();
        draw(&mut terminal, &mut renderer, &mut view);
        renderer.latest_turn();
        draw(&mut terminal, &mut renderer, &mut view);
        let start = renderer.lines.len() - view.scroll - renderer.transcript.height as usize;
        assert_eq!(start, renderer.timeline.turns[1].0);
        renderer.follow(&mut view);
        draw(&mut terminal, &mut renderer, &mut view);
        assert_eq!(view.scroll, 0);
    }
    #[test]
    fn footer_measures_unicode_long_labels_and_large_metrics() {
        let mut v = View::default();
        v.title = Some("这是一个很长的会话标题 🔎 review ".repeat(6));
        v.model_label = "provider/very-long-model-name-with-reasoning-variant".repeat(3);
        v.usage.total_tokens = u64::MAX;
        v.model_metrics.requests.last_output_tokens_per_second = Some(f64::MAX);
        let m = Metadata {
            session: "test".into(),
            cwd: "/Users/开发者/workspaces/很长的目录名字/YourAI-Harness".into(),
            trusted_shell: false,
            yolo: true,
        };
        for width in 30..=160 {
            let lines = footer_lines(width, &v, &m, usize::MAX);
            assert_eq!(lines.len(), 1);
            for line in &lines {
                assert!(line.width() <= width, "width {width}: {line}");
            }
            let text = lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            for field in ["ctx ", "YOLO"] {
                assert!(text.contains(field));
            }
        }
        v.title = Some("Review".into());
        v.model_label = "mock/model".into();
        v.usage.total_tokens = 1200;
        v.model_metrics.requests.last_output_tokens_per_second = Some(47.5);
        let lines = footer_lines(120, &v, &m, 0);
        assert_eq!(lines.len(), 1, "footer must always use one row");
        assert!(
            lines[0].to_string().contains(&m.cwd),
            "a fitting path stays complete"
        );
    }
    #[test]
    fn tiny_approval_keeps_error_and_reply_visible() {
        let mut view = View::default();
        view.event(Out::Ask {
            id: "ask".into(),
            payload: serde_json::json!({"kind":"permission", "tool_name":"shell"}),
        });
        let ask = view.ask_mut().unwrap();
        ask.error = Some(ask.answer().unwrap_err());
        ask.editor.insert("n");
        let meta = Metadata {
            session: "test".into(),
            cwd: "/workspace".into(),
            trusted_shell: false,
            yolo: false,
        };
        let mut terminal = Terminal::new(TestBackend::new(30, 10)).unwrap();
        terminal
            .draw(|f| Renderer::default().draw(f, &mut view, &meta, &SessionStatus::Idle, 0, false))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("Enter y to allow once"), "{text}");
        assert!(
            !text.contains("Message"),
            "inactive composer should not displace approval"
        );
    }
    #[test]
    fn narrow_footer_preserves_permissions_and_all_dashboard_sizes_fit() {
        let mut v = View::default();
        v.model_label = "provider/model".into();
        v.context_usage = Some(yourai_harness::runtime::ContextUsage {
            estimated_tokens: 90000,
            context_window: Some(100000),
            input_budget: Some(90000),
            output_reserve: 8000,
        });
        let m = Metadata {
            session: "test".into(),
            cwd: "/workspace".into(),
            trusted_shell: false,
            yolo: true,
        };
        for width in [30, 35, 39, 40, 80, 120] {
            let mut terminal = Terminal::new(TestBackend::new(width, 20)).unwrap();
            let mut renderer = Renderer::default();
            terminal
                .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let footer = (0..width)
                .map(|x| buffer[(x, 19)].symbol())
                .collect::<String>();
            assert!(footer.contains("YOLO"), "{footer}");
            assert!(footer.contains("ctx 90%"), "{footer}");
            let footer_rows = footer_lines(width as usize, &v, &m, 0).len() as u16;
            let details = (20 - footer_rows..20)
                .flat_map(|y| (0..width).map(move |x| buffer[(x, y)].symbol()))
                .collect::<String>();
            for field in if width >= 80 {
                vec!["/workspace", "tok", "ctx 90%", "tok/s"]
            } else {
                vec!["ctx 90%"]
            } {
                assert!(details.contains(field), "missing {field}: {details}");
            }
            assert!(
                renderer.panel.is_none(),
                "no tasks means full-width conversation"
            );
            v.overlay = Overlay::Stats { scroll: 0 };
            terminal
                .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
                .unwrap();
            v.overlay = Overlay::None;
        }
    }
}
