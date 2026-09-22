use super::{
    editor::Editor,
    state::{DiffRow, Item, Role, ToolStatus, View},
    theme::{
        lerp_color, mix, Theme, ACCENT, BG, BLUE, BORDER, CYAN, DIFF_ADD_BG, DIFF_DEL_BG, FAINT,
        GREEN, MUTED, PANEL, RED, TEXT, YELLOW,
    },
};
use ratatui::{
    prelude::*,
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
};
use std::time::Duration;
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
    key: Option<(u64, u16, u16, u8)>,
    lines: Vec<Line<'static>>,
    headers: Vec<(usize, u64)>,
    hits: Vec<(Rect, u64)>,
    command_hits: Vec<(Rect, super::commands::Command)>,
    command_area: Option<Rect>,
    todo_hit: Option<Rect>,
    todo_area: Option<Rect>,
    transcript: Rect,
    panel: Option<Rect>,
    anchor: Option<(u64, usize)>,
    reveal: Option<u64>,
    animation_start: Option<std::time::Instant>,
    /// 40ms tick frame index shared by the breathing bar and card spinners.
    tick: u64,
    /// Cached git branch for the footer idle slot; refreshed every ~5s.
    git_branch: Option<(String, std::time::Instant)>,
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
    pub fn follow(&mut self, view: &mut View) {
        self.anchor = None;
        self.reveal = None;
        view.follow();
    }
    pub fn reveal(&mut self, id: Option<u64>) {
        self.reveal = id;
    }

    /// Returns the current git branch (or short detached-HEAD hash) for the
    /// given cwd, cached for ~5s to avoid re-reading `.git/HEAD` every frame.
    /// Returns an empty string when not in a git repo.
    fn git_branch(&mut self, cwd: &str) -> &str {
        let now = std::time::Instant::now();
        let refresh = self
            .git_branch
            .as_ref()
            .is_some_and(|(_, ts)| now.duration_since(*ts) < Duration::from_secs(5));
        if !refresh {
            let branch = read_git_branch(cwd);
            self.git_branch = Some((branch, now));
        }
        self.git_branch.as_ref().unwrap().0.as_str()
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
        self.command_hits.clear();
        self.command_area = None;
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
        // Sidebar = Todo list (top, when todos exist) + telemetry (bottom).
        // Visible when enabled and terminal is wide enough (≥110 cols, matching
        // the original sidebar threshold). Below that, fall back to a single-line
        // Todo dock above the input.
        let sidebar_visible = v.todo_panel && area.width >= 110;
        let panel_w = (area.width / 4).clamp(30, 44);
        let cols = if sidebar_visible {
            Layout::horizontal([Constraint::Min(50), Constraint::Length(panel_w)]).split(area)
        } else {
            Layout::horizontal([Constraint::Percentage(100)]).split(area)
        };
        let width = cols[0].width.saturating_sub(4).max(2) as usize;
        let (editor_lines, _, _) = v.editor.layout(width);
        let input_height = (editor_lines.len() as u16 + 2)
            .clamp(3, (area.height * 3 / 10).clamp(3, 14))
            .min(area.height.saturating_sub(6));
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
            area.height
                .saturating_sub(input_height + activity_height + narrow_dock + 6),
        );
        let rows = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(ask_height),
            Constraint::Length(activity_height),
            Constraint::Length(narrow_dock),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(cols[0]);
        let status_text: &str = if !v.asks_empty() {
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
        let transcript = rows[0];
        let title = v.title.as_deref().unwrap_or("Untitled session");
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(BORDER))
            .title(Line::from(vec![
                Span::styled(" ✦ ", Style::default().fg(ACCENT)),
                Span::styled(title.to_owned(), Style::default().fg(TEXT).bold()),
                Span::raw(" "),
            ]))
            .title(
                Line::from(Span::styled(
                    if v.scroll > 0 {
                        format!(" {} new · Ctrl-End follows ", v.unseen)
                    } else {
                        String::new()
                    },
                    Style::default().fg(MUTED),
                ))
                .alignment(Alignment::Right),
            );
        let inner = block.inner(transcript);
        f.render_widget(block, transcript);
        self.transcript = inner;
        let running_tool = v
            .items()
            .iter()
            .any(|item| matches!(item, Item::Tool(t) if t.status == ToolStatus::Running));
        let spinner = if running_tool {
            (self.tick as usize % SPINNER.len()) as u8
        } else {
            0
        };
        let preview_rows = edit_preview_quota(inner.height);
        let key = (v.revision, inner.width, inner.height, spinner);
        if self.key != Some(key) {
            let old = self.lines.len();
            (self.lines, self.headers) = timeline_at(
                v,
                inner.width.saturating_sub(2) as usize,
                preview_rows,
                self.tick,
            );
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
        // Right-edge scrollbar: only render when content overflows the viewport.
        // Scroll position is top-anchored; `v.scroll` is bottom-anchored, so the
        // top index = lines.len() - scroll - height.
        if self.lines.len() > height {
            let mut state = ScrollbarState::new(self.lines.len())
                .viewport_content_length(height)
                .position(start);
            f.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .thumb_symbol("█")
                    .track_symbol(Some("▐"))
                    .begin_symbol(None)
                    .end_symbol(None)
                    .thumb_style(Style::default().fg(ACCENT))
                    .track_style(Style::default().fg(MUTED)),
                inner,
                &mut state,
            );
        }
        if v.items().is_empty() {
            welcome(f, inner);
        }
        let n_img = v.pending_attachments.len();
        let mode: String = if v.active {
            if n_img > 0 {
                format!(" Steer · Esc interrupts · {n_img} img ")
            } else {
                " Steer current turn · Esc interrupts ".into()
            }
        } else if n_img > 0 {
            format!(" Message · {n_img} img · Ctrl+V paste · Esc clear ")
        } else {
            " Message ".into()
        };
        let input_title_right = " ^T sidebar · ^B stats · F1 help ";
        draw_editor(
            f,
            &v.editor,
            rows[4],
            &mode,
            input_title_right,
            v.asks_empty(),
            ACCENT,
        );
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
            if parts[2].width > 0 && parts[2].height > 0 {
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
        let footer = rows[5];
        draw_footer(f, footer, v, m, status_text, busy, compact, queued, self);
        v.commands.sync(&v.editor.text, v.asks_empty() && !v.help);
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
        // @ mention autocomplete popup.
        if v.mention.active && !v.mention.entries.is_empty() {
            let entries = &v.mention.entries;
            let height = (entries.len() as u16 + 2).min(rows[4].y.saturating_sub(area.y));
            let rect = Rect::new(
                cols[0].x,
                rows[4].y.saturating_sub(height),
                cols[0].width.min(62),
                height,
            );
            f.render_widget(Clear, rect);
            let visible = height.saturating_sub(2) as usize;
            let start = v
                .mention
                .selected
                .saturating_sub(visible.saturating_sub(1));
            let lines = entries
                .iter()
                .enumerate()
                .skip(start)
                .take(visible)
                .map(|(i, e)| {
                    let icon = if e.is_dir { "📁" } else { "📄" };
                    Line::from(Span::styled(
                        format!(
                            " {} {:<40}",
                            if i == v.mention.selected { "›" } else { " " },
                            format!("{icon} {}", e.display)
                        ),
                        Style::default()
                            .fg(if i == v.mention.selected {
                                ACCENT
                            } else {
                                TEXT
                            })
                            .bg(if i == v.mention.selected { BG } else { PANEL }),
                    ))
                })
                .collect::<Vec<_>>();
            f.render_widget(
                Paragraph::new(lines)
                    .style(Style::default().bg(PANEL))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(BORDER))
                            .title(" @ files · ↑↓ · Enter · Esc "),
                    ),
                rect,
            );
        }
        if let Some(side) = cols.get(1) {
            let hits = sidebar(f, *side, v, m, queued);
            self.panel = Some(*side);
            self.todo_hit = hits.0;
            self.todo_area = hits.1;
        } else {
            self.panel = None;
        }
        if v.stats {
            stats_overlay(f, area, v, m, queued);
        }
        if v.model_picker.is_some() {
            model_picker_overlay(f, area, v);
        }
        if v.session_picker.is_some() {
            sessions_overlay(f, area, v);
        }
        if v.theme_picker.is_some() {
            theme_picker_overlay(f, area, v);
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

/// Read the current git branch from `.git/HEAD` by walking up from `cwd`.
/// Returns an empty string if not in a git repo. On detached HEAD, returns
/// the first 7 chars of the commit hash.
fn read_git_branch(cwd: &str) -> String {
    let mut path = std::path::PathBuf::from(cwd);
    loop {
        let head = path.join(".git").join("HEAD");
        if let Ok(content) = std::fs::read_to_string(&head) {
            let trimmed = content.trim();
            if let Some(branch) = trimmed.strip_prefix("ref: refs/heads/") {
                return branch.to_string();
            }
            // Detached HEAD: show short hash.
            return trimmed.chars().take(7).collect();
        }
        if !path.pop() {
            return String::new();
        }
    }
}

// Time-based brightness keeps the indicator smooth without changing its width.
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
    let logo: &[&str] = &[
        " __   __                _    ___ ",
        r" \ \ / /__  _   _ _ __ / \  |_ _|",
        r"  \ V / _ \| | | | '__/ _ \  | | ",
        r"   | | (_) | |_| | | / ___ \ | | ",
        r"   |_|\___/ \__,_|_|/_/   \_\___|",
    ];
    let logo_w = logo[0].len() as u16;
    let palette = f.area().width; // just to avoid unused; real palette via theme
    let _ = palette;
    let wide = area.width >= 56 && area.height >= 12;
    if wide {
        let info_w = 38u16;
        let gap = 4u16;
        let total = logo_w + gap + info_w;
        let offset_x = area.x + (area.width.saturating_sub(total)) / 2;
        let offset_y = area.y + (area.height.saturating_sub(logo.len() as u16 + 4)) / 2;
        // Logo with blue→cyan gradient per column.
        let blue = BLUE;
        let cyan = CYAN;
        for (row, line) in logo.iter().enumerate() {
            for (col, ch) in line.chars().enumerate() {
                let x = offset_x + col as u16;
                let y = offset_y + row as u16;
                if x >= area.right() || y >= area.bottom() {
                    continue;
                }
                if ch == ' ' {
                    continue;
                }
                let t = if logo_w > 1 {
                    col as f32 / (logo_w - 1) as f32
                } else {
                    0.0
                };
                let color = lerp_color(blue, cyan, t);
                f.buffer_mut()[(x, y)].set_char(ch).set_fg(color);
            }
        }
        // Right info column (neofetch style).
        let info_x = offset_x + logo_w + gap;
        let mut y = offset_y;
        let mut info = |label: &str, value: &str| {
            if y >= area.bottom() {
                return;
            }
            let line = format!("{label:<10}{value}");
            for (i, ch) in line.chars().enumerate() {
                let px = info_x + i as u16;
                if px >= area.right() {
                    break;
                }
                let cell = &mut f.buffer_mut()[(px, y)];
                cell.set_char(ch);
                if i < label.len() {
                    cell.set_fg(MUTED);
                } else {
                    cell.set_fg(TEXT);
                }
            }
            y += 1;
        };
        info("yourai", "your coding companion");
        info("", "──────────────────────");
        info("", "");
        info("hint", "/help · ^T sidebar · ^B stats");
        info("", "/models · /sessions");
    } else {
        let lines = vec![
            Line::from(vec![Span::styled(
                "  YourAI",
                Style::default().fg(ACCENT).bold(),
            )]),
            Line::from(Span::styled(
                "  What shall we build?",
                Style::default().fg(MUTED),
            )),
            Line::from(Span::styled(
                "  /help for commands",
                Style::default().fg(MUTED),
            )),
        ];
        f.render_widget(Paragraph::new(lines), area);
    }
}

fn draw_editor(
    f: &mut Frame<'_>,
    e: &Editor,
    area: Rect,
    title: &str,
    title_right: &str,
    focus: bool,
    color: Color,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focus { color } else { BORDER }))
        .style(Style::default().bg(PANEL))
        .title(format!(" {title} "))
        .title(
            Line::from(Span::styled(
                title_right.to_owned(),
                Style::default().fg(MUTED),
            ))
            .alignment(Alignment::Right),
        );
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
/// Right sidebar: Todo list (top, when todos exist) + telemetry (bottom).
/// Returns (title hit box, todo scroll area).
fn sidebar(
    f: &mut Frame<'_>,
    area: Rect,
    v: &View,
    m: &Metadata,
    queued: usize,
) -> (Option<Rect>, Option<Rect>) {
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(PANEL));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let has_todos = !v.todos.is_empty();
    let done = v.todos.iter().filter(|t| t.completed).count();
    let total = v.todos.len();
    let mut lines: Vec<Line<'static>> = vec![];
    let title_hit = Some(Rect::new(inner.x, inner.y, inner.width, 1));
    let scroll_start;
    if has_todos {
        lines.push(Line::from(Span::styled(
            format!(" Todo · {done}/{total} · ^T "),
            Style::default().fg(ACCENT).bold(),
        )));
        lines.push(Line::default());
        scroll_start = lines.len();
        // Stable board order; three-state markers.
        let first_pending = v.todos.iter().position(|t| !t.completed);
        let todo_capacity = inner.height as usize / 2;
        let skip = v
            .todo_scroll
            .min(v.todos.len().saturating_sub(todo_capacity));
        for (i, todo) in v.todos.iter().enumerate().skip(skip).take(todo_capacity) {
            let (mark, mark_color, text_color) = if todo.completed {
                ("✓", GREEN, MUTED)
            } else if first_pending == Some(i) {
                ("•", ACCENT, TEXT)
            } else {
                (" ", MUTED, TEXT)
            };
            let prefix = format!("[{mark}] ");
            let indent = "    ";
            let avail = (inner.width as usize).saturating_sub(prefix.width());
            let wrapped = wrap_todo_text(&todo.text.replace('\n', " "), avail);
            for (wi, wline) in wrapped.iter().enumerate() {
                let p = if wi == 0 { &prefix } else { indent };
                lines.push(Line::from(vec![
                    Span::styled(p.to_owned(), Style::default().fg(mark_color)),
                    Span::styled(wline.clone(), Style::default().fg(text_color)),
                ]));
            }
        }
        let hidden = v.todos.len().saturating_sub(skip + todo_capacity);
        if skip > 0 || hidden > 0 {
            lines.push(Line::from(Span::styled(
                format!(" {skip}↑ {hidden}↓ · wheel scrolls"),
                Style::default().fg(FAINT),
            )));
        }
        let todo_area = Some(Rect::new(
            inner.x,
            inner.y + scroll_start as u16,
            inner.width,
            lines.len().saturating_sub(scroll_start) as u16,
        ));
        lines.push(Line::default());
        // Telemetry section below todos.
        lines.extend(sidebar_telemetry(v, m, queued));
        f.render_widget(Paragraph::new(lines), inner);
        (title_hit, todo_area)
    } else {
        // No todos: telemetry fills the whole sidebar.
        lines.push(Line::from(Span::styled(
            " Session · ^T ",
            Style::default().fg(ACCENT).bold(),
        )));
        lines.push(Line::default());
        lines.extend(sidebar_telemetry(v, m, queued));
        f.render_widget(Paragraph::new(lines), inner);
        (title_hit, None)
    }
}

/// Telemetry lines shared by the sidebar and the ^B overlay.
fn sidebar_telemetry(v: &View, m: &Metadata, queued: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    // Session section.
    lines.push(label(&v.model_label.clone(), TEXT));
    lines.push(label(&format!("cwd {}", abbreviate_home(&m.cwd)), MUTED));
    lines.push(label(
        &format!(
            "session {} · {} calls",
            m.session.chars().take(8).collect::<String>(),
            v.model_metrics.calls
        ),
        MUTED,
    ));
    if queued > 0 {
        lines.push(label(&format!("{queued} queued"), YELLOW));
    }
    lines.push(Line::default());
    // Context section.
    lines.push(label("Context", MUTED));
    if let Some(usage) = &v.context_usage {
        let used = usage.estimated_tokens;
        if let Some(window) = usage.context_window.filter(|w| *w > 0) {
            let ratio = used as f64 / window as f64;
            lines.push(label(
                &format!(
                    "{} / {} ({:.0}%)",
                    tokens(used),
                    tokens(window),
                    ratio * 100.0
                ),
                TEXT,
            ));
            lines.push(label(&ctx_bar(ratio), ctx_color(ratio)));
        } else {
            lines.push(label(
                &format!("{} used · limit unknown", tokens(used)),
                TEXT,
            ));
        }
        if let Some(budget) = usage.input_budget {
            lines.push(label(
                &format!(
                    "{} of {} budget",
                    tokens(budget.saturating_sub(used)),
                    tokens(budget)
                ),
                MUTED,
            ));
        }
        lines.push(label(
            &format!("reserve {}", tokens(usage.output_reserve)),
            MUTED,
        ));
    } else {
        lines.push(label("Estimate unavailable", MUTED));
    }
    let metrics = &v.model_metrics.requests;
    if metrics.journal_errors > 0 {
        lines.push(label("Request log write failed", RED));
    }
    lines.push(Line::default());
    // Requests section.
    lines.push(label("Requests · this run", MUTED));
    lines.push(label(
        &format!(
            "{} calls · {} in 60s",
            v.model_metrics.calls, metrics.attempts_last_minute
        ),
        TEXT,
    ));
    lines.push(label(
        &format!(
            "{} active · {} cancelled",
            metrics.active, metrics.cancelled
        ),
        MUTED,
    ));
    let failed_line = format!("{} failed", metrics.failed);
    let rate_line = if metrics.rate_limited > 0 {
        format!(" · {} 429", metrics.rate_limited)
    } else {
        String::new()
    };
    lines.push(Line::from(vec![
        Span::styled(
            format!("  {failed_line}"),
            Style::default().fg(if metrics.failed > 0 { RED } else { MUTED }),
        ),
        Span::styled(
            rate_line,
            Style::default().fg(if metrics.rate_limited > 0 { RED } else { MUTED }),
        ),
    ]));
    lines.push(label(
        &metrics
            .last_output_tokens_per_second
            .map(|n| format!("{n:.1} tok/s"))
            .unwrap_or_else(|| "tok/s · awaiting".into()),
        TEXT,
    ));
    lines.push(label(
        &metrics
            .cache_hit_percent()
            .map(|n| {
                format!(
                    "cache {:.1}% ({}/{})",
                    n, metrics.cache_reported_responses, metrics.completed
                )
            })
            .unwrap_or_else(|| "cache · not reported".into()),
        TEXT,
    ));
    lines.push(Line::default());
    // Tokens section.
    lines.push(label("Tokens · session", MUTED));
    lines.push(label(
        &format!(
            "{} in · {} out",
            tokens(v.usage.input_tokens),
            tokens(v.usage.output_tokens)
        ),
        TEXT,
    ));
    if let Some((pin, pout)) = v.pricing {
        let cost =
            (v.usage.input_tokens as f64 * pin + v.usage.output_tokens as f64 * pout) / 1_000_000.0;
        lines.push(label(&format!("${cost:.4} (${pin}/{pout} per M)"), YELLOW));
    }
    lines.push(label(&format!("{} responses", v.recorded_responses), MUTED));
    lines.push(Line::default());
    // Permissions.
    lines.push(label("Permissions", MUTED));
    lines.push(label(
        if m.yolo {
            "YOLO · approvals skipped"
        } else if m.trusted_shell {
            "Trusted local execution"
        } else {
            "Ask before commands"
        },
        if m.yolo { ACCENT } else { TEXT },
    ));
    lines.push(label(&format!("theme {}", v.theme.label()), MUTED));
    lines
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
            result.push(std::mem::take(&mut line));
            used = 0;
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
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(PANEL));
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(
        Paragraph::new(elide(&text, inner.width as usize)).style(Style::default().fg(MUTED)),
        inner,
    );
}

/// Breathing bar: spinner + activity + elapsed + Esc hint.
/// Rotating status phrases that make the wait feel like a conversation.
/// Indexed by elapsed seconds; cycles every ~8s so it never feels stale.
fn fun_phrase(v: &View, tick: u64) -> Option<&'static str> {
    if v.is_thinking() {
        // Thinking phase — the model is reasoning before producing output.
        const PHRASES: &[&str] = &[
            "正在思考",
            "梳理思路",
            "组织逻辑",
            "斟酌措辞",
            "推演方案",
            "检索知识",
            "权衡取舍",
            "灵感涌现",
        ];
        // Rotate every ~1.5s (25 ticks at 40ms).
        Some(PHRASES[((tick / 25) as usize) % PHRASES.len()])
    } else if v.is_responding() {
        // Responding phase — streaming tokens.
        const PHRASES: &[&str] = &["正在回复", "敲敲键盘", "奋笔疾书", "逐字输出", "整理答案"];
        Some(PHRASES[((tick / 30) as usize) % PHRASES.len()])
    } else {
        None
    }
}

fn draw_activity_bar(f: &mut Frame<'_>, area: Rect, v: &View, compact: bool, tick: u64) {
    let spinner = spinner_frame(tick);
    let mut act = activity(v, compact);
    // Replace the generic "Thinking"/"Responding" with rotating phrases.
    if let Some(phrase) = fun_phrase(v, tick) {
        act = phrase.into();
    }
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

/// Two-segment status bar (footer, left column bottom row).
/// Rendering inputs are kept explicit because they come from separate UI subsystems.
#[allow(clippy::too_many_arguments)]
fn draw_footer(
    f: &mut Frame<'_>,
    area: Rect,
    v: &View,
    m: &Metadata,
    status_text: &str,
    busy: bool,
    compact: bool,
    queued: usize,
    renderer: &mut Renderer,
) {
    let model_label = &v.model_label;
    let perm = permission_label(m.yolo, m.trusted_shell);
    // Right segments (right-to-left): permission, todo count, tokens, ctx bar.
    let metrics = &v.model_metrics.requests;
    let todo_seg = if !v.todos.is_empty() {
        let done = v.todos.iter().filter(|t| t.completed).count();
        format!(" · {}/{}", done, v.todos.len())
    } else {
        String::new()
    };
    let token_seg = format!(
        " · ↑{} ↓{}",
        tokens(v.usage.input_tokens),
        tokens(v.usage.output_tokens)
    );
    let ctx_seg = if let Some(ratio) = ctx_pressure(v) {
        format!(" {} {}%", ctx_bar(ratio), (ratio * 100.0) as u64)
    } else {
        String::new()
    };
    let rate_seg = if metrics.rate_limited > 0 {
        format!(" · 429×{}", metrics.rate_limited)
    } else {
        String::new()
    };
    let cost_seg = if let Some((pin, pout)) = v.pricing {
        let cost =
            (v.usage.input_tokens as f64 * pin + v.usage.output_tokens as f64 * pout) / 1_000_000.0;
        if cost >= 0.01 {
            format!(" · ${cost:.2}")
        } else if cost > 0.0 {
            format!(" · ${cost:.4}")
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    let right = format!("{ctx_seg}{rate_seg}{token_seg}{cost_seg}{todo_seg} · {perm}");
    let right_w = right.width();
    // Left segment.
    let left = if busy {
        let act = activity(v, compact);
        let elapsed = elapsed_str(v);
        let q = if queued > 0 {
            format!(" · {queued} queued")
        } else {
            String::new()
        };
        format!("● {act}{elapsed}{q}")
    } else {
        let branch = renderer.git_branch(&m.cwd);
        if branch.is_empty() {
            format!("{model_label} · {status_text}")
        } else {
            format!("{model_label} ·  {branch} · {status_text}")
        }
    };
    let left_color = if busy && v.asks_empty() {
        let seconds = renderer
            .animation_start
            .get_or_insert_with(std::time::Instant::now)
            .elapsed()
            .as_secs_f32();
        pulse_color(seconds, v.theme)
    } else {
        renderer.animation_start = None;
        TEXT
    };
    // Render: left (elided if needed), right (fixed).
    let avail = area.width as usize;
    let left_max = avail.saturating_sub(right_w + 2);
    let left_text = elide(&left, left_max);
    let left_w = left_text.width();
    let mut spans = vec![Span::styled(left_text, Style::default().fg(left_color))];
    if right_w + 2 <= avail {
        let pad = avail - left_w - right_w;
        spans.push(Span::raw(" ".repeat(pad)));
        // Color the ctx bar segment.
        let right_spans = if let Some(ratio) = ctx_pressure(v) {
            vec![
                Span::styled(ctx_bar(ratio), Style::default().fg(ctx_color(ratio))),
                Span::styled(
                    format!(" {}%", (ratio * 100.0) as u64),
                    Style::default().fg(ctx_color(ratio)),
                ),
                Span::styled(
                    right.trim_start_matches(['▓', '░', ' ']),
                    Style::default().fg(MUTED),
                ),
            ]
        } else {
            // ctx_seg is empty; just render right without the bar prefix.
            let rest = right.trim_start();
            vec![Span::styled(rest, Style::default().fg(MUTED))]
        };
        // Permission segment color is secondary for now; render right as MUTED.
        let _ = right_spans;
        spans.push(Span::styled(right.trim_start(), Style::default().fg(MUTED)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// ^B dashboard overlay (replaces the old sidebar content).
fn stats_overlay(f: &mut Frame<'_>, area: Rect, v: &View, m: &Metadata, queued: usize) {
    let _ = queued;
    let width = area.width.clamp(40, 64);
    let mut lines: Vec<Line<'static>> = vec![];
    // Session section.
    lines.push(label(&v.model_label, TEXT));
    lines.push(label(&format!("cwd {}", abbreviate_home(&m.cwd)), MUTED));
    lines.push(label(
        &format!(
            "session {} · {} calls",
            m.session.chars().take(8).collect::<String>(),
            v.model_metrics.calls
        ),
        MUTED,
    ));
    lines.push(Line::default());
    // Context section.
    lines.push(label("Context", MUTED));
    if let Some(usage) = &v.context_usage {
        let used = usage.estimated_tokens;
        if let Some(window) = usage.context_window.filter(|w| *w > 0) {
            let ratio = used as f64 / window as f64;
            lines.push(label(
                &format!(
                    "{} / {} ({:.0}%)",
                    tokens(used),
                    tokens(window),
                    ratio * 100.0
                ),
                TEXT,
            ));
            lines.push(label(&ctx_bar(ratio), ctx_color(ratio)));
        } else {
            lines.push(label(
                &format!("{} used · limit unknown", tokens(used)),
                TEXT,
            ));
        }
        if let Some(budget) = usage.input_budget {
            lines.push(label(
                &format!(
                    "{} input remaining of {} budget",
                    tokens(budget.saturating_sub(used)),
                    tokens(budget)
                ),
                MUTED,
            ));
        }
        lines.push(label(
            &format!("reserve {}", tokens(usage.output_reserve)),
            MUTED,
        ));
    } else {
        lines.push(label("Estimate unavailable", MUTED));
    }
    let metrics = &v.model_metrics.requests;
    if metrics.journal_errors > 0 {
        lines.push(label("Request log write failed", RED));
    }
    lines.push(Line::default());
    // Requests section.
    lines.push(label("Requests · this run", MUTED));
    lines.push(label(
        &format!(
            "{} calls · {} in last 60s",
            v.model_metrics.calls, metrics.attempts_last_minute
        ),
        TEXT,
    ));
    lines.push(label(
        &format!(
            "{} active · {} cancelled",
            metrics.active, metrics.cancelled
        ),
        MUTED,
    ));
    let failed_line = format!("{} failed", metrics.failed);
    let rate_line = if metrics.rate_limited > 0 {
        format!(" · {} HTTP 429", metrics.rate_limited)
    } else {
        String::new()
    };
    lines.push(Line::from(vec![
        Span::styled(
            format!("  {failed_line}"),
            Style::default().fg(if metrics.failed > 0 { RED } else { MUTED }),
        ),
        Span::styled(
            rate_line,
            Style::default().fg(if metrics.rate_limited > 0 { RED } else { MUTED }),
        ),
    ]));
    lines.push(label(
        &metrics
            .last_output_tokens_per_second
            .map(|n| format!("{n:.1} tok/s last response"))
            .unwrap_or_else(|| "tok/s · awaiting usage".into()),
        TEXT,
    ));
    lines.push(label(
        &metrics
            .cache_hit_percent()
            .map(|n| {
                format!(
                    "cache hit {:.1}% ({}/{})",
                    n, metrics.cache_reported_responses, metrics.completed
                )
            })
            .unwrap_or_else(|| "cache hit · not reported".into()),
        TEXT,
    ));
    lines.push(Line::default());
    // Tokens section.
    lines.push(label("Tokens · session", MUTED));
    lines.push(label(
        &format!(
            "{} in · {} out · {} total",
            tokens(v.usage.input_tokens),
            tokens(v.usage.output_tokens),
            tokens(v.usage.total_tokens)
        ),
        TEXT,
    ));
    if let Some((pin, pout)) = v.pricing {
        let cost =
            (v.usage.input_tokens as f64 * pin + v.usage.output_tokens as f64 * pout) / 1_000_000.0;
        lines.push(label(
            &format!("${cost:.4} est. cost (${pin}/{pout} per M)"),
            YELLOW,
        ));
    }
    lines.push(label(&format!("{} responses", v.recorded_responses), MUTED));
    lines.push(Line::default());
    // Permissions.
    lines.push(label("Permissions", MUTED));
    lines.push(label(
        if m.yolo {
            "YOLO · approvals skipped"
        } else if m.trusted_shell {
            "Trusted local execution"
        } else {
            "Ask before commands"
        },
        if m.yolo { ACCENT } else { TEXT },
    ));
    // Theme line.
    lines.push(label(&format!("theme {}", v.theme.label()), MUTED));
    let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(PANEL).fg(TEXT))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(ACCENT))
                    .title(" Session · esc / ^B "),
            ),
        rect,
    );
}

fn abbreviate_home(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if path.starts_with(&home) {
            return format!("~{}", &path[home.len()..]);
        }
    }
    path.to_string()
}

/// `/models` picker overlay.
fn model_picker_overlay(f: &mut Frame<'_>, area: Rect, v: &View) {
    let choices = &v.model_choices;
    if choices.is_empty() {
        return;
    }
    let selected = v.model_picker.unwrap_or(0);
    let current_label = &v.model_label;
    let rect = crate::picker::centered(area, 56, choices.len().saturating_add(2));
    f.render_widget(Clear, rect);
    let lines: Vec<Line<'static>> = choices
        .iter()
        .enumerate()
        .skip(
            crate::picker::visible_rows(choices.len(), selected, rect.height.saturating_sub(2))
                .start,
        )
        .take(usize::from(rect.height.saturating_sub(2)))
        .map(|(i, label)| {
            let is_current = label == current_label;
            let prefix = if i == selected { "► " } else { "  " };
            let mark = if is_current { "●" } else { "○" };
            let color = if i == selected { ACCENT } else { TEXT };
            Line::from(vec![
                Span::styled(prefix.to_owned(), Style::default().fg(ACCENT)),
                Span::styled(format!("{mark} {label}"), Style::default().fg(color)),
            ])
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(PANEL).fg(TEXT))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(ACCENT))
                    .title(" Models · ↑↓ · Enter · Esc "),
            ),
        rect,
    );
}

/// `/sessions` picker overlay. Rows are pre-loaded into the picker state;
/// filtering is computed per-frame via the shared `filter_sessions` helper.
fn sessions_overlay(f: &mut Frame<'_>, area: Rect, v: &View) {
    let Some(picker) = v.session_picker.as_ref() else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let filtered = crate::sessions::filter_sessions(&picker.rows, &picker.query);
    let rect = crate::picker::centered(area, 80, filtered.len().saturating_add(6).min(24));
    let width = rect.width;
    f.render_widget(Clear, rect);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let hint = if picker.query.is_empty() {
        "type to filter · ↑↓ move · Enter switch · Esc close"
    } else {
        ""
    };
    lines.push(Line::from(vec![
        Span::styled("filter ", Style::default().fg(MUTED)),
        Span::styled(picker.query.clone(), Style::default().fg(TEXT)),
        Span::styled(hint.to_owned(), Style::default().fg(MUTED)),
    ]));
    lines.push(Line::default());
    if filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            "No sessions match.",
            Style::default().fg(YELLOW),
        )));
    } else {
        let inner_w = (width.saturating_sub(4)) as usize;
        for rank in crate::picker::visible_rows(
            filtered.len(),
            picker.selected,
            rect.height.saturating_sub(4),
        ) {
            let idx = filtered[rank];
            let row = &picker.rows[idx];
            let is_selected = rank == picker.selected;
            let marker = if is_selected { "►" } else { " " };
            let current = if row.is_current { "●" } else { "○" };
            let title_w = inner_w.saturating_sub(36).clamp(8, 32);
            let title = elide(&row.title, title_w);
            let id8: String = row.id.0.chars().take(8).collect();
            let model = if row.model.is_empty() {
                "—".to_string()
            } else {
                elide(&row.model, 16)
            };
            let time = crate::sessions::relative_time(row.updated_at, now);
            let prefix = format!("{marker} {current} ");
            let body = format!("{title:<title_w$} {id8} · {model:<16} · {time}");
            let color = if is_selected {
                ACCENT
            } else if row.is_current {
                GREEN
            } else {
                TEXT
            };
            let style = if is_selected {
                Style::default().fg(color).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(color)
            };
            lines.push(Line::from(vec![
                Span::styled(prefix, Style::default().fg(ACCENT)),
                Span::styled(body, style),
            ]));
        }
    }
    f.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(PANEL).fg(TEXT))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(ACCENT))
                    .title(" Sessions · filter · ↑↓ · Enter · Ctrl-D del · Esc "),
            ),
        rect,
    );
}

/// `/theme` picker overlay: lists every theme in `Theme::ALL` with the
/// currently active one marked. Enter applies immediately (live preview).
fn theme_picker_overlay(f: &mut Frame<'_>, area: Rect, v: &View) {
    let all = super::theme::Theme::ALL;
    let selected = v.theme_picker.unwrap_or(0);
    let rect = crate::picker::centered(area, 40, all.len().saturating_add(2));
    f.render_widget(Clear, rect);
    let lines: Vec<Line<'static>> = all
        .iter()
        .enumerate()
        .skip(crate::picker::visible_rows(all.len(), selected, rect.height.saturating_sub(2)).start)
        .take(usize::from(rect.height.saturating_sub(2)))
        .map(|(i, t)| {
            let is_current = *t == v.theme;
            let is_selected = i == selected;
            let prefix = if is_selected { "► " } else { "  " };
            let mark = if is_current { "●" } else { "○" };
            let label = t.label();
            let color = if is_selected {
                ACCENT
            } else if is_current {
                GREEN
            } else {
                TEXT
            };
            let style = if is_selected {
                Style::default().fg(color).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(color)
            };
            Line::from(vec![
                Span::styled(prefix.to_owned(), Style::default().fg(ACCENT)),
                Span::styled(format!("{mark} {label}"), style),
            ])
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(PANEL).fg(TEXT))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(ACCENT))
                    .title(" Themes · ↑↓ · Enter · Esc "),
            ),
        rect,
    );
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
    f.render_widget(Paragraph::new("Enter          Send / steer; confirm reply\nCtrl-J/Alt-Enter  Newline (paste preserves newlines)\nArrows/Home/End  Move cursor; Backspace/Delete\nCtrl-A/E/B/F   Line start/end · char back/fwd\nCtrl-W/U/K     Del word · to line start/end\nAlt-B/F/D·Ctrl-Left/Right  Word move · del word\nUp/Down·Ctrl-P/N  History (or row move in multiline)\nPgUp / PgDn     Scroll conversation\nCtrl-End        Follow newest output\n/               Command menu · Up/Down · Tab/Enter\nF6/Shift-F6·Click  Select next/prev · expand block\nCtrl-O / Ctrl-R  Toggle selected block / thinking\nCtrl-T          Toggle right sidebar (Todo + telemetry)\nCtrl-B          Toggle stats dashboard overlay\nCtrl-Y          Cycle color theme\nCtrl-V          Paste image from clipboard (Esc clears)\n@               Reference a file (text inlined; images/PDF attached)\nMouse drag      Release to copy automatically\nEsc / Ctrl-C    Cancel exec / clear selection / close\nAlt-PgUp/PgDn   Scroll approval details\nCtrl-Q          Quit\n\n/queue TEXT     Schedule a follow-up turn\n/compact        Compact idle conversation\n/clear          Clear screen only (history unchanged)\n/theme          Theme picker (or /theme NAME)\n/models         Switch model (picker or /models p/m [variant])\n/sessions       List and switch sessions (Ctrl-D deletes)\n/status         Same as Ctrl-B dashboard\n/help           This help · Esc closes\n\nApprovals: y/n + Enter. No automatic or permanent grants.").style(Style::default().bg(PANEL).fg(TEXT)).block(Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).border_style(Style::default().fg(ACCENT)).title(" Help ")),rect);
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
    timeline_at(v, width, 3, 0)
}

fn timeline_at(
    v: &View,
    width: usize,
    edit_preview_rows: usize,
    tick: u64,
) -> (Vec<Line<'static>>, Vec<(usize, u64)>) {
    let mut lines = Vec::new();
    let mut headers = Vec::new();
    for (index, item) in v.items().iter().enumerate() {
        let id = v.item_id(index);
        let expanded = v.expanded(id);
        let selected = v.selected() == Some(id);
        match item {
            Item::Text {
                role: Role::Thinking,
                text,
            } => {
                headers.push((lines.len(), id));
                let chars = text.chars().count();
                let size = if chars >= 1000 {
                    format!("{:.1}K", chars as f64 / 1000.0)
                } else {
                    format!("{chars}")
                };
                let first_line = text
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .chars()
                    .take(40)
                    .collect::<String>();
                let is_running = v.is_thinking_at(index);
                let glyph = "✦";
                let color = if selected || is_running {
                    ACCENT
                } else {
                    MUTED
                };
                let title = if first_line.is_empty() {
                    format!("┃ {glyph} Thinking · {size}")
                } else {
                    format!("┃ {glyph} Thinking · {size} · {first_line}")
                };
                lines.push(Line::from(Span::styled(
                    title,
                    Style::default().fg(color).add_modifier(Modifier::ITALIC),
                )));
                if expanded {
                    let dim = |line: Line<'static>| {
                        Line::from(
                            line.spans
                                .into_iter()
                                .map(|s| {
                                    Span::styled(
                                        s.content,
                                        s.style.fg(MUTED).add_modifier(Modifier::ITALIC),
                                    )
                                })
                                .collect::<Vec<_>>(),
                        )
                    };
                    lines.extend(super::markdown::render(text, width).into_iter().map(dim));
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
                lines.push(tool_title(t, selected, width, tick));
                if expanded {
                    tool_expanded(t, &mut lines, width, v.theme);
                } else {
                    tool_preview(t, &mut lines, width, v.theme, edit_preview_rows);
                }
            }
        }
        lines.push(Line::default());
    }
    (lines, headers)
}
/// Card title: "{▸ cursor} {status glyph} {subject}{pad}{meta}". The glyph
/// carries the status color; the subject is bright and is the only elidable
/// span; the right-hand meta always survives intact.
fn tool_title(
    t: &super::state::ToolView,
    selected: bool,
    width: usize,
    tick: u64,
) -> Line<'static> {
    let (glyph, color) = match t.status {
        ToolStatus::Running => (spinner_frame(tick).to_string(), ACCENT),
        ToolStatus::Done => ("✓".into(), GREEN),
        ToolStatus::Failed => ("✗".into(), RED),
        ToolStatus::Interrupted => ("■".into(), MUTED),
    };
    let mut meta = tool_meta(t);
    // Sub-second runs stay silent: "0s" is pure noise on fast tools.
    if let Some(n) = t.seconds.filter(|n| *n > 0) {
        if !meta.is_empty() {
            meta.push_str(" · ");
        }
        meta.push_str(&format!("{n}s"));
    }
    let subject = t
        .summary
        .lines()
        .next()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(t.name.as_str());
    // cursor(2) + glyph+space(2) + ≥2 pad + meta; subject gets the remainder.
    let budget = width.saturating_sub(6 + meta.width()).max(4);
    let subject = elide(subject, budget);
    let pad = width
        .saturating_sub(4 + subject.width() + meta.width())
        .max(2);
    Line::from(vec![
        Span::styled(
            if selected { "▸ " } else { "  " },
            Style::default().fg(ACCENT),
        ),
        Span::styled(format!("{glyph} "), Style::default().fg(color)),
        Span::styled(subject, Style::default().fg(TEXT)),
        Span::raw(" ".repeat(pad)),
        Span::styled(meta, Style::default().fg(FAINT)),
    ])
}

/// Right-hand title metadata, from structured state only (never parsed text).
fn tool_meta(t: &super::state::ToolView) -> String {
    match t.name.as_str() {
        "shell" if t.status != ToolStatus::Running => {
            t.exit_code.map(|c| format!("exit {c}")).unwrap_or_default()
        }
        "edit" if t.status != ToolStatus::Running => match (t.adds, t.dels) {
            (Some(a), Some(d)) => format!("+{a} −{d}"),
            _ => String::new(),
        },
        // write knows its line count up front (content is in the input);
        // "new" only once the server confirmed creation.
        "write" => {
            let mut meta = t.adds.map(|a| format!("+{a}")).unwrap_or_default();
            if t.created && t.status == ToolStatus::Done {
                if !meta.is_empty() {
                    meta.push_str(" · ");
                }
                meta.push_str("new");
            }
            meta
        }
        "read" if t.status != ToolStatus::Running => {
            format!("{} lines", t.output.lines().count())
        }
        "websearch" if t.status != ToolStatus::Running && !t.brief.is_empty() => {
            format!("{} results", t.brief.len())
        }
        "webfetch" if t.status != ToolStatus::Running => {
            t.content_format.as_deref().unwrap_or("text").to_owned()
        }
        _ => String::new(),
    }
}

/// "⋯ N more · ^O" footer shared by previews.
fn more_hint(lines: &mut Vec<Line<'static>>, hidden: usize, width: usize) {
    if hidden > 0 {
        lines.extend(wrap(
            &format!("⋯ {hidden} more · ^O"),
            Style::default().fg(FAINT),
            width,
            "  ",
        ));
    }
}

/// One diff row: + green / − red / context muted, tinted full-width bg;
/// file/hunk headers stay quiet.
fn diff_line(line: &str, width: usize, prefix: &str) -> Vec<Line<'static>> {
    let (fg, bg) = if line.starts_with("++") || line.starts_with("--") || line.starts_with("@@") {
        (FAINT, BG)
    } else if line.starts_with('+') {
        (GREEN, DIFF_ADD_BG)
    } else if line.starts_with('-') {
        (RED, DIFF_DEL_BG)
    } else {
        (MUTED, BG)
    };
    wrap_bg(line, Style::default().fg(fg).bg(bg), width, prefix)
}

/// Width budget above which diffs render side-by-side (opencode's <diff>
/// component uses the same 120-column threshold).
const SPLIT_DIFF_MIN_WIDTH: usize = 120;

/// Render parsed diff rows width-adaptively: side-by-side columns at ≥120
/// cols, unified −/+ lines below. Hunk headers only appear in expanded bodies
/// (previews stay dense). Returns the lines plus how many changed lines were
/// rendered (drives the "⋯ N more" hint).
fn diff_render(
    rows: &[DiffRow],
    width: usize,
    theme: Theme,
    max_rows: usize,
    show_hunks: bool,
) -> (Vec<Line<'static>>, usize) {
    fn hunk_line(h: &str, width: usize) -> Vec<Line<'static>> {
        wrap(h, Style::default().fg(FAINT), width, "  ")
    }
    let mut out = Vec::new();
    let (mut shown, mut changes) = (0usize, 0usize);
    for row in rows {
        if let Some(h) = &row.hunk {
            if show_hunks {
                out.extend(hunk_line(h, width));
            }
            continue;
        }
        if shown >= max_rows {
            break;
        }
        if width >= SPLIT_DIFF_MIN_WIDTH {
            // Side-by-side: context spans the row, changes get two cells.
            if let Some(ctx) = &row.ctx {
                out.push(clipped_plain_line(ctx, width, "    "));
            } else if row.old.is_some() || row.new.is_some() {
                let cw = (width.saturating_sub(3)) / 2;
                let mut spans = diff_cell(cw, &row.old, DIFF_DEL_BG, theme);
                spans.push(Span::styled(" │ ", Style::default().fg(FAINT)));
                spans.extend(diff_cell(cw, &row.new, DIFF_ADD_BG, theme));
                out.push(Line::from(spans));
            }
        } else {
            // Unified: sign gutter + tinted full-width lines.
            if let Some(ctx) = &row.ctx {
                out.push(clipped_plain_line(ctx, width, "  "));
            } else {
                if let Some((_, spans)) = &row.old {
                    out.push(unified_change_line('-', spans, DIFF_DEL_BG, width, theme));
                }
                if let Some((_, spans)) = &row.new {
                    out.push(unified_change_line('+', spans, DIFF_ADD_BG, width, theme));
                }
            }
        }
        shown += 1;
        changes += row.changes();
    }
    (out, changes)
}

/// One split-view cell, exactly `cw` columns: " 41 " gutter + subdued syntax
/// text on the tinted bg. A missing side is an untinted gap (vimdiff look).
fn diff_cell(
    cw: usize,
    side: &Option<(usize, Vec<Span<'static>>)>,
    tint: Color,
    theme: Theme,
) -> Vec<Span<'static>> {
    let bg = if side.is_some() { tint } else { BG };
    let mut out = Vec::new();
    let mut used = 0usize;
    if let Some((no, spans)) = side {
        let gutter = format!("{no:>3} ");
        out.push(Span::styled(
            gutter.clone(),
            Style::default().fg(FAINT).bg(bg),
        ));
        used += gutter.width();
        let subtle: Vec<Span> = spans.iter().map(|s| subtle_span(theme, s, bg)).collect();
        let body = clip_spans(
            &subtle,
            cw.saturating_sub(used + 1),
            Style::default().fg(FAINT).bg(bg),
        );
        used += spans_width(&body);
        out.extend(body);
    }
    out.push(Span::styled(
        " ".repeat(cw.saturating_sub(used)),
        Style::default().bg(bg),
    ));
    debug_assert_eq!(spans_width(&out), cw);
    out
}

/// Unified-view change line: "  − " sign + subdued text, full-width tint.
fn unified_change_line(
    sign: char,
    spans: &[Span<'static>],
    tint: Color,
    width: usize,
    theme: Theme,
) -> Line<'static> {
    let gutter = format!("  {sign} ");
    let mut out = vec![Span::styled(
        gutter.clone(),
        Style::default()
            .fg(if sign == '+' { GREEN } else { RED })
            .bg(tint),
    )];
    let subtle: Vec<Span> = spans.iter().map(|s| subtle_span(theme, s, tint)).collect();
    let body = clip_spans(
        &subtle,
        width.saturating_sub(gutter.width() + 1),
        Style::default().fg(FAINT).bg(tint),
    );
    let used = gutter.width() + spans_width(&body);
    out.extend(body);
    out.push(Span::styled(
        " ".repeat(width.saturating_sub(used)),
        Style::default().bg(tint),
    ));
    Line::from(out)
}

/// Context/plain row: straight syntax colors, ellipsized to `width`.
fn clipped_plain_line(ctx: &[Span<'static>], width: usize, prefix: &'static str) -> Line<'static> {
    let mut out = vec![Span::raw(prefix)];
    let body = clip_spans(
        ctx,
        width.saturating_sub(prefix.width()),
        Style::default().fg(FAINT),
    );
    out.extend(body);
    Line::from(out)
}

/// Syntax color softened toward TEXT so it sits calmly on a diff tint —
/// opencode's `generateSubtleSyntax` trick. Mixed from *themed* colors, so
/// the result is intentionally a non-slot color that Theme::apply skips.
fn subtle_span(theme: Theme, span: &Span<'static>, bg: Color) -> Span<'static> {
    let fg = mix(
        theme.color(span.style.fg.unwrap_or(TEXT)),
        theme.color(TEXT),
        0.45,
    );
    Span::styled(span.content.clone(), Style::default().fg(fg).bg(bg))
}

fn spans_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

/// Clip spans to ≤`budget` columns (grapheme-safe, adjacent same-style
/// graphemes merged); on truncation the tail carries `ellipsis_style` "…".
fn clip_spans(spans: &[Span<'static>], budget: usize, ellipsis_style: Style) -> Vec<Span<'static>> {
    let (kept, cut) = truncate_spans(spans, budget);
    if !cut {
        return kept;
    }
    let (mut kept, _) = truncate_spans(spans, budget.saturating_sub(1));
    kept.push(Span::styled("…", ellipsis_style));
    kept
}

/// Grapheme-safe span truncation; second return = something was dropped.
fn truncate_spans(spans: &[Span<'static>], budget: usize) -> (Vec<Span<'static>>, bool) {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for span in spans {
        for g in span.content.graphemes(true) {
            if used + g.width() > budget {
                return (out, true);
            }
            if let Some(last) = out
                .last_mut()
                .filter(|s: &&mut Span<'static>| s.style == span.style)
            {
                last.content.to_mut().push_str(g);
            } else {
                out.push(Span::styled(g.to_owned(), span.style));
            }
            used += g.width();
        }
    }
    (out, false)
}

/// Expanded card body (^O): input block, then the tool-specific full result.
fn tool_expanded(
    t: &super::state::ToolView,
    lines: &mut Vec<Line<'static>>,
    width: usize,
    theme: Theme,
) {
    let prefix = "  ";
    // edit/write/webfetch already have fully structured bodies; repeating their
    // often-large JSON arguments before the useful content only adds noise.
    if !matches!(t.name.as_str(), "edit" | "write" | "webfetch") {
        for line in t.input.lines() {
            lines.extend(wrap(line, Style::default().fg(MUTED), width, prefix));
        }
    }
    if t.status == ToolStatus::Running {
        for line in t.progress.lines() {
            lines.extend(wrap(
                line,
                Style::default().fg(MUTED).italic(),
                width,
                prefix,
            ));
        }
        if t.name != "write" {
            return;
        }
    }
    match t.name.as_str() {
        "read" | "write" if t.content_hl.is_some() => {
            for line in t.content_hl.as_ref().unwrap() {
                lines.extend(super::markdown::wrap_spans(
                    line.spans.clone(),
                    width,
                    prefix,
                ));
            }
        }
        "edit" => match &t.diff_rows {
            Some(rows) => {
                let (rendered, _) = diff_render(rows, width, theme, usize::MAX, true);
                lines.extend(rendered);
            }
            None => {
                for line in t.output.lines() {
                    lines.extend(diff_line(line, width, prefix));
                }
            }
        },
        "webfetch" => {
            // Highlighted structured content (json/xml/html) renders like code;
            // markdown/text pages stream as wrapped body lines.
            if let Some(hl) = &t.content_hl {
                for line in hl {
                    lines.extend(super::markdown::wrap_spans(
                        line.spans.clone(),
                        width,
                        prefix,
                    ));
                }
            } else {
                for line in t.output.lines() {
                    lines.extend(wrap(line, Style::default().fg(TEXT), width, prefix));
                }
            }
        }
        "shell" => {
            for line in t.output.lines() {
                lines.extend(wrap(line, Style::default().fg(TEXT), width, prefix));
            }
            if !t.stderr.trim().is_empty() {
                lines.extend(wrap(
                    "── stderr ──",
                    Style::default().fg(FAINT),
                    width,
                    prefix,
                ));
                for line in t.stderr.lines() {
                    lines.extend(wrap(line, Style::default().fg(MUTED), width, prefix));
                }
            }
        }
        _ => {
            for line in t.output.lines() {
                lines.extend(wrap(line, Style::default().fg(MUTED), width, prefix));
            }
        }
    }
}

/// Type-aware card preview with per-tool line quotas: read renders nothing,
/// shell keeps one tail line, edit/write keep the first change rows,
/// websearch lists sources. Everything else is at most one summary line.
fn tool_preview(
    t: &super::state::ToolView,
    lines: &mut Vec<Line<'static>>,
    width: usize,
    theme: Theme,
    edit_preview_rows: usize,
) {
    let prefix = "  ";
    match t.name.as_str() {
        "shell" if t.status == ToolStatus::Running => {
            // Last 3 progress lines, following scroll.
            let prog: Vec<&str> = t.progress.lines().collect();
            if prog.is_empty() {
                lines.extend(wrap(
                    "waiting for output…",
                    Style::default().fg(MUTED),
                    width,
                    prefix,
                ));
            } else {
                for line in &prog[prog.len().saturating_sub(3)..] {
                    lines.extend(wrap(line, Style::default().fg(CYAN), width, prefix));
                }
            }
        }
        "shell" => {
            // Preview source: stdout, falling back to stderr (many CLIs print
            // their headline to stderr).
            let out: Vec<&str> = if t.output.trim().is_empty() {
                t.stderr.lines().collect()
            } else {
                t.output.lines().collect()
            };
            if t.status == ToolStatus::Failed {
                for line in out.iter().take(5) {
                    lines.extend(wrap(line, Style::default().fg(RED), width, prefix));
                }
                more_hint(lines, out.len().saturating_sub(5), width);
            } else {
                let start = out.len().saturating_sub(1);
                for line in &out[start..] {
                    lines.extend(wrap(line, Style::default().fg(MUTED), width, prefix));
                }
                if out.is_empty() {
                    lines.extend(wrap("no output", Style::default().fg(MUTED), width, prefix));
                }
            }
        }
        "edit" if t.status != ToolStatus::Running => match &t.diff_rows {
            Some(rows) if !rows.is_empty() => {
                let (rendered, shown) =
                    diff_render(rows, width, theme, edit_preview_rows.max(1), false);
                lines.extend(rendered);
                let total = t.adds.unwrap_or(0) + t.dels.unwrap_or(0);
                more_hint(lines, total.saturating_sub(shown), width);
            }
            Some(_) => {
                lines.extend(wrap(
                    "no changes",
                    Style::default().fg(MUTED),
                    width,
                    prefix,
                ));
            }
            // Fallback for cards without a parsed diff (orphaned events).
            None => {
                let rows: Vec<&str> = t
                    .output
                    .lines()
                    .filter(|l| {
                        (l.starts_with('+') && !l.starts_with("+++"))
                            || (l.starts_with('-') && !l.starts_with("---"))
                            || l.starts_with(' ')
                    })
                    .take(4)
                    .collect();
                if rows.is_empty() {
                    lines.extend(wrap(
                        "no changes",
                        Style::default().fg(MUTED),
                        width,
                        prefix,
                    ));
                } else {
                    for line in &rows {
                        lines.extend(diff_line(line, width, prefix));
                    }
                }
            }
        },
        "write" => {
            // Ghost-diff of what is (about to be) written; highlighted at
            // ToolStarted, so this also streams while running.
            if let Some(hl) = &t.content_hl {
                for line in hl.iter().take(2) {
                    lines.extend(super::markdown::wrap_spans(
                        line.spans.clone(),
                        width,
                        prefix,
                    ));
                }
                more_hint(lines, hl.len().saturating_sub(2), width);
            }
        }
        // read: the title already says what was read; a content teaser is noise.
        "read" => {}
        "websearch" if !t.brief.is_empty() => {
            for row in t.brief.iter().take(3) {
                lines.extend(wrap(row, Style::default().fg(MUTED), width, "  ⏤ "));
            }
            more_hint(lines, t.brief.len().saturating_sub(3), width);
        }
        "webfetch" if !t.brief.is_empty() => {
            for row in t.brief.iter().take(3) {
                lines.extend(wrap(row, Style::default().fg(MUTED), width, "  "));
            }
            more_hint(lines, t.brief.len().saturating_sub(3), width);
        }
        _ => {
            if let Some(line) = t.output.lines().find(|l| !l.trim().is_empty()) {
                lines.extend(wrap(line, Style::default().fg(MUTED), width, prefix));
            }
        }
    }
}

/// Like wrap() but applies bg color to the full line width (for diff backgrounds).
fn wrap_bg(text: &str, style: Style, width: usize, prefix: &str) -> Vec<Line<'static>> {
    let available = width.saturating_sub(prefix.width()).max(2);
    let mut result = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    for g in text.graphemes(true) {
        if used + g.width() > available && !line.is_empty() {
            let pad = available.saturating_sub(used);
            result.push(Line::from(Span::styled(
                format!("{prefix}{line}{}", " ".repeat(pad)),
                style,
            )));
            line.clear();
            used = 0;
        }
        line.push_str(g);
        used += g.width();
    }
    let pad = available.saturating_sub(used);
    result.push(Line::from(Span::styled(
        format!("{prefix}{line}{}", " ".repeat(pad)),
        style,
    )));
    result
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
    #[test]
    fn pickers_fit_small_terminals_and_scroll_to_selected_rows() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut v = View::default();
        v.model_choices = (0..100).map(|i| format!("model-{i:03}")).collect();
        v.model_picker = Some(99);
        v.theme_picker = Some(super::super::theme::Theme::ALL.len() - 1);
        v.session_picker = Some(super::super::state::SessionPickerState {
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
                model_picker_overlay as fn(&mut ratatui::Frame<'_>, Rect, &View),
                "model-099",
            ),
            (sessions_overlay, "row-099"),
        ] {
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

    #[test]
    fn running_tool_shows_braille_spinner_glyph() {
        let mut v = View::default();
        v.event(Out::ToolStarted {
            id: "t".into(),
            name: "shell".into(),
            input: json!({"command":"sleep 1"}),
        });
        // A running card title carries one of the braille spinner frames.
        let (lines, _) = timeline_at(&v, 80, 3, 0);
        let text = timeline_text(&lines);
        assert!(
            SPINNER.iter().any(|c| text.contains(*c)),
            "spinner frame visible: {text}"
        );
        assert!(
            !text.contains('●'),
            "static dot replaced by spinner: {text}"
        );

        // Done cards still use the static checkmark.
        let mut v = View::default();
        v.event(Out::ToolStarted {
            id: "t".into(),
            name: "shell".into(),
            input: json!({"command":"echo hi"}),
        });
        v.event(Out::ToolDone {
            id: "t".into(),
            name: "shell".into(),
            output: json!({"ok":true,"exit_code":0,"stdout":"hi\n","output_complete":true,"stderr":""}),
            is_error: false,
        });
        let (lines, _) = timeline(&v, 80);
        let text = timeline_text(&lines);
        assert!(text.contains('✓'), "done glyph: {text}");
    }

    #[test]
    fn webfetch_preview_and_meta_render() {
        let mut v = View::default();
        v.event(Out::ToolStarted {
            id: "f".into(),
            name: "webfetch".into(),
            input: json!({"url":"https://example.com"}),
        });
        v.event(Out::ToolDone {
            id: "f".into(),
            name: "webfetch".into(),
            output: json!({"ok":true,"format":"markdown","content":"# Title One\nFirst line\nSecond line\nThird line\nFourth line"}),
            is_error: false,
        });
        let (lines, _) = timeline(&v, 80);
        let text = timeline_text(&lines);
        // The format shows up as title metadata.
        assert!(text.contains("markdown"), "format meta: {text}");
        // Preview shows up to three brief lines, not the full page.
        assert!(text.contains("Title One"), "preview body: {text}");
        assert!(!text.contains("Fourth line"), "fourth line hidden: {text}");
    }

    #[test]
    fn edit_preview_quota_adapts_to_terminal_height() {
        // Short terminals show fewer preview rows; tall ones show more.
        let quota_short = edit_preview_quota(10);
        let quota_tall = edit_preview_quota(60);
        assert!(quota_short < quota_tall, "{quota_short} < {quota_tall}");
        assert!(quota_tall >= 3, "tall quota: {quota_tall}");

        // And rendering actually uses the quota: more change lines appear on a
        // tall terminal than on a short one.
        let mut v = View::default();
        v.event(Out::ToolStarted {
            id: "e".into(),
            name: "edit".into(),
            input: json!({"path":"f.rs"}),
        });
        v.event(Out::ToolDone {
            id: "e".into(),
            name: "edit".into(),
            output: json!({"ok":true,"path":"f.rs","changed":true,"diff":"--- f.rs\n+++ f.rs\n@@ -1,5 +1,6 @@\n a\n-b1\n+b2\n b3\n b4\n b5\n"}),
            is_error: false,
        });
        let (lines_short, _) = timeline_at(&v, 80, edit_preview_quota(10), 0);
        let (lines_tall, _) = timeline_at(&v, 80, edit_preview_quota(60), 0);
        let text_short = timeline_text(&lines_short);
        let text_tall = timeline_text(&lines_tall);
        assert!(
            text_tall.len() >= text_short.len(),
            "tall shows at least as much"
        );
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
            v.stats = true;
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
            assert!(text.contains("32.0K / 128.0K"));
            assert!(text.contains("8 calls"));
            assert!(text.contains("3 failed"));
            assert!(text.contains("47.5 tok/s"));
            assert!(text.contains("cache hit"));
            v.stats = false;
            // Theme cycling via ^Y still works (footer button removed).
            v.theme = v.theme.next();
            assert_ne!(v.theme, theme);
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
        v.stats = true;
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
        // Welcome now shows neofetch-style logo + info (no robot).
        assert!(screen.iter().any(|r| r.contains("yourai")));
        assert!(screen[31].contains("test-model"));

        assert!(!v.stats);
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
        assert!(text.contains("✦ Fix parser crash"));
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
                // Card preview shows output by design (§4.3); full input only when expanded.
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
        v.session_picker = Some(SessionPickerState {
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
        v.session_picker.as_mut().unwrap().query = "parser".into();
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
        v.theme_picker = Some(0);
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
        v.help = true;
        frame!("help overlay opens", SessionStatus::Idle);
        v.help = false;
        frame!("help overlay closes", SessionStatus::Idle);
        assert_eq!(prev.unwrap().area, Rect::new(0, 0, w, h));
    }
}
