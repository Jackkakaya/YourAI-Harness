use super::{frame_time::FrameTime, presentation::Canvas};
mod cards;
mod footer;
mod overlays;
mod timeline;
use super::overlay::Overlay;
use super::{
    editor::Editor,
    state::{Item, ToolStatus, View},
    theme::{
        lerp_color, Theme, ACCENT, BG, BLUE, BORDER, BRAND_TEAL, BRAND_VIOLET, CODE_SURFACE,
        FOCUS_SURFACE, GREEN, MUTED, PANEL, TEXT,
    },
};
use crate::text::elide;
use cards::tool_title_row;
use footer::{footer_lines, tokens};
use overlays::{
    ask_overlay, effort_picker_overlay, model_picker_overlay, sessions_overlay, stats_overlay,
    theme_picker_overlay,
};
use ratatui::{
    prelude::*,
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use yourai_core::prelude::Level;
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
    /// The stacked rows: [transcript, ask, todo dock, input].
    rows: Vec<Rect>,
    /// Todo sidebar rect when the wide layout shows it.
    panel: Option<Rect>,
    home: Option<HomeLayout>,
    lines: timeline::LayoutLines,
    headers: Vec<(usize, u64)>,
    turns: Vec<(usize, u64)>,
    hits: Vec<(Rect, u64)>,
    command_hits: Vec<(Rect, super::commands::Command)>,
    command_area: Option<Rect>,
    mention_hits: Vec<(Rect, usize)>,
    mention_area: Option<Rect>,
    follow_hit: Option<Rect>,
    todo_hit: Option<Rect>,
    todo_area: Option<Rect>,
}
#[derive(Clone, Copy)]
struct HomeLayout {
    logo: Rect,
    workspace: Option<Rect>,
    metrics: Option<Rect>,
    suggestions: Option<Rect>,
    footer: Rect,
}

/// What a click on the last painted frame hit. The renderer answers queries
/// about its own layout; the resulting View mutations live in the action
/// layer (`app::click_dispatch`), keeping input state out of the render path.
pub(crate) enum Hit<'a> {
    /// The "↓ Latest" pill shown above the composer while scrolled up.
    FollowLatest,
    /// A row of the slash-command menu.
    Command(&'a super::commands::Command),
    /// A row of the @-mention autocomplete popup (index into its entries).
    Mention(usize),
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
        if self.layout.mention_area.is_some_and(|r| r.contains(point)) {
            return self
                .layout
                .mention_hits
                .iter()
                .find(|(r, _)| r.contains(point))
                .map(|(_, i)| Hit::Mention(*i));
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
        v.session
            .navigation
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
        v.session.navigation.scroll(
            rows,
            up,
            self.layout
                .lines
                .len()
                .saturating_sub(self.layout.transcript.height as usize),
        );
    }

    pub fn latest_turn(&self, v: &mut View) {
        v.session
            .navigation
            .turn(self.layout.turns.last().map(|(_, id)| *id));
    }
    pub fn jump_turn(&self, v: &mut View, previous: bool) {
        v.session.navigation.jump(
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
        self.layout.mention_hits.clear();
        self.layout.mention_area = None;
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
        // An empty rectangle explicitly clears a previous session's scroll
        // window; Canvas::None means preserve the backend's current window.
        f.set_scroll_region(if self.layout.home.is_some() {
            Rect::default()
        } else {
            self.layout.transcript
        });
        // Stage 2: navigation — pending intents resolve against the new
        // layout. This and the anchor above are the only View writes in the
        // prepare path.
        v.session.navigation.resolve(
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
        let home = v.items().iter().all(|item| {
            matches!(
                item,
                Item::Notice {
                    level: Level::Info,
                    ..
                }
            )
        }) && !v.session.active
            && !compact
            && v.asks_empty()
            && !v.session.todos.panel_open();
        layout.home = None;
        let content_area = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
        // Todo panel appears only when there are tasks to inspect.
        // Preserve a readable conversation column; compact windows use the task dock.
        let sidebar_visible = v.session.todos.panel_open() && area.width >= 100;
        let panel_w = (area.width / 3).clamp(32, 44);
        // The composer owns the whole window width, independently of Todo.
        let composer_width = if home {
            area.width.saturating_sub(4).min(100)
        } else {
            content_area.width
        };
        let width = composer_width.saturating_sub(4).max(2) as usize;
        let (editor_lines, _, _) = v.draft.editor().layout(width);
        // One editable row plus breathing room; grow only as the draft wraps.
        let input_height = editor_lines
            .len()
            .saturating_add(if area.height >= 16 { 3 } else { 2 })
            .max(3)
            .min(usize::from((area.height / 3).clamp(3, 8)))
            .min(usize::from(area.height.saturating_sub(6))) as u16;
        let input_height = if v.asks_empty() { input_height } else { 0 };
        let narrow_dock = if !sidebar_visible && v.session.todos.panel_open() {
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
                .saturating_sub(input_height + narrow_dock + 1),
        );
        // One status row below the composer, total: while busy the footer's
        // left side carries the spinner/activity instead of title and path.
        let rows = ratatui::layout::Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(ask_height),
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
        let reading_anchor = (v.session.navigation.offset() > 0)
            .then(|| {
                layout.lines.anchor_at(layout.lines.len().saturating_sub(
                    v.session.navigation.offset() + layout.transcript.height as usize,
                ))
            })
            .flatten();
        layout.transcript = inner;
        layout.panel = cols.get(1).copied();
        layout.rows = rows.to_vec();
        if home {
            let logo_height = if composer_width >= 60 && area.height >= 20 {
                5
            } else {
                1
            };
            let rich = composer_width >= 64 && area.height >= 26;
            let gap = if area.height >= 16 { 2 } else { 1 };
            let extra = if rich { 6 } else { 0 };
            let group_height = logo_height + gap + extra + input_height + if rich { 5 } else { 2 };
            let x = area.x + (area.width - composer_width) / 2;
            let y = content_area.y + content_area.height.saturating_sub(group_height) / 2;
            let split_brand = rich && composer_width >= 92;
            let input_y = y + logo_height + gap + extra;
            layout.rows[3] = Rect::new(x, input_y, composer_width, input_height);
            layout.home = Some(HomeLayout {
                logo: Rect::new(
                    x,
                    y,
                    if split_brand { 46 } else { composer_width },
                    logo_height,
                ),
                workspace: split_brand.then(|| Rect::new(x + 52, y, composer_width - 52, 5)),
                metrics: rich.then(|| Rect::new(x, y + logo_height + gap, composer_width, 4)),
                suggestions: rich.then(|| {
                    Rect::new(
                        x + 3,
                        layout.rows[3].bottom() + 3,
                        composer_width.saturating_sub(4),
                        1,
                    )
                }),
                footer: Rect::new(
                    x + 3,
                    layout.rows[3].bottom() + 1,
                    composer_width.saturating_sub(4),
                    1,
                ),
            });
        }
        let preview_rows = edit_preview_quota(inner.height);
        let key = (
            v.session.revision,
            inner.width,
            inner.height,
            v.theme,
            v.session.active,
        );
        if self.key != Some(key) {
            (layout.lines, layout.headers) = self.timeline.layout(
                v,
                inner.width.saturating_sub(4) as usize,
                preview_rows,
                self.tick,
            );
            if let Some(line) = reading_anchor.and_then(|anchor| layout.lines.locate(anchor)) {
                v.session
                    .navigation
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
        // While busy, the footer's left side becomes the activity status
        // (spinner + action + elapsed + esc hint) — no second status row.
        let busy_status = {
            let busy = v.session.active || compact || !v.asks_empty();
            busy.then(|| {
                let act = activity(v, compact, time.monotonic);
                let elapsed = elapsed_str(v, time.monotonic);
                let spinner = spinner_frame(self.tick);
                let text = if elapsed.is_empty() {
                    format!("{spinner} {act} · esc stop")
                } else {
                    format!("{spinner} {act} · {elapsed} · esc stop")
                };
                (text, pulse_color(self.tick as f32 * 0.1, v.theme))
            })
        };
        let footer_lines = footer_lines(
            area.width as usize,
            v,
            m,
            queued,
            busy_status.as_ref().map(|(t, c)| (t.as_str(), *c)),
        );
        let height = inner.height as usize;
        let end = layout
            .lines
            .len()
            .saturating_sub(v.session.navigation.offset());
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
                        inner.width.saturating_sub(4) as usize,
                        self.tick,
                    );
                }
            }
            layout.hits.push((
                Rect::new(inner.x, inner.y + (line - start) as u16, inner.width, 1),
                id,
            ));
        }
        let reading = Rect::new(
            inner.x + 1,
            inner.y,
            inner.width.saturating_sub(2),
            inner.height,
        );
        if let Some(home) = layout.home {
            welcome(f, home, v, m);
        } else {
            f.render_widget(Paragraph::new(visible), reading);
        }
        // Staged-attachment badge on the composer's meta row (right side);
        // the model label takes the left side, inside draw_editor.
        let (n_img, n_ref) = v.draft.attachment_counts();
        let badge = match (n_img, n_ref) {
            (0, 0) => String::new(),
            (0, r) => format!("{r} ref · Esc clears"),
            (i, 0) => format!("{i} img · Esc clears"),
            (i, r) => format!("{i} img · {r} ref · Esc clears"),
        };
        draw_editor(
            f,
            v.draft.editor(),
            rows[3],
            v.asks_empty() && !v.overlay.is_open(),
            if v.session.active {
                "Add guidance…"
            } else {
                "Ask anything…"
            },
            ComposerMeta {
                // The composer's model tag carries the effective thinking
                // effort, e.g. "gateway/kimi-k3 · high".
                model: &match &v.model.effort {
                    Some(effort) => format!("{} · {}", v.model.label, effort),
                    None => v.model.label.clone(),
                },
                badge: &badge,
                active: v.session.active,
                home: layout.home.is_some(),
            },
        );
        // Show one recovery action only while reading history; keep idle input quiet.
        if v.session.navigation.offset() > 0 && rows[3].height >= 3 && !v.overlay.is_open() {
            let label = "↓ Latest";
            if rows[3].width >= label.width() as u16 + 4 {
                let rect = Rect::new(
                    rows[3].right() - label.width() as u16 - 2,
                    rows[3].y.saturating_sub(1),
                    label.width() as u16,
                    1,
                );
                f.render_widget(Clear, rect);
                f.render_widget(
                    Paragraph::new(label).style(Style::default().fg(MUTED).bg(PANEL)),
                    rect,
                );
                layout.follow_hit = Some(rect);
            }
        }
        ask_overlay(f, rows[1], v);
        // Narrow-screen TODO dock (single line, only when panel won't fit).
        if rows[2].height > 0 {
            draw_narrow_todo_dock(f, rows[2], v);
            layout.todo_hit = Some(rows[2]);
        }
        let footer = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
        if layout.home.is_none() {
            f.render_widget(Paragraph::new(footer_lines), footer);
        }
        let menu = v.menu();
        let commands = menu.items();
        let selected_command = menu.selected;
        if !commands.is_empty() {
            let height = (commands.len() as u16 + 2)
                .min(12)
                .min(rows[3].y.saturating_sub(area.y));
            let rect = Rect::new(
                rows[3].x,
                rows[3].y.saturating_sub(height),
                rows[3].width.min(62),
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
                    let selected = i == selected_command;
                    let label = format!(" {} {:<13} ", if selected { "›" } else { " " }, c.text);
                    let description = elide(
                        c.description,
                        (rect.width as usize).saturating_sub(2 + label.width()),
                    );
                    Line::from(vec![
                        Span::styled(
                            label,
                            Style::default()
                                .fg(if selected { ACCENT } else { TEXT })
                                .bold(),
                        ),
                        Span::styled(description, Style::default().fg(MUTED)),
                    ])
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
                            .title(" Commands ")
                            .title_bottom(" ↑↓ select · Enter apply · Esc close "),
                    ),
                rect,
            );
        }
        // @ mention autocomplete popup.
        if v.asks_empty()
            && !v.overlay.is_open()
            && v.draft.mention().active
            && !v.draft.mention().entries.is_empty()
        {
            let entries = &v.draft.mention().entries;
            let height = (entries.len() as u16 + 2).min(rows[3].y.saturating_sub(area.y));
            let rect = Rect::new(
                inner.x,
                rows[3].y.saturating_sub(height),
                inner.width.min(62),
                height,
            );
            layout.mention_area = Some(rect);
            let visible = height.saturating_sub(2) as usize;
            let start = v
                .draft
                .mention()
                .selected
                .saturating_sub(visible.saturating_sub(1));
            for (row, (index, _)) in entries
                .iter()
                .enumerate()
                .skip(start)
                .take(visible)
                .enumerate()
            {
                layout.mention_hits.push((
                    Rect::new(
                        rect.x + 1,
                        rect.y + 1 + row as u16,
                        rect.width.saturating_sub(2),
                        1,
                    ),
                    index,
                ));
            }
            f.render_widget(Clear, rect);
            let lines = entries
                .iter()
                .enumerate()
                .skip(start)
                .take(visible)
                .map(|(i, e)| {
                    let icon = if e.is_dir { "dir" } else { "file" };
                    Line::from(Span::styled(
                        elide(
                            &format!(
                                " {} {icon:<4} {}",
                                if i == v.draft.mention().selected {
                                    "›"
                                } else {
                                    " "
                                },
                                e.display
                            ),
                            rect.width.saturating_sub(2) as usize,
                        ),
                        Style::default().fg(if i == v.draft.mention().selected {
                            ACCENT
                        } else {
                            TEXT
                        }),
                    ))
                    .style(Style::default().bg(
                        if i == v.draft.mention().selected {
                            FOCUS_SURFACE
                        } else {
                            PANEL
                        },
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
                            .title(" @ Files ")
                            .title_bottom(" ↑↓ select · Enter attach · Esc close "),
                    ),
                rect,
            );
        }
        if let Some(side) = layout.panel {
            let hits = sidebar(f, side, v);
            layout.todo_hit = hits.0;
            layout.todo_area = hits.1;
        }
        match &mut v.overlay {
            Overlay::None => {}
            Overlay::Stats { .. } => stats_overlay(f, area, v, m),
            Overlay::Models(_) => model_picker_overlay(f, area, v),
            Overlay::Effort { .. } => effort_picker_overlay(f, area, v),
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
            Overlay::Help { scroll } => help(f, area, scroll),
        }
        v.theme.apply(f.buffer_mut());
        self.selection
            .render(f.buffer_mut(), v.theme.color(BG), v.theme.color(BLUE));
        if let Some((message, since)) = &v.toast {
            if time.monotonic.saturating_duration_since(*since).as_secs() < 3 {
                let width = (message.width() as u16 + 6).min(area.width.saturating_sub(4));
                let rect = Rect::new(
                    area.x + (area.width - width) / 2,
                    rows[3].y.saturating_sub(4),
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
        return "Preparing context".into();
    }
    if let Some(retry) = &v.session.retry {
        let seconds = retry
            .until
            .saturating_duration_since(now)
            .as_secs_f64()
            .ceil() as u64;
        return format!(
            "{} · retry in {}s ({}/{})",
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
    v.session
        .since
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
/// Empty sessions keep the brand, actual editor and orientation in one group.
/// This is the same editor used in a conversation, including paste and menus.
fn welcome(f: &mut Canvas, home: HomeLayout, v: &View, m: &Metadata) {
    let area = f.area();
    let logo = if home.logo.height >= 5 {
        crate::branding::wordmark()
    } else {
        vec![Line::from(Span::styled(
            "YourAI",
            Style::default().fg(BRAND_TEAL).bold(),
        ))
        .alignment(Alignment::Center)]
    };
    f.render_widget(Paragraph::new(logo), home.logo);
    f.render_widget(
        Paragraph::new("New session").style(Style::default().fg(MUTED)),
        Rect::new(home.footer.x, area.y, home.footer.width, 1),
    );
    if let Some(rect) = home.workspace {
        let lines = vec![
            Line::from(Span::styled("WORKSPACE", Style::default().fg(BRAND_TEAL))),
            Line::from(Span::styled(
                crate::text::elide_tail(&m.cwd, rect.width as usize),
                Style::default().fg(TEXT).bold(),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Think clearly. Build confidently.",
                Style::default().fg(BRAND_VIOLET),
            )),
        ];
        f.render_widget(Paragraph::new(lines), rect);
    }
    if let Some(rect) = home.metrics {
        // This is an empty conversation. A next-request estimate includes
        // system/tool overhead, so it must not read as messages already sent.
        let capacity = v
            .session
            .context_usage
            .as_ref()
            .and_then(|u| u.context_window)
            .filter(|w| *w > 0)
            .map(|w| format!("{} token window", tokens(w)))
            .unwrap_or_else(|| "Capacity unavailable".into());
        let access = if m.yolo {
            "YOLO"
        } else if m.trusted_shell {
            "Trusted shell"
        } else {
            "Ask before execution"
        };
        let detail = if m.yolo {
            "All tools auto-approved"
        } else if m.trusted_shell {
            "Other permissions still ask"
        } else {
            "/yolo toggles auto-approval"
        };
        let cols =
            ratatui::layout::Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
                .spacing(2)
                .split(rect);
        for (card, title, value, detail, color) in [
            (
                cols[0],
                "MODEL CAPACITY",
                capacity,
                "No messages sent",
                BRAND_TEAL,
            ),
            (
                cols[1],
                "EXECUTION",
                access.into(),
                detail,
                if m.yolo {
                    super::theme::YELLOW
                } else {
                    BRAND_VIOLET
                },
            ),
        ] {
            f.render_widget(
                Block::default().style(Style::default().bg(CODE_SURFACE)),
                card,
            );
            let width = card.width.saturating_sub(4) as usize;
            let lines = vec![
                Line::from(Span::styled(
                    elide(title, width),
                    Style::default().fg(color),
                )),
                Line::from(Span::styled(
                    elide(&value, width),
                    Style::default().fg(TEXT).bold(),
                )),
                Line::from(Span::styled(
                    elide(detail, width),
                    Style::default().fg(MUTED),
                )),
            ];
            f.render_widget(
                Paragraph::new(lines).style(Style::default().bg(CODE_SURFACE)),
                Rect::new(card.x + 3, card.y, card.width.saturating_sub(4), 3),
            );
        }
    }
    if let Some(rect) = home.suggestions {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Try  ", Style::default().fg(BRAND_VIOLET)),
                Span::styled(
                    elide(
                        "Map this project · Review changes · Fix a failing test",
                        rect.width.saturating_sub(5) as usize,
                    ),
                    Style::default().fg(MUTED),
                ),
            ])),
            rect,
        );
    }
    let permission = if m.yolo {
        "YOLO"
    } else if m.trusted_shell {
        "trusted"
    } else {
        "ask"
    };
    let tips = if home.footer.width >= 80 {
        "/sessions · /models · /theme · F1"
    } else if home.footer.width >= 50 {
        "/ commands · F1 help"
    } else {
        "/ · F1"
    };
    // Each fact has one home: workspace above when visible, permissions in
    // their card when visible; compact layouts move those facts to this row.
    let permission = if home.metrics.is_none() {
        permission
    } else {
        ""
    };
    let right = if permission.is_empty() {
        tips.into()
    } else {
        format!("{tips} · {permission}")
    };
    let width = home.footer.width as usize;
    let path = if home.workspace.is_none() {
        crate::text::elide_tail(&m.cwd, width.saturating_sub(right.width() + 2))
    } else {
        String::new()
    };
    let gap = if path.is_empty() {
        0
    } else {
        width.saturating_sub(path.width() + right.width())
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(path, Style::default().fg(MUTED)),
            Span::raw(" ".repeat(gap)),
            Span::styled(tips, Style::default().fg(MUTED)),
            Span::styled(
                if permission.is_empty() {
                    String::new()
                } else {
                    format!(" · {permission}")
                },
                Style::default().fg(if m.yolo { super::theme::YELLOW } else { MUTED }),
            ),
        ])),
        home.footer,
    );
    // Local setup actions (such as choosing a model) keep the home editor
    // in place and show their latest confirmation directly beneath it.
    if home.footer.bottom() < area.bottom() - 1 {
        if let Some(Item::Notice {
            level: Level::Info,
            text,
        }) = v.items().back()
        {
            f.render_widget(
                Paragraph::new(elide(
                    &format!("· {}", text.lines().next().unwrap_or("")),
                    home.footer.width as usize,
                ))
                .style(Style::default().fg(MUTED)),
                Rect::new(home.footer.x, home.footer.bottom(), home.footer.width, 1),
            );
        }
    }
    f.render_widget(
        Paragraph::new(format!("v{}", env!("CARGO_PKG_VERSION")))
            .alignment(Alignment::Right)
            .style(Style::default().fg(MUTED)),
        Rect::new(home.footer.x, area.bottom() - 1, home.footer.width, 1),
    );
}

/// The composer uses a colored left rail,
/// text with breathing room, and a meta row underneath — the active model on
/// the left, staged attachments on the right. The rail brightens with focus.
struct ComposerMeta<'a> {
    model: &'a str,
    badge: &'a str,
    active: bool,
    home: bool,
}

fn draw_editor(
    f: &mut Canvas,
    e: &Editor,
    area: Rect,
    focus: bool,
    placeholder: &str,
    meta: ComposerMeta<'_>,
) {
    let ComposerMeta {
        model,
        badge,
        active,
        home,
    } = meta;
    if area.is_empty() {
        return;
    }
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(if focus {
            if home {
                BRAND_TEAL
            } else {
                ACCENT
            }
        } else {
            BORDER
        }))
        .border_set(ratatui::symbols::border::Set {
            vertical_left: "│",
            ..ratatui::symbols::border::Set::default()
        })
        .style(Style::default().bg(PANEL))
        .padding(ratatui::widgets::Padding::new(2, 1, 1, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);
    // The text viewport gives its last row to the meta line.
    let (lines, row, col) = e.layout(inner.width as usize);
    let view = (inner.height as usize).saturating_sub(1);
    let top = row.saturating_sub(view.saturating_sub(1));
    let text_rect = Rect::new(inner.x, inner.y, inner.width, view as u16);
    if !e.text.is_empty() {
        f.render_widget(
            Paragraph::new(
                lines
                    .into_iter()
                    .skip(top)
                    .take(view)
                    .map(Line::from)
                    .collect::<Vec<_>>(),
            ),
            text_rect,
        );
    } else if !text_rect.is_empty() {
        f.render_widget(
            Paragraph::new(elide(placeholder, inner.width as usize))
                .style(Style::default().fg(MUTED)),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
    }
    if inner.height > 0 {
        let meta = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        let submit = if active { "Enter steer" } else { "Enter send" };
        let hint = if !badge.is_empty() {
            badge.to_owned()
        } else if home {
            String::new()
        } else if meta.width >= 70 {
            format!("/ commands · @ files · {submit}")
        } else if meta.width >= 42 {
            format!("/ commands · {submit}")
        } else {
            String::new()
        };
        let hint = elide(&hint, (meta.width as usize).saturating_sub(12));
        let model_label = crate::text::elide_tail(
            model,
            (meta.width as usize)
                .saturating_sub(hint.width() + 2)
                .min(48),
        );
        let gap = (meta.width as usize).saturating_sub(model_label.width() + hint.width());
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    model_label,
                    Style::default().fg(if home { BRAND_VIOLET } else { ACCENT }),
                ),
                Span::raw(" ".repeat(gap)),
                Span::styled(hint, Style::default().fg(MUTED)),
            ])),
            meta,
        );
    }
    if focus && inner.width > 0 && inner.height > 0 {
        f.set_cursor_position((
            inner.x + col.min(inner.width.saturating_sub(1) as usize) as u16,
            inner.y + (row - top) as u16,
        ));
    }
}
/// Session orientation and task progress; full diagnostics stay in the dashboard.
fn sidebar(f: &mut Canvas, area: Rect, v: &View) -> (Option<Rect>, Option<Rect>) {
    let block = Block::default()
        .padding(ratatui::widgets::Padding::new(2, 2, 1, 1))
        .style(Style::default().bg(CODE_SURFACE));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let done = v
        .session
        .todos
        .items()
        .iter()
        .filter(|t| t.completed)
        .count();
    let mut heading = Vec::new();
    let title = v.session.title.as_deref().unwrap_or("New session");
    if inner.height >= 8 {
        for text in wrap_todo_text(title, inner.width as usize)
            .into_iter()
            .take(2)
        {
            heading.push(Line::from(Span::styled(
                text,
                Style::default().fg(TEXT).bold(),
            )));
        }
        heading.push(Line::default());
    }
    // Context has one compact home in the footer; this panel focuses on tasks.
    let title_y = inner.y + heading.len() as u16;
    heading.push(Line::from(vec![
        Span::styled(
            format!("Todo · {done}/{}", v.session.todos.items().len()),
            Style::default().fg(TEXT).bold(),
        ),
        Span::styled("  ^T", Style::default().fg(MUTED)),
    ]));
    heading.push(Line::default());
    let heading_height = heading.len() as u16;
    f.render_widget(Paragraph::new(heading), inner);
    let body = Rect::new(
        inner.x,
        inner.y.saturating_add(heading_height),
        inner.width,
        inner.height.saturating_sub(heading_height),
    );
    let current = v.session.todos.items().iter().position(|t| !t.completed);
    let mut lines = Vec::new();
    for (i, todo) in v.session.todos.items().iter().enumerate() {
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
        lines.push(Line::default());
    }
    let scroll = v
        .session
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
        Some(Rect::new(inner.x, title_y, inner.width, 1)),
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
            let split = line
                .rfind(char::is_whitespace)
                .filter(|&at| at > 0 && !line[..at].trim().is_empty());
            if !g.chars().all(char::is_whitespace) {
                if let Some(at) = split {
                    let rest = line[at..].trim_start().to_owned();
                    result.push(line[..at].trim_end().to_owned());
                    line = rest;
                    used = line.width();
                } else {
                    result.push(std::mem::take(&mut line).trim_end().to_owned());
                    used = 0;
                }
            } else {
                result.push(std::mem::take(&mut line).trim_end().to_owned());
                used = 0;
            }
            // The whitespace that triggered the break belongs to the end of
            // the previous line; keeping it would shift the continuation one
            // column past the 4-space indent and misalign wrapped rows.
            if g.chars().all(char::is_whitespace) {
                continue;
            }
        }
        // Wider than a whole line: drop instead of overflowing the width.
        if gw > width {
            continue;
        }
        line.push_str(g);
        used += gw;
    }
    result.push(line);
    result
}

/// Narrow-screen single-line TODO dock (between ask and input).
fn draw_narrow_todo_dock(f: &mut Canvas, area: Rect, v: &View) {
    let done = v
        .session
        .todos
        .items()
        .iter()
        .filter(|t| t.completed)
        .count();
    let total = v.session.todos.items().len();
    let next = v
        .session
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

/// Wrapped help with one stored offset matching the visible scroll position.
fn help(f: &mut Canvas, area: Rect, scroll: &mut u16) {
    let rect = crate::picker::centered(area, 78, area.height.saturating_sub(2) as usize);
    let text="Enter          Send / steer; confirm reply\nCtrl-J/Alt-Enter  Newline (paste preserves newlines)\nArrows/Home/End  Move cursor; Backspace/Delete\nCtrl-A/E/B/F   Line start/end · char back/fwd\nCtrl-W/U/K     Del word · to line start/end\nAlt-B/F/D·Ctrl-Left/Right  Word move · del word\nUp/Down·Ctrl-P/N  History (or row move in multiline)\nPgUp / PgDn     Scroll conversation\nCtrl-End        Follow newest output\nCtrl-Home       Jump to latest question\nCtrl-Up/Down    Previous / next question\nCtrl-G          Toggle YOLO between turns\nF2              Cycle configured models\nCtrl-X          Edit prompt in $VISUAL/$EDITOR\n/               Command menu · Up/Down · Tab/Enter\nF6/Shift-F6·Click  Select next/prev · expand block\nCtrl-O / Ctrl-R  Toggle selected block / thinking\nCtrl-T          Toggle Todo panel\nCtrl-B          Toggle stats dashboard overlay\nCtrl-Y          Cycle color theme\nCtrl-V          Paste image from clipboard (Esc clears)\n@               Reference a file (text inlined; images/PDF attached)\nMouse drag      Release to copy automatically\nEsc / Ctrl-C    Cancel exec / clear selection / close\nAlt-PgUp/PgDn   Scroll approval details\nCtrl-Q          Quit\n\n/queue TEXT     Schedule a follow-up turn\n/continue       Retry pending execution failures\n/editor         Edit the draft in $VISUAL/$EDITOR\n/compact        Compact idle conversation\n/new · /clear   Fresh context; previous session saved\n/yolo [on|off]   Change permissions between turns\n/theme          Theme picker (or /theme NAME)\n/models         Switch model then thinking effort (picker)\n/sessions       Switch sessions (Ctrl-D asks to delete)\n/status         Same as Ctrl-B dashboard\n/help           This help · Esc closes\n\nApprovals: y allow once · a always this session · n deny (YOLO skips approvals).";
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
    let offset = (*scroll as usize).min(
        lines
            .len()
            .saturating_sub(rect.height.saturating_sub(2) as usize),
    ) as u16;
    *scroll = offset;
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
                v.session.navigation.set_offset(100 + (i % 30) * 3);
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
            v.session.navigation.set_offset(100 + i * 3);
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
        v.session.context_usage = Some(yourai_harness::runtime::ContextUsage {
            estimated_tokens: 32_000,
            context_window: Some(128_000),
            input_budget: Some(119_000),
            output_reserve: 8_000,
        });
        v.session.model_metrics.calls = 8;
        v.session.model_metrics.requests.failed = 3;
        v.session.model_metrics.requests.rate_limited = 3;
        v.session.model_metrics.requests.completed = 5;
        v.session
            .model_metrics
            .requests
            .last_output_tokens_per_second = Some(47.5);
        v.session.model_metrics.requests.cache_read_tokens = 80;
        v.session.model_metrics.requests.cache_known_input_tokens = 100;
        v.session.model_metrics.requests.cache_reported_responses = 1;
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
                terminal.backend().buffer()[(1, 0)].bg,
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
        v.draft.insert("/");
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
        assert_eq!(v.draft.text(), "/queue ");
        v.draft.set_text("");
        v.toast = None;
        v.session.context_usage.as_mut().unwrap().context_window = None;
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
    fn model_label_rides_the_composer_meta_row() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut renderer = Renderer::default();
        let mut v = View::default();
        v.model.label = "gateway/kimi-k3".into();
        let m = Metadata {
            session: "session-123".into(),
            cwd: "/workspace".into(),
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
        // The active model is always visible; a staged attachment shares the row.
        assert!(
            text.contains("gateway/kimi-k3"),
            "model label missing from the input area"
        );
        assert!(text.contains("Ask anything…"), "placeholder still visible");
        v.draft
            .stage_image(super::super::clipboard::ClipboardImage {
                mime: "image/png".into(),
                data: "aGVsbG8=".into(),
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
        assert!(text.contains("1 img · Esc clears"));
        assert!(text.contains("gateway/kimi-k3"));
    }

    #[test]
    fn centered_home_keeps_cursor_menus_and_first_turn_inside_their_regions() {
        let m = Metadata {
            session: "home".into(),
            cwd: "/workspace".into(),
            trusted_shell: false,
            yolo: true,
        };
        for (width, height) in [(30, 10), (50, 16), (68, 26), (96, 26), (120, 35)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut renderer = Renderer::default();
            let mut view = View::default();
            view.draft.insert(&"long draft 中文 ".repeat(50));
            terminal
                .draw(|f| renderer.draw(f, &mut view, &m, &SessionStatus::Idle, 0, false))
                .unwrap();
            let home = renderer
                .layout
                .home
                .expect("empty session uses home layout");
            let input = renderer.layout.rows[3];
            let cursor = terminal.get_cursor_position().unwrap();
            assert!(home.logo.bottom() <= input.y);
            assert_eq!(home.footer.x, input.x + 3);
            assert_eq!(home.footer.right(), input.right() - 1);
            assert!(home.footer.y > input.bottom());
            assert!(home.footer.bottom() < height);
            if let Some(metrics) = home.metrics {
                assert!(metrics.bottom() < input.y);
            }
            if let Some(suggestions) = home.suggestions {
                assert_eq!(suggestions.x, home.footer.x);
                assert_eq!(suggestions.width, home.footer.width);
                assert!(suggestions.y > home.footer.bottom());
                assert!(suggestions.bottom() < height);
            }
            assert!(cursor.x >= input.x + 3 && cursor.x < input.right() - 1);
            assert!(cursor.y > input.y && cursor.y < input.bottom() - 1);
            let row = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(
                row.matches("YOLO").count() == 1,
                "home must preserve permission mode: {row}"
            );
            view.draft.set_text("/");
            terminal
                .draw(|f| renderer.draw(f, &mut view, &m, &SessionStatus::Idle, 0, false))
                .unwrap();
            assert_eq!(
                renderer.layout.command_area.unwrap().bottom(),
                renderer.layout.rows[3].y
            );
            view.draft.set_text("");
            view.notice(Level::Info, "Model switched to example/model");
            terminal
                .draw(|f| renderer.draw(f, &mut view, &m, &SessionStatus::Idle, 0, false))
                .unwrap();
            assert!(
                renderer.layout.home.is_some(),
                "local setup keeps the home editor in place"
            );
            view.user("First prompt", false);
            terminal
                .draw(|f| renderer.draw(f, &mut view, &m, &SessionStatus::Idle, 0, false))
                .unwrap();
            assert!(renderer.layout.home.is_none());
            assert_eq!(renderer.layout.rows[3].x, 0);
            assert_eq!(renderer.layout.rows[3].width, width);
            assert_eq!(renderer.layout.rows[3].bottom(), height - 1);
        }
    }

    #[test]
    fn home_alignment_and_theme_mapping_follow_the_actual_editor() {
        let m = Metadata {
            session: "home".into(),
            cwd: "/workspace/project".into(),
            trusted_shell: false,
            yolo: false,
        };
        let mut brand_pairs = std::collections::HashSet::new();
        for &theme in Theme::ALL.iter().filter(|t| **t != Theme::System) {
            let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
            let mut renderer = Renderer::default();
            let mut view = View::default();
            view.theme = theme;
            view.model.label = "example/coding-model".into();
            view.session.context_usage = Some(yourai_harness::runtime::ContextUsage {
                estimated_tokens: 0,
                context_window: Some(128_000),
                input_budget: Some(119_000),
                output_reserve: 8_000,
            });
            terminal
                .draw(|f| renderer.draw(f, &mut view, &m, &SessionStatus::Idle, 0, false))
                .unwrap();
            let home = renderer.layout.home.unwrap();
            let input = renderer.layout.rows[3];
            let buffer = terminal.backend().buffer();
            let first_text_x = |y| {
                (0..120)
                    .find(|&x| {
                        let symbol = buffer[(x, y)].symbol();
                        !symbol.trim().is_empty() && symbol != "│"
                    })
                    .unwrap()
            };
            assert_eq!(
                first_text_x(input.y + 1),
                home.footer.x,
                "{} prompt",
                theme.name()
            );
            assert_eq!(
                first_text_x(input.bottom() - 1),
                home.footer.x,
                "{} model",
                theme.name()
            );
            assert_eq!(
                first_text_x(home.footer.y),
                home.footer.x,
                "{} shortcuts",
                theme.name()
            );
            assert_eq!(
                first_text_x(home.suggestions.unwrap().y),
                home.footer.x,
                "{} suggestions",
                theme.name()
            );
            let version_end = (0..120)
                .rfind(|&x| !buffer[(x, 31)].symbol().trim().is_empty())
                .unwrap()
                + 1;
            assert_eq!(version_end, home.footer.right(), "{} version", theme.name());
            assert_eq!(buffer[(0, 0)].bg, theme.color(BG));
            assert_eq!(buffer[(input.x + 1, input.y)].bg, theme.color(PANEL));
            assert_eq!(
                buffer[(home.footer.x, input.bottom() - 1)].fg,
                theme.color(BRAND_VIOLET)
            );
            for slot in [BRAND_TEAL, BRAND_VIOLET] {
                assert!(
                    buffer
                        .content()
                        .iter()
                        .any(|cell| cell.symbol() == "█" && cell.fg == theme.color(slot)),
                    "{} wordmark",
                    theme.name()
                );
            }
            assert!(
                brand_pairs.insert((theme.color(BRAND_TEAL), theme.color(BRAND_VIOLET))),
                "{} brand hues should be distinctive",
                theme.name()
            );
            if let Ok(prefix) = std::env::var("YOURAI_HOME_THEME_SNAPSHOT") {
                let cells = buffer.content().iter().map(|c| json!({"text":c.symbol(),"fg":format!("{:?}",c.fg),"bg":format!("{:?}",c.bg),"bold":c.modifier.contains(Modifier::BOLD)})).collect::<Vec<_>>();
                std::fs::write(
                    format!("{prefix}-{}.json", theme.name()),
                    serde_json::to_vec(&json!({"width":120,"height":32,"cells":cells})).unwrap(),
                )
                .unwrap();
            }
        }
    }

    #[test]
    fn welcome_footer_and_live_activity() {
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        let mut renderer = Renderer::default();
        let mut v = View::default();
        v.model.label = "pai/Kimi-k3[300k]".into();
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
        assert!(screen.iter().any(|r| r.contains("WORKSPACE")));
        assert!(screen.iter().any(|r| r.contains("/sessions")));
        assert!(screen.iter().any(|r| r.contains("pai/Kimi-k3[300k]")));
        assert!(screen.iter().any(|r| r.contains("/workspace")));
        assert!(screen.iter().any(|r| r.contains("MODEL CAPACITY")));
        assert!(
            screen.iter().any(|r| r.contains("Capacity unavailable")),
            "model labels cannot imply context capacity"
        );
        assert!(screen.iter().any(|r| r.contains("Ask before execution")));
        let home_text = screen.join("\n");
        assert_eq!(home_text.matches("/workspace").count(), 1);
        assert!(!home_text.contains("SETUP"));
        assert!(!home_text.contains("Theme ·"));
        assert!(screen[0].contains("New session"));
        assert!(screen[31].contains(env!("CARGO_PKG_VERSION")));
        assert!(renderer.layout.rows[3].x > 0, "home composer is centered");
        assert!(
            renderer.layout.rows[3].bottom() < 27,
            "home composer leaves space below"
        );

        assert!(!v.overlay.is_open());
        // Runtime metadata refreshes the home cards without moving the editor.
        let input_before = renderer.layout.rows[3];
        v.session.context_usage = Some(yourai_harness::runtime::ContextUsage {
            estimated_tokens: 18_000,
            context_window: Some(300_000),
            input_budget: Some(291_000),
            output_reserve: 8_000,
        });
        v.model_choices = (0..3)
            .map(|index| crate::models::ModelChoice {
                id: format!("example/model-{index}"),
                variant: None,
                label: format!("example/model-{index}"),
                effort: None,
            })
            .collect();
        terminal
            .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
            .unwrap();
        assert_eq!(renderer.layout.rows[3], input_before);
        let refreshed = rows(&terminal).join("\n");
        assert!(refreshed.contains(&format!("{} token window", tokens(300_000))));
        assert!(refreshed.contains("No messages sent"));
        assert!(
            !refreshed.contains(&tokens(18_000)),
            "system/tool estimate is not conversation usage"
        );
        assert!(!refreshed.contains("6% used"));
        assert!(!refreshed.contains("reserve"));
        assert!(!refreshed.contains("3 model profiles"));
        assert_eq!(refreshed.matches("/workspace").count(), 1);
        if let Ok(path) = std::env::var("YOURAI_WELCOME_SNAPSHOT") {
            let cells = terminal.backend().buffer().content().iter().map(|c| json!({"text":c.symbol(),"fg":format!("{:?}",c.fg),"bg":format!("{:?}",c.bg),"bold":c.modifier.contains(Modifier::BOLD)})).collect::<Vec<_>>();
            std::fs::write(
                path,
                serde_json::to_vec(&json!({"width":120,"height":32,"cells":cells})).unwrap(),
            )
            .unwrap();
        }
        v.user("Review code", false);
        v.session.active = true;
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
        v.session.navigation.anchor(0, 0);
        v.session.navigation.reveal(Some(0));

        v.follow();
        v.session.navigation.resolve(100, 10, &[(0, 0)], &[]);
        assert_eq!(v.session.navigation.offset(), 0);
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
        // Expanding the tool must not expand supporting reasoning.
        assert!(!renderer
            .layout
            .lines
            .iter()
            .any(|l| l.to_string().contains("private-thought")));
        let (thought_rect, thought_id) = renderer.layout.hits[0];
        super::super::app::click_dispatch(
            &mut renderer,
            &mut view,
            thought_rect.x + 2,
            thought_rect.y,
        );
        terminal
            .draw(|f| renderer.draw(f, &mut view, &m, &SessionStatus::Idle, 0, false))
            .unwrap();
        assert!(renderer
            .layout
            .lines
            .iter()
            .any(|l| l.to_string().contains("private-thought")));
        view.toggle(thought_id);
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
        v.session.title = Some("Fix parser crash".into());
        let mut todos: Vec<Todo> = (0..9)
            .map(|i| Todo {
                id: format!("t{i}"),
                text: format!("Task number {i}"),
                completed: i == 0,
            })
            .collect();
        v.session.todos.set(todos.clone());
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
        assert!(!v.session.todos.panel);
        let text = draw(&mut terminal, &mut renderer, &mut v).join("\n");
        // With panel off, Todo items are not visible.
        assert!(!text.contains("Task number 0"));
        // Re-open via the field directly (wide screen has no dock to click).
        v.session.todos.panel = true;
        draw(&mut terminal, &mut renderer, &mut v);
        // Wheel over the TODO list scrolls it, not the transcript.
        let area = renderer.layout.todo_area.unwrap();
        super::super::app::wheel_dispatch(&mut renderer, &mut v, area.x, area.y, false);
        assert_eq!(v.session.todos.scroll(), 1);
        todos.retain(|t| t.id != "t8");
        v.session.todos.set(todos);
        assert_eq!(v.session.todos.scroll(), 1);
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
            v.model.label = "pai/Kimi-k3[300k]".into();
            v.session.title = Some("Parser boundary review".into());
            v.session.context_usage = Some(yourai_harness::runtime::ContextUsage {
                estimated_tokens: 106_394,
                context_window: Some(300_000),
                input_budget: None,
                output_reserve: 0,
            });
            v.user("Fix the parser and run tests", false);
            v.event(Out::Reasoning {
                text: "Inspect the parser boundary and preserve existing behavior.".into(),
            });
            v.session.todos.set(vec![
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
            v.draft
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
                assert!(content.contains("Parser boundary review"));
                assert!(!content.contains("106.4K tokens"));
                assert!(!content.contains("35% used"));
                assert_eq!(content.matches("ctx 35%").count(), 1);
                let title = renderer
                    .layout
                    .todo_hit
                    .expect("task title remains clickable");
                assert!(title.y < renderer.layout.panel.unwrap().y + 6);
                assert!(content.contains("│"), "composer left rail present");
                assert!(
                    !content.contains("╹"),
                    "composer uses a continuous focus rail"
                );
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
        v.model.label = "example/coding-model".into();
        v.user("Fix the parser boundary and verify the change.", false);
        v.session.active = true;
        v.event(Out::Reasoning {
            text: "Inspect the empty-input branch before editing.".into(),
        });
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
        v.event(Out::Message { text:"## Parser boundary updated\n\nThe empty-input case **still needs a fix**.\n\n### Verification\n- Build passed.\n- **Parser test failed**: empty input triggers an assertion.\n\n### Next step\nHandle empty input, then rerun the parser tests.".into() });
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
        assert!(!content.contains("You"));
        assert!(!content.contains("YourAI"));
        assert!(content.contains("Failed · $ cargo test parser"));
        assert!(content.contains("Read & search · 2 operations"));
        assert!(!content.contains("Inspect the empty-input branch"));
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
            v.session.navigation.set_offset(scroll);
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
        assert_eq!(v.session.navigation.offset(), top);
        super::super::app::wheel_dispatch(&mut renderer, &mut v, 1, 1, false);
        assert_eq!(v.session.navigation.offset(), top - 3);
        let hit = renderer.layout.follow_hit.unwrap();
        super::super::app::click_dispatch(&mut renderer, &mut v, hit.x, hit.y);
        assert_eq!(v.session.navigation.offset(), 0);
        terminal
            .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
            .unwrap();
        assert!(renderer.layout.follow_hit.is_none());
        renderer.latest_turn(&mut v);
        terminal
            .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
            .unwrap();
        let start = renderer.layout.lines.len()
            - v.session.navigation.offset()
            - renderer.layout.transcript.height as usize;
        assert_eq!(start, renderer.timeline.turns[0].0);
    }

    #[test]
    fn composer_grows_across_full_width_and_keeps_cursor_above_single_footer() {
        let mut v = View::default();
        v.theme = Theme::Dark;
        v.session.todos.set(vec![super::super::state::Todo {
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
                v.draft.set_text("");
                v.draft.insert(text);
                terminal
                    .draw(|f| renderer.draw(f, &mut v, &m, &SessionStatus::Idle, 0, false))
                    .unwrap();
                let cursor = terminal.get_cursor_position().unwrap();
                let buffer = terminal.backend().buffer();
                let prompt_y = (0..height)
                    .find(|&y| buffer[(0, y)].symbol() == "│")
                    .expect("composer left rail remains visible")
                    + 1; // the first text row under the rail's breathing room
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
                    let input_height =
                        (if height >= 16 { 4 } else { 3 }).min((height / 3).clamp(3, 8));
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
        v.session.active = true;
        v.session.since = Some(std::time::Instant::now());
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
        view.session.navigation.set_offset(300);
        renderer.prepare(area, &mut view, &meta, FrameTime::now(), 0, false);
        let top = |r: &Renderer, v: &View| {
            r.layout.lines.len()
                - v.session.navigation.offset()
                - r.layout.transcript.height as usize
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
            - view.session.navigation.offset()
            - renderer.layout.transcript.height as usize;
        assert_eq!(start, renderer.timeline.turns[1].0);
        renderer.jump_turn(&mut view, true);
        draw(&mut terminal, &mut renderer, &mut view);
        let start = renderer.layout.lines.len()
            - view.session.navigation.offset()
            - renderer.layout.transcript.height as usize;
        assert_eq!(start, renderer.timeline.turns[0].0);
        renderer.jump_turn(&mut view, false);
        draw(&mut terminal, &mut renderer, &mut view);
        assert_eq!(
            renderer.layout.lines.len()
                - view.session.navigation.offset()
                - renderer.layout.transcript.height as usize,
            renderer.timeline.turns[1].0
        );
        terminal.backend_mut().resize(40, 20);
        terminal.autoresize().unwrap();
        draw(&mut terminal, &mut renderer, &mut view);
        renderer.latest_turn(&mut view);
        draw(&mut terminal, &mut renderer, &mut view);
        let start = renderer.layout.lines.len()
            - view.session.navigation.offset()
            - renderer.layout.transcript.height as usize;
        assert_eq!(start, renderer.timeline.turns[1].0);

        view.follow();
        draw(&mut terminal, &mut renderer, &mut view);
        assert_eq!(view.session.navigation.offset(), 0);
    }
    #[test]
    fn busy_footer_carries_the_activity_status_instead_of_title_and_path() {
        let mut v = View::default();
        v.session.title = Some("session title".into());
        let m = Metadata {
            session: "s".into(),
            cwd: "/workspace".into(),
            trusted_shell: false,
            yolo: false,
        };
        let line = footer_lines(
            100,
            &v,
            &m,
            0,
            Some(("⠋ Running cargo test · 12s · esc stop", GREEN)),
        )
        .remove(0);
        let text = line
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect::<String>();
        assert!(
            text.contains("⠋ Running cargo test · 12s · esc stop"),
            "{text}"
        );
        assert!(!text.contains("session title"), "{text}");
        assert!(!text.contains("/workspace"), "{text}");
        // Idle keeps the title/path and carries no status.
        let idle = footer_lines(100, &v, &m, 0, None).remove(0);
        let idle_text = idle
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect::<String>();
        assert!(idle_text.contains("session title"), "{idle_text}");
        assert!(!idle_text.contains("esc stop"), "{idle_text}");
    }
    #[test]
    fn footer_measures_unicode_long_labels_and_large_metrics() {
        let mut v = View::default();
        v.session.title = Some("这是一个很长的会话标题 🔎 review ".repeat(6));
        v.model.label = "provider/very-long-model-name-with-reasoning-variant".repeat(3);
        v.restore_usage(
            yourai_core::prelude::Usage {
                total_tokens: u64::MAX,
                ..Default::default()
            },
            0,
        );
        v.session
            .model_metrics
            .requests
            .last_output_tokens_per_second = Some(f64::MAX);
        v.session.context_usage = Some(yourai_harness::runtime::ContextUsage {
            estimated_tokens: u64::MAX,
            context_window: Some(100),
            input_budget: None,
            output_reserve: 0,
        });
        let m = Metadata {
            session: "test".into(),
            cwd: "/Users/开发者/workspaces/很长的目录名字/YourAI-Harness".into(),
            trusted_shell: false,
            yolo: true,
        };
        for width in 30..=160 {
            let lines = footer_lines(width, &v, &m, usize::MAX, None);
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
        v.session.title = Some("Review".into());
        v.model.label = "mock/model".into();
        v.restore_usage(
            yourai_core::prelude::Usage {
                total_tokens: 1200,
                ..Default::default()
            },
            0,
        );
        v.session
            .model_metrics
            .requests
            .last_output_tokens_per_second = Some(47.5);
        let lines = footer_lines(120, &v, &m, 0, None);
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
            payload: serde_json::json!({"kind":"permission", "tool_name":"shell", "input":{"command":"cargo test"}, "reason":"Tool permission"}),
        });
        // The choice list defaults to "allow once"; deny is two Downs away.
        view.ask_mut().unwrap().permission_choice = 2;
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
        // Compact form for tiny terminals: the question and the key hint.
        assert!(text.contains("Allow shell?"), "{text}");
        assert!(
            text.contains("y once") && text.contains("a session") && text.contains("▸n deny"),
            "{text}"
        );
        assert!(
            !text.contains("Message"),
            "inactive composer should not displace approval"
        );
        // Full choice list when the terminal has room.
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
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
        assert!(text.contains("command: cargo test"), "{text}");
        assert!(text.contains("y  Allow once"), "{text}");
        assert!(text.contains("a  Allow always · this session"), "{text}");
        assert!(text.contains("n  Deny"), "{text}");
    }
    #[test]
    fn narrow_footer_preserves_permissions_and_all_dashboard_sizes_fit() {
        let mut v = View::default();
        v.user("Review context pressure", false);
        v.model.label = "provider/model".into();
        v.session.context_usage = Some(yourai_harness::runtime::ContextUsage {
            estimated_tokens: 90000,
            context_window: Some(100000),
            input_budget: Some(90000),
            output_reserve: 8000,
        });
        v.session
            .model_metrics
            .requests
            .last_output_tokens_per_second = Some(47.5);
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
            let footer_rows = footer_lines(width as usize, &v, &m, 0, None).len() as u16;
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
        view.session.active = true;
        view.session.since = Some(start);
        view.toast = Some(("copied".into(), start));
        view.session.retry = Some(RetryState {
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
