use super::{frame_time::FrameTime, presentation::Canvas};
mod cards;
mod overlays;
mod timeline;
use super::overlay::Overlay;
use super::{
    editor::Editor,
    state::{Item, ToolStatus, View},
    theme::{
        lerp_color, Theme, ACCENT, BG, BLUE, BORDER, CODE_SURFACE, FOCUS_SURFACE, GREEN, MUTED,
        PANEL, RED, TEXT, YELLOW,
    },
};
use crate::text::{elide, elide_tail};
use cards::tool_title_row;
use overlays::{
    ask_overlay, model_picker_overlay, sessions_overlay, stats_overlay, theme_picker_overlay,
};
use ratatui::{
    prelude::*,
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
#[cfg(test)]
use yourai_core::prelude::Out;
#[cfg(test)]
use yourai_core::prelude::SessionStatus;

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
    /// One frame's resolved layout: the rects, the laid-out lines and the hit
    /// regions of the last prepared frame. Input-time queries (hit tests,
    /// selection anchors, scroll clamps, turn jumps) read this snapshot; the
    /// cache and the animation clock stay on the renderer.
    layout: Layout,
    /// Animation is clock-driven, independent of input/preparation frequency.
    animation_start: Option<std::time::Instant>,
    tick: u64,
}
#[derive(Default)]
struct Layout {
    /// Transcript viewport: the reading column, the native-scroll window and
    /// the anchor row base.
    transcript: Rect,
    /// The stacked rows: [transcript, ask, activity, todo dock, input].
    rows: Vec<Rect>,
    /// Todo sidebar rect when the wide layout shows it.
    panel: Option<Rect>,
    lines: timeline::LayoutLines,
    headers: Vec<(usize, u64)>,
    turns: Vec<(usize, u64)>,
    hits: Vec<(Rect, u64)>,
    command_hits: Vec<(Rect, super::commands::Command)>,
    command_area: Option<Rect>,
    follow_hit: Option<Rect>,
    todo_hit: Option<Rect>,
    todo_area: Option<Rect>,
}
/// What a click on the last painted frame hit. The renderer answers queries
/// about its own layout; the resulting View mutations live in the action
/// layer (`app::click_dispatch`), keeping input state out of the render path.
pub(crate) enum Hit<'a> {
    /// The "↓ Latest" pill shown above the composer while scrolled up.
    FollowLatest,
    /// A row of the slash-command menu.
    Command(&'a super::commands::Command),
    /// The Todo panel title (same as ^T).
    TodoToggle,
    /// The header line of a foldable block (tool card or thinking).
    Block(u64),
}

impl Renderer {
    pub fn begin_selection(&mut self, x: u16, y: u16) {
        let point = Position::new(x, y);
        let region = if self.layout.transcript.contains(point) {
            Some(self.layout.transcript)
        } else if self.layout.panel.is_some_and(|r| r.contains(point)) {
            self.layout.panel
        } else {
            // Title/border rows and the footer: select from the whole screen.
            self.selection.screen.as_ref().map(|b| b.area)
        };
        self.selection.begin(point, region.unwrap_or_default());
    }

    /// Resolve a click against the last painted frame's hit regions.
    pub fn hit_test(&self, x: u16, y: u16) -> Option<Hit<'_>> {
        let point = Position::new(x, y);
        if self
            .layout
            .follow_hit
            .is_some_and(|rect| rect.contains(point))
        {
            return Some(Hit::FollowLatest);
        }
        if self.layout.command_area.is_some_and(|r| r.contains(point)) {
            // Rows only; the menu chrome swallows clicks so they do not fall
            // through to the transcript underneath.
            return self
                .layout
                .command_hits
                .iter()
                .find(|(r, _)| r.contains(point))
                .map(|(_, c)| Hit::Command(c));
        }
        if self.layout.todo_hit.is_some_and(|r| r.contains(point)) {
            return Some(Hit::TodoToggle);
        }
        self.layout
            .hits
            .iter()
            .find(|(r, _)| r.contains(point))
            .map(|(_, id)| Hit::Block(*id))
    }

    /// Remember the screen row a block header was clicked at, so the next
    /// frame pins that line back where the user grabbed it.
    pub fn anchor(&self, v: &mut View, id: u64, y: u16) {
        v.navigation
            .anchor(id, y.saturating_sub(self.layout.transcript.y) as usize);
    }

    /// Whether a wheel event at (x, y) belongs to the Todo panel.
    pub fn wheel_on_todo(&self, x: u16, y: u16, panel_visible: bool) -> bool {
        panel_visible
            && self
                .layout
                .todo_area
                .is_some_and(|r| r.contains(Position::new(x, y)))
    }

    /// Transcript wheel: three rows per tick, matching codex and opencode —
    /// one row per event reads as laggy crawling on remote links even when
    /// every frame lands on time. Clamped to the current layout.
    pub fn scroll(&self, v: &mut View, rows: usize, up: bool) {
        v.navigation.scroll(
            rows,
            up,
            self.layout
                .lines
                .len()
                .saturating_sub(self.layout.transcript.height as usize),
        );
    }

    pub fn latest_turn(&self, v: &mut View) {
        v.navigation
            .turn(self.layout.turns.last().map(|(_, id)| *id));
    }
    pub fn jump_turn(&self, v: &mut View, previous: bool) {
        v.navigation.jump(
            &self.layout.turns,
            self.layout.lines.len(),
            self.layout.transcript.height as usize,
            previous,
        );
    }

    /// Resolve layout-dependent interaction state in memory, then return the
    /// complete screen. Presentation decides whether any terminal I/O is owed.
    pub fn prepare(
        &mut self,
        area: Rect,
        v: &mut View,
        m: &Metadata,
        time: FrameTime,
        queued: usize,
        compact: bool,
    ) -> Canvas {
        let mut canvas = Canvas::new(area);
        self.compose(&mut canvas, v, m, time, queued, compact);
        canvas
    }

    #[cfg(test)]
    pub fn draw(
        &mut self,
        f: &mut ratatui::Frame<'_>,
        v: &mut View,
        m: &Metadata,
        _status: &SessionStatus,
        queued: usize,
        compact: bool,
    ) {
        self.prepare(f.area(), v, m, FrameTime::now(), queued, compact)
            .paint(f);
    }

    /// Prepare one frame in three stages: relayout (rects + cache + reading
    /// anchor), navigation resolution against the new layout, then paint.
    fn compose(
        &mut self,
        f: &mut Canvas,
        v: &mut View,
        m: &Metadata,
        time: FrameTime,
        queued: usize,
        compact: bool,
    ) {
        let area = f.area();
        self.layout.hits.clear();
        self.layout.command_hits.clear();
        self.layout.command_area = None;
        self.layout.follow_hit = None;
        self.layout.todo_hit = None;
        self.layout.todo_area = None;
        let start = *self.animation_start.get_or_insert(time.monotonic);
        self.tick = time.monotonic.saturating_duration_since(start).as_millis() as u64 / 100;
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
        // Stage 1: resolve the layout. The only View write is preserving the
        // reading anchor across a relayout, which needs the old and new
        // layout at once.
        self.relayout(area, v, compact);
        // The native-scroll window rides on the canvas; Presentation hands it
        // to the terminal backend. The renderer never touches terminal state.
        f.set_scroll_region(self.layout.transcript);
        // Stage 2: navigation — pending intents resolve against the new
        // layout. This and the anchor above are the only View writes in the
        // prepare path.
        v.navigation.resolve(
            self.layout.lines.len(),
            self.layout.transcript.height as usize,
            &self.layout.headers,
            &self.layout.turns,
        );
        // Stage 3: paint. Rendering records hit regions into the layout.
        self.paint(f, v, m, time, queued, compact);
    }

    /// Compute the frame's rects and, on a cache miss, relayout the timeline.
    fn relayout(&mut self, area: Rect, v: &mut View, compact: bool) {
        let layout = &mut self.layout;
        let content_area = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
        // Todo panel appears only when there are tasks to inspect.
        // Preserve a readable conversation column; compact windows use the task dock.
        let sidebar_visible = v.todos.panel_open() && area.width >= 100;
        let panel_w = (area.width / 3).clamp(28, 40);
        // The composer owns the whole window width, independently of Todo.
        let width = content_area.width.saturating_sub(4).max(2) as usize;
        let (editor_lines, _, _) = v.editor.layout(width);
        // One editable row plus breathing room; grow only as the draft wraps.
        let input_height = editor_lines
            .len()
            .saturating_add(2)
            .max(3)
            .min(usize::from((area.height / 3).clamp(3, 8)))
            .min(usize::from(area.height.saturating_sub(6))) as u16;
        let input_height = if v.asks_empty() { input_height } else { 0 };
        let busy = v.active || compact || !v.asks_empty();
        let activity_height = if busy { 1 } else { 0 };
        let narrow_dock = if !sidebar_visible && v.todos.panel_open() {
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
        let rows = ratatui::layout::Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(ask_height),
            Constraint::Length(activity_height),
            Constraint::Length(narrow_dock),
            Constraint::Length(input_height),
        ])
        .split(content_area);
        let cols = if sidebar_visible {
            ratatui::layout::Layout::horizontal([Constraint::Min(50), Constraint::Length(panel_w)])
                .split(rows[0])
        } else {
            ratatui::layout::Layout::horizontal([Constraint::Percentage(100)]).split(rows[0])
        };
        let inner = cols[0];
        // The reading anchor is extracted from the OLD layout before the
        // transcript rect is replaced.
        let reading_anchor = (v.navigation.offset() > 0)
            .then(|| {
                layout.lines.anchor_at(
                    layout
                        .lines
                        .len()
                        .saturating_sub(v.navigation.offset() + layout.transcript.height as usize),
                )
            })
            .flatten();
        layout.transcript = inner;
        layout.panel = cols.get(1).copied();
        layout.rows = rows.to_vec();
        let preview_rows = edit_preview_quota(inner.height);
        let key = (v.revision, inner.width, inner.height, v.theme, v.active);
        if self.key != Some(key) {
            (layout.lines, layout.headers) = self.timeline.layout(
                v,
                inner.width.saturating_sub(2) as usize,
                preview_rows,
                self.tick,
            );
            if let Some(line) = reading_anchor.and_then(|anchor| layout.lines.locate(anchor)) {
                v.navigation
                    .preserve_line(line, layout.lines.len(), inner.height as usize);
            }
            self.key = Some(key);
        }
        layout.turns = self.timeline.turns.clone();
    }

    /// Render the resolved layout; hit regions are recorded into it so the
    /// next input event resolves against this frame.
    fn paint(
        &mut self,
        f: &mut Canvas,
        v: &mut View,
        m: &Metadata,
        time: FrameTime,
        queued: usize,
        compact: bool,
    ) {
        let area = f.area();
        let layout = &mut self.layout;
        let inner = layout.transcript;
        let rows = layout.rows.clone();
        let footer_lines = footer_lines(area.width as usize, v, m, queued);
        let height = inner.height as usize;
        let end = layout.lines.len().saturating_sub(v.navigation.offset());
        let start = end.saturating_sub(height);
        let mut visible = layout.lines.viewport(start..end);
        // Only visible tool headers need animation or mouse hit regions.
        let first_header = layout.headers.partition_point(|(line, _)| *line < start);
        for &(line, id) in &layout.headers[first_header..] {
            if line >= end {
                break;
            }
            if let Some(Item::Tool(tool)) = v.items().get(id.saturating_sub(v.item_id(0)) as usize)
            {
                if tool.status == ToolStatus::Running {
                    // Animated titles bypass the cache: the tick changes the
                    // spinner but not the content version. Same title builder
                    // as the layout path (cards::tool_title_row).
                    visible[line - start] = tool_title_row(
                        tool,
                        v.selected() == Some(id),
                        v.expanded(id),
                        inner.width.saturating_sub(2) as usize,
                        self.tick,
                    );
                }
            }
            layout.hits.push((
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
                "Add guidance…"
            } else {
                "Ask anything…"
            },
        );
        // Show one recovery action only while reading history; keep idle input quiet.
        if v.navigation.offset() > 0 && rows[4].height >= 3 && !v.overlay.is_open() {
            let label = "↓ Latest";
            if rows[4].width >= label.width() as u16 + 4 {
                let rect = Rect::new(
                    rows[4].right() - label.width() as u16 - 2,
                    rows[4].bottom() - 1,
                    label.width() as u16,
                    1,
                );
                f.render_widget(
                    Paragraph::new(label).style(Style::default().fg(MUTED).bg(PANEL)),
                    rect,
                );
                layout.follow_hit = Some(rect);
            }
        }
        ask_overlay(f, rows[1], v);
        // Breathing bar: one line above the input, visible only while busy.
        if rows[2].height > 0 {
            draw_activity_bar(f, rows[2], v, compact, self.tick, time.monotonic);
        }
        // Narrow-screen TODO dock (single line, only when panel won't fit).
        if rows[3].height > 0 {
            draw_narrow_todo_dock(f, rows[3], v);
            layout.todo_hit = Some(rows[3]);
        }
        let footer = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
        f.render_widget(Paragraph::new(footer_lines), footer);
        let menu = v.menu();
        let commands = menu.items();
        let selected_command = menu.selected;
        if !commands.is_empty() {
            let height = (commands.len() as u16 + 2).min(rows[4].y.saturating_sub(area.y));
            let rect = Rect::new(
                rows[4].x,
                rows[4].y.saturating_sub(height),
                rows[4].width.min(62),
                height,
            );
            layout.command_area = Some(rect);
            let visible = height.saturating_sub(2) as usize;
            let start = selected_command.saturating_sub(visible.saturating_sub(1));
            for (row, command) in commands.iter().skip(start).take(visible).enumerate() {
                layout.command_hits.push((
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
                            if i == selected_command { "›" } else { " " },
                            c.text,
                            c.description
                        ),
                        Style::default()
                            .fg(if i == selected_command { ACCENT } else { TEXT })
                            .add_modifier(if i == selected_command {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ))
                    .style(Style::default().bg(if i == selected_command {
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
        if let Some(side) = layout.panel {
            let hits = sidebar(f, side, v);
            layout.todo_hit = hits.0;
            layout.todo_area = hits.1;
        }
        match &v.overlay {
            Overlay::None => {}
            Overlay::Stats { .. } => stats_overlay(f, area, v, m, queued),
            Overlay::Models(_) => model_picker_overlay(f, area, v),
            Overlay::LoadingSessions => {
                let rect = crate::picker::centered(area, 40, 3);
                f.render_widget(Clear, rect);
                f.render_widget(
                    Paragraph::new("Loading sessions… · Esc close")
                        .style(Style::default().fg(TEXT).bg(PANEL)),
                    rect,
                );
            }
            Overlay::Sessions(_) => sessions_overlay(f, area, v, time.unix_seconds),
            Overlay::Themes(_) => theme_picker_overlay(f, area, v),
            Overlay::Help { scroll } => help(f, area, *scroll),
        }
        v.theme.apply(f.buffer_mut());
        self.selection
            .render(f.buffer_mut(), v.theme.color(BG), v.theme.color(BLUE));
        if let Some((message, since)) = &v.toast {
            if time.monotonic.saturating_duration_since(*since).as_secs() < 3 {
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
pub(super) fn spinner_frame(tick: u64) -> char {
    SPINNER[(tick as usize) % SPINNER.len()]
}
fn activity(v: &View, compact: bool, now: std::time::Instant) -> String {
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
            .saturating_duration_since(now)
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
fn elapsed_str(v: &View, now: std::time::Instant) -> String {
    v.since
        .map(|t| {
            let s = now.saturating_duration_since(t).as_secs();
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
fn welcome(f: &mut Canvas, area: Rect) {
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

fn draw_editor(f: &mut Canvas, e: &Editor, area: Rect, focus: bool, placeholder: &str) {
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
fn sidebar(f: &mut Canvas, area: Rect, v: &View) -> (Option<Rect>, Option<Rect>) {
    let block = Block::default()
        .padding(ratatui::widgets::Padding::new(2, 2, 1, 1))
        .style(Style::default().bg(CODE_SURFACE));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let done = v.todos.items().iter().filter(|t| t.completed).count();
    f.render_widget(
        Paragraph::new(format!(" Todo · {done}/{} · ^T", v.todos.items().len()))
            .style(Style::default().fg(TEXT).bold()),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let body = Rect::new(
        inner.x,
        inner.y.saturating_add(2),
        inner.width,
        inner.height.saturating_sub(2),
    );
    let current = v.todos.items().iter().position(|t| !t.completed);
    let mut lines = Vec::new();
    for (i, todo) in v.todos.items().iter().enumerate() {
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
        .todos
        .scroll_within(lines.len().saturating_sub(body.height as usize));
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
fn draw_narrow_todo_dock(f: &mut Canvas, area: Rect, v: &View) {
    let done = v.todos.items().iter().filter(|t| t.completed).count();
    let total = v.todos.items().len();
    let next = v
        .todos
        .items()
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

fn draw_activity_bar(
    f: &mut Canvas,
    area: Rect,
    v: &View,
    compact: bool,
    tick: u64,
    now: std::time::Instant,
) {
    let spinner = spinner_frame(tick);
    let act = activity(v, compact, now);
    let elapsed = elapsed_str(v, now);
    let text = if elapsed.is_empty() {
        format!("{spinner} {act} · Esc 打断")
    } else {
        format!("{spinner} {act} · {elapsed} · Esc 打断")
    };
    let color = pulse_color(tick as f32 * 0.1, v.theme);
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
        (0, format!("tok {}", tokens(v.usage().total_tokens))),
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
    // A partial title competes with the path and conveys little: show it whole or omit it.
    let show_title = !omitted && title.width() + 3 + m.cwd.width() <= left_width;
    let title_label = if show_title {
        format!("{title} · ")
    } else {
        String::new()
    };
    let path = elide_tail(&m.cwd, left_width.saturating_sub(title_label.width()));
    let gap = width.saturating_sub(title_label.width() + path.width() + right_width);
    vec![Line::from(vec![
        Span::styled(title_label, Style::default().fg(TEXT).bold()),
        Span::styled(path, muted),
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

fn label(s: &str, color: Color) -> Line<'static> {
    Line::from(Span::styled(format!("  {s}"), Style::default().fg(color)))
}
fn help(f: &mut Canvas, area: Rect, scroll: u16) {
    let rect = crate::picker::centered(area, 78, area.height.saturating_sub(2) as usize);
    let text="Enter          Send / steer; confirm reply\nCtrl-J/Alt-Enter  Newline (paste preserves newlines)\nArrows/Home/End  Move cursor; Backspace/Delete\nCtrl-A/E/B/F   Line start/end · char back/fwd\nCtrl-W/U/K     Del word · to line start/end\nAlt-B/F/D·Ctrl-Left/Right  Word move · del word\nUp/Down·Ctrl-P/N  History (or row move in multiline)\nPgUp / PgDn     Scroll conversation\nCtrl-End        Follow newest output\nCtrl-Home       Jump to latest question\nCtrl-Up/Down    Previous / next question\nCtrl-G          Toggle YOLO between turns\n/               Command menu · Up/Down · Tab/Enter\nF6/Shift-F6·Click  Select next/prev · expand block\nCtrl-O / Ctrl-R  Toggle selected block / thinking\nCtrl-T          Toggle Todo panel\nCtrl-B          Toggle stats dashboard overlay\nCtrl-Y          Cycle color theme\nMouse drag      Release to copy automatically\nEsc / Ctrl-C    Cancel exec / clear selection / close\nAlt-PgUp/PgDn   Scroll approval details\nCtrl-Q          Quit\n\n/queue TEXT     Schedule a follow-up turn\n/compact        Compact idle conversation\n/new · /clear   Fresh context; previous session saved\n/yolo [on|off]   Change permissions between turns\n/theme          Theme picker (or /theme NAME)\n/models         Switch model (picker or /models p/m [variant])\n/sessions       Switch sessions (Ctrl-D asks to delete)\n/status         Same as Ctrl-B dashboard\n/help           This help · Esc closes\n\nApprovals: y/n + Enter (YOLO skips approvals).";
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
                v.navigation.set_offset(100 + (i % 30) * 3);
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
                renderer.layout.lines.len(),
                v.items().len()
            );
        }
        // Include cell diffing and actual ANSI encoding, not only TestBackend writes.
        struct Output(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);
        impl std::io::Write for Output {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.borrow_mut().extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let output = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut terminal = Terminal::with_options(
            ratatui::backend::CrosstermBackend::new(Output(output.clone())),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, 160, 48)),
            },
        )
        .unwrap();
        let mut renderer = Renderer::default();
        let mut samples = Vec::new();
        let mut bytes = 0;
        for i in 0..121 {
            v.navigation.set_offset(100 + i * 3);
            output.borrow_mut().clear();
            let start = Instant::now();
            terminal
                .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
                .unwrap();
            if i > 0 {
                samples.push(start.elapsed().as_secs_f64() * 1000.0);
                bytes += output.borrow().len();
            }
        }
        samples.sort_by(f64::total_cmp);
        eprintln!(
            "ansi_scroll p50={:.3}ms p95={:.3}ms bytes/frame={}",
            samples[60],
            samples[114],
            bytes / 120
        );
    }

    #[allow(clippy::wildcard_imports)]
    use super::*;
    use crate::ui::state::Role;
    use crate::ui::theme::USER_SURFACE;
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
        v.restore_usage(
            yourai_core::prelude::Usage {
                input_tokens: 125_000,
                output_tokens: 6_000,
                total_tokens: 131_000,
            },
            0,
        );
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
            .layout
            .command_hits
            .iter()
            .find(|(_, c)| c.text == "/queue")
            .copied()
            .unwrap();
        super::super::app::click_dispatch(&mut renderer, &mut v, rect.x, rect.y);
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
        v.model.label = "test-model".into();
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
        assert_eq!(activity(&v, false, std::time::Instant::now()), "Thinking");
        v.event(Out::ToolStarted {
            id: "t".into(),
            name: "shell".into(),
            input: json!({"command":"cargo test"}),
        });
        assert_eq!(
            activity(&v, false, std::time::Instant::now()),
            "Running cargo test"
        );
        terminal
            .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 2, false))
            .unwrap();
        let screen = rows(&terminal).join("\n");
        assert!(!screen.contains("yourai · your"));
        assert!(screen.contains("Running cargo"));
        assert!(screen.contains("Add guidance…"));
        // Queued count is now in the footer left slot.
        assert!(screen.contains("2 queued"));
        v.event(Out::Ask {
            id: "approval".into(),
            payload: json!({"kind":"permission"}),
        });
        assert_eq!(
            activity(&v, false, std::time::Instant::now()),
            "Waiting for approval"
        );
        v.settle();
        terminal
            .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
            .unwrap();
        v.navigation.anchor(0, 0);
        v.navigation.reveal(Some(0));

        v.follow();
        v.navigation.resolve(100, 10, &[(0, 0)], &[]);
        assert_eq!(v.navigation.offset(), 0);
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
        assert_eq!(renderer.layout.hits.len(), 2);
        let (rect, id) = renderer.layout.hits[1];
        // Card preview shows output but NOT the input JSON (only expanded shows input).
        assert!(!renderer
            .layout
            .lines
            .iter()
            .any(|l| l.to_string().contains("\"command\"")));
        super::super::app::click_dispatch(&mut renderer, &mut view, rect.x + 2, rect.y);
        terminal
            .draw(|f| renderer.draw(f, &mut view, &m, &SessionStatus::Idle, 0, false))
            .unwrap();
        // Expanded shows the input JSON.
        assert!(renderer
            .layout
            .lines
            .iter()
            .any(|l| l.to_string().contains("\"command\"")));
        // Expanded thinking shows full text (card title already has a summary).
        assert!(renderer
            .layout
            .lines
            .iter()
            .any(|l| l.to_string().contains("private-thought")));
        terminal.backend_mut().resize(50, 20);
        terminal.resize(Rect::new(0, 0, 50, 20)).unwrap();
        terminal
            .draw(|f| renderer.draw(f, &mut view, &m, &SessionStatus::Idle, 0, false))
            .unwrap();
        let (rect, _) = *renderer
            .layout
            .hits
            .iter()
            .find(|(_, key)| *key == id)
            .unwrap();
        super::super::app::click_dispatch(&mut renderer, &mut view, rect.x + 2, rect.y);
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
        v.todos.set(todos.clone());
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
        let hit = renderer.layout.todo_hit.unwrap();
        super::super::app::click_dispatch(&mut renderer, &mut v, hit.x, hit.y);
        assert!(!v.todos.panel);
        let text = draw(&mut terminal, &mut renderer, &mut v).join("\n");
        // With panel off, Todo items are not visible.
        assert!(!text.contains("Task number 0"));
        // Re-open via the field directly (wide screen has no dock to click).
        v.todos.panel = true;
        draw(&mut terminal, &mut renderer, &mut v);
        // Wheel over the TODO list scrolls it, not the transcript.
        let area = renderer.layout.todo_area.unwrap();
        super::super::app::wheel_dispatch(&mut renderer, &mut v, area.x, area.y, false);
        assert_eq!(v.todos.scroll(), 1);
        todos.retain(|t| t.id != "t8");
        v.todos.set(todos);
        assert_eq!(v.todos.scroll(), 1);
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
            v.todos.set(vec![
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
    fn completed_turn_keeps_original_evidence_without_duplicate_summary() {
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
        v.event(Out::Message { text:"## Parser boundary updated\n\nThe empty-input case still needs a follow-up fix.\n\n- Build command exited successfully.\n- Parser test command failed; inspect the diagnostic below.".into() });
        v.settle();
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
        assert!(!content.contains("Recorded actions"));
        assert!(content.contains("parser_empty_input"));
        assert!(content.contains("cargo check"));
        assert_eq!(content.matches("Parser boundary updated").count(), 1);
        if let Ok(path) = std::env::var("YOURAI_READING_SNAPSHOT") {
            let cells = terminal.backend().buffer().content().iter().map(|c|json!({"text":c.symbol(),"fg":format!("{:?}",c.fg),"bg":format!("{:?}",c.bg),"bold":c.modifier.contains(Modifier::BOLD)})).collect::<Vec<_>>();
            std::fs::write(
                path,
                serde_json::to_vec(&json!({"width":120,"height":48,"cells":cells})).unwrap(),
            )
            .unwrap();
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
            v.navigation.set_offset(scroll);
            terminal
                .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
                .unwrap();
            assert!(renderer.layout.lines.len() > renderer.layout.transcript.height as usize);
            if scroll > 0 {
                assert!(
                    renderer.layout.follow_hit.is_some(),
                    "history has a return action"
                );
            } else {
                assert!(
                    renderer.layout.follow_hit.is_none(),
                    "live view stays quiet"
                );
            }
            for y in 0..renderer.layout.transcript.bottom() {
                assert_eq!(terminal.backend().buffer()[(79, y)].symbol(), " ");
            }
        }
        // Overscroll cannot accumulate invisible distance and delay direction reversal.
        for _ in 0..1000 {
            super::super::app::wheel_dispatch(&mut renderer, &mut v, 1, 1, true);
        }
        let top = renderer.layout.lines.len() - renderer.layout.transcript.height as usize;
        assert_eq!(v.navigation.offset(), top);
        super::super::app::wheel_dispatch(&mut renderer, &mut v, 1, 1, false);
        assert_eq!(v.navigation.offset(), top - 3);
        let hit = renderer.layout.follow_hit.unwrap();
        super::super::app::click_dispatch(&mut renderer, &mut v, hit.x, hit.y);
        assert_eq!(v.navigation.offset(), 0);
        terminal
            .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
            .unwrap();
        assert!(renderer.layout.follow_hit.is_none());
        renderer.latest_turn(&mut v);
        terminal
            .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
            .unwrap();
        let start = renderer.layout.lines.len()
            - v.navigation.offset()
            - renderer.layout.transcript.height as usize;
        assert_eq!(start, renderer.timeline.turns[0].0);
    }

    #[test]
    fn composer_grows_across_full_width_and_keeps_cursor_above_single_footer() {
        let mut v = View::default();
        v.theme = Theme::Dark;
        v.todos.set(vec![super::super::state::Todo {
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
                if let Some(panel) = renderer.layout.panel {
                    assert!(panel.bottom() < prompt_y);
                }
                assert!(cursor.x >= 3 && cursor.x < width - 1);
                assert!(cursor.y >= prompt_y && cursor.y < height - 2);
                let footer = (0..width)
                    .map(|x| buffer[(x, height - 1)].symbol())
                    .collect::<String>();
                assert!(footer.contains("YOLO"));
                if text.is_empty() {
                    let input_height = 3.min((height / 3).clamp(3, 8));
                    assert_eq!(
                        prompt_y,
                        height - input_height,
                        "compact input adapts to short windows"
                    );
                }
            }
        }
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
            super::super::app::wheel_dispatch(&mut renderer, &mut v, 10, 10, true);
            frame!(format!("wheel scroll up #{i}"), SessionStatus::Idle);
        }

        v.follow();
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
    #[test]
    fn reading_anchor_survives_head_eviction_and_tail_append() {
        let mut view = View::default();
        for i in 0..1000 {
            view.notice(yourai_core::prelude::Level::Info, format!("message {i:04}"));
        }
        let meta = Metadata {
            session: "test".into(),
            cwd: "/tmp".into(),
            trusted_shell: false,
            yolo: false,
        };
        let mut renderer = Renderer::default();
        let area = Rect::new(0, 0, 80, 24);
        renderer.prepare(area, &mut view, &meta, FrameTime::now(), 0, false);
        view.navigation.set_offset(300);
        renderer.prepare(area, &mut view, &meta, FrameTime::now(), 0, false);
        let top = |r: &Renderer, v: &View| {
            r.layout.lines.len() - v.navigation.offset() - r.layout.transcript.height as usize
        };
        let before = renderer
            .layout
            .lines
            .viewport(top(&renderer, &view)..top(&renderer, &view) + 1);
        view.notice(yourai_core::prelude::Level::Info, "appended after eviction");
        renderer.prepare(area, &mut view, &meta, FrameTime::now(), 0, false);
        let after = renderer
            .layout
            .lines
            .viewport(top(&renderer, &view)..top(&renderer, &view) + 1);
        assert_eq!(before, after);
    }

    #[allow(clippy::wildcard_imports)]
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

        renderer.latest_turn(&mut view);
        draw(&mut terminal, &mut renderer, &mut view);
        let start = renderer.layout.lines.len()
            - view.navigation.offset()
            - renderer.layout.transcript.height as usize;
        assert_eq!(start, renderer.timeline.turns[1].0);
        renderer.jump_turn(&mut view, true);
        draw(&mut terminal, &mut renderer, &mut view);
        let start = renderer.layout.lines.len()
            - view.navigation.offset()
            - renderer.layout.transcript.height as usize;
        assert_eq!(start, renderer.timeline.turns[0].0);
        renderer.jump_turn(&mut view, false);
        draw(&mut terminal, &mut renderer, &mut view);
        assert_eq!(
            renderer.layout.lines.len()
                - view.navigation.offset()
                - renderer.layout.transcript.height as usize,
            renderer.timeline.turns[1].0
        );
        terminal.backend_mut().resize(40, 20);
        terminal.autoresize().unwrap();
        draw(&mut terminal, &mut renderer, &mut view);
        renderer.latest_turn(&mut view);
        draw(&mut terminal, &mut renderer, &mut view);
        let start = renderer.layout.lines.len()
            - view.navigation.offset()
            - renderer.layout.transcript.height as usize;
        assert_eq!(start, renderer.timeline.turns[1].0);

        view.follow();
        draw(&mut terminal, &mut renderer, &mut view);
        assert_eq!(view.navigation.offset(), 0);
    }
    #[test]
    fn footer_measures_unicode_long_labels_and_large_metrics() {
        let mut v = View::default();
        v.title = Some("这是一个很长的会话标题 🔎 review ".repeat(6));
        v.model.label = "provider/very-long-model-name-with-reasoning-variant".repeat(3);
        v.restore_usage(
            yourai_core::prelude::Usage {
                total_tokens: u64::MAX,
                ..Default::default()
            },
            0,
        );
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
            assert!(
                !text.contains("这是"),
                "overflowing title must disappear entirely"
            );
            for field in ["ctx ", "YOLO"] {
                assert!(text.contains(field));
            }
        }
        v.title = Some("Review".into());
        v.model.label = "mock/model".into();
        v.restore_usage(
            yourai_core::prelude::Usage {
                total_tokens: 1200,
                ..Default::default()
            },
            0,
        );
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
        v.model.label = "provider/model".into();
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
                renderer.layout.panel.is_none(),
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

#[cfg(test)]
mod clock_tests {
    use super::{activity, elapsed_str, FrameTime, Metadata, Renderer, View};
    use crate::ui::state::RetryState;
    use ratatui::layout::Rect;
    use std::time::{Duration, Instant};
    #[test]
    fn supplied_time_controls_animation_retry_elapsed_and_toast_expiry() {
        let start = Instant::now();
        let at = |seconds| FrameTime {
            monotonic: start + Duration::from_secs(seconds),
            unix_seconds: seconds as i64,
        };
        let mut view = View::default();
        view.active = true;
        view.since = Some(start);
        view.toast = Some(("copied".into(), start));
        view.retry = Some(RetryState {
            attempt: 1,
            max: 3,
            reason: "retry".into(),
            until: start + Duration::from_secs(5),
        });
        assert!(activity(&view, false, at(2).monotonic).contains("3s"));
        assert_eq!(elapsed_str(&view, at(2).monotonic), "2s");
        let meta = Metadata {
            session: "test".into(),
            cwd: "/tmp".into(),
            trusted_shell: false,
            yolo: false,
        };
        let mut renderer = Renderer::default();
        let area = Rect::new(0, 0, 80, 24);
        let mut first = renderer.prepare(area, &mut view, &meta, at(0), 0, false);
        let repeated = renderer.prepare(area, &mut view, &meta, at(0), 0, false);
        assert!(first == repeated);
        assert!(first
            .buffer_mut()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
            .contains("copied"));
        let mut expired = renderer.prepare(area, &mut view, &meta, at(4), 0, false);
        assert!(first != expired);
        assert!(!expired
            .buffer_mut()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
            .contains("copied"));
    }
}
