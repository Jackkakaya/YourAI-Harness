//! Dashboard and pickers, constrained to the current terminal.
use super::Canvas;
use super::{ctx_bar, ctx_color, label, tokens, Metadata};
use crate::ui::markdown::wrap_text;
use crate::ui::overlay::Overlay;
use crate::ui::state::View;
use crate::ui::theme::{Theme, ACCENT, FOCUS_SURFACE, GREEN, MUTED, PANEL, RED, TEXT, YELLOW};
use ratatui::{
    prelude::*,
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};
/// ^B dashboard overlay (replaces the old sidebar content).
pub(super) fn stats_overlay(f: &mut Canvas, area: Rect, v: &View, m: &Metadata, queued: usize) {
    let _ = queued;
    let width = area.width.min(64);
    let mut lines: Vec<Line<'static>> = vec![];
    // Session section.
    lines.push(label(&v.model.label, TEXT));
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
    lines.push(label("Context", TEXT).style(Style::default().bold()));
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
    lines.push(label("Requests · this run", TEXT).style(Style::default().bold()));
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
    lines.push(label("Tokens · session", TEXT).style(Style::default().bold()));
    lines.push(label(
        &format!(
            "{} in · {} out · {} total",
            tokens(v.usage().input_tokens),
            tokens(v.usage().output_tokens),
            tokens(v.usage().total_tokens)
        ),
        TEXT,
    ));
    if let Some((pin, pout)) = v.model.pricing {
        lines.push(label(
            &format!("Current model: ${pin}/{pout} per M in/out"),
            YELLOW,
        ));
    }
    lines.push(label(
        &format!("{} responses", v.recorded_responses()),
        MUTED,
    ));
    lines.push(Line::default());
    // Permissions.
    lines.push(label("Permissions", TEXT).style(Style::default().bold()));
    lines.push(label(
        if m.yolo {
            "YOLO · approvals skipped"
        } else if m.trusted_shell {
            "Trusted local execution"
        } else {
            "Ask before commands"
        },
        if m.yolo { YELLOW } else { TEXT },
    ));
    // Theme line.
    lines.push(label(&format!("theme {}", v.theme.label()), MUTED));
    let lines: Vec<_> = lines
        .into_iter()
        .flat_map(|line| {
            let spans = line
                .spans
                .into_iter()
                .map(|mut span| {
                    span.style = line.style.patch(span.style);
                    span
                })
                .collect();
            crate::ui::markdown::wrap_spans(spans, width.saturating_sub(2) as usize, "")
        })
        .collect();
    let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    f.render_widget(Clear, rect);
    let scroll = match v.overlay {
        Overlay::Stats { scroll } => usize::from(scroll).min(
            lines
                .len()
                .saturating_sub(rect.height.saturating_sub(2) as usize),
        ) as u16,
        _ => 0,
    };
    f.render_widget(
        Paragraph::new(lines)
            .scroll((scroll, 0))
            .style(Style::default().bg(PANEL).fg(TEXT))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(ACCENT))
                    .title(" Session · Esc / ^B · ↑↓ scroll "),
            ),
        rect,
    );
}

pub(super) fn abbreviate_home(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if path.starts_with(&home) {
            return format!("~{}", &path[home.len()..]);
        }
    }
    path.to_string()
}

/// One row of a picker list: rendered body plus whether it is the currently
/// active item (drawn with the ● mark instead of ○).
struct PickRow {
    body: String,
    current: bool,
}

/// Shared picker chrome: centered rounded box, ► selection cursor, ●/○
/// current-item mark, focused-row surface and visible-row scrolling.
/// `header` lines sit above the list (e.g. the sessions filter line);
/// `current_color` tints the current item (GREEN for themes/sessions,
/// TEXT when only the mark distinguishes it, as in models).
#[allow(clippy::too_many_arguments)] // all params are distinct view concerns
fn pick_list(
    f: &mut Canvas,
    area: Rect,
    width: u16,
    title: &str,
    header: &[Line<'static>],
    rows: &[PickRow],
    selected: usize,
    current_color: Color,
) {
    let rect = crate::picker::centered(area, width, rows.len() + header.len() + 2);
    f.render_widget(Clear, rect);
    let capacity = rect
        .height
        .saturating_sub(u16::try_from(header.len()).unwrap_or(u16::MAX) + 2);
    let start = crate::picker::visible_rows(rows.len(), selected, capacity).start;
    let mut lines: Vec<Line<'static>> = header.to_vec();
    lines.extend(
        rows.iter()
            .enumerate()
            .skip(start)
            .take(usize::from(capacity))
            .map(|(i, row)| {
                let selected_row = i == selected;
                let mark = if row.current { "●" } else { "○" };
                let color = if selected_row {
                    ACCENT
                } else if row.current {
                    current_color
                } else {
                    TEXT
                };
                let style = if selected_row {
                    Style::default().fg(color).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(color)
                };
                Line::from(vec![
                    Span::styled(
                        if selected_row { "► " } else { "  " },
                        Style::default().fg(ACCENT),
                    ),
                    Span::styled(format!("{mark} {}", row.body), style),
                ])
                .style(if selected_row {
                    Style::default().bg(FOCUS_SURFACE)
                } else {
                    Style::default()
                })
            }),
    );
    f.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(PANEL).fg(TEXT))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(ACCENT))
                    .title(title),
            ),
        rect,
    );
}

/// `/models` picker overlay.
pub(super) fn model_picker_overlay(f: &mut Canvas, area: Rect, v: &View) {
    let choices = &v.model_choices;
    if choices.is_empty() {
        let rect = crate::picker::centered(area, 54, 5);
        f.render_widget(Clear, rect);
        f.render_widget(
            Paragraph::new("No configured models.\nUse /models provider/model.\nEsc closes.")
                .style(Style::default().fg(TEXT).bg(PANEL))
                .block(Block::bordered().title(" Models ")),
            rect,
        );
        return;
    }
    let selected = match v.overlay {
        Overlay::Models(i) => i,
        _ => 0,
    };
    let rows: Vec<PickRow> = choices
        .iter()
        .map(|label| PickRow {
            body: label.clone(),
            current: label == &v.model.label,
        })
        .collect();
    pick_list(
        f,
        area,
        56,
        " Models · ↑↓ · Enter · Esc ",
        &[],
        &rows,
        selected,
        TEXT,
    );
}

/// `/sessions` picker overlay. Rows are pre-loaded into the picker state;
/// filtering is computed per-frame via the shared `filter_sessions` helper.
pub(super) fn sessions_overlay(f: &mut Canvas, area: Rect, v: &View, now: i64) {
    let Overlay::Sessions(picker) = &v.overlay else {
        return;
    };
    if let Some(row) = &picker.pending_delete {
        let rect = crate::picker::centered(area, 64, 9);
        f.render_widget(Clear, rect);
        let text = format!("Delete this session permanently?\n{}\n{}\nThis cannot be undone.\nY delete · N / Esc keep", row.title, row.id.0);
        f.render_widget(
            Paragraph::new(text)
                .wrap(ratatui::widgets::Wrap { trim: false })
                .style(Style::default().fg(TEXT).bg(PANEL))
                .block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(RED))
                        .title(" Confirm deletion "),
                ),
            rect,
        );
        return;
    }
    let filtered = crate::sessions::filter_sessions(&picker.rows, &picker.query);
    let hint = if picker.query.is_empty() {
        "type to filter · ↑↓ move · Enter switch · Esc close"
    } else {
        ""
    };
    let filter_line = Line::from(vec![
        Span::styled("filter ", Style::default().fg(MUTED)),
        Span::styled(picker.query.clone(), Style::default().fg(TEXT)),
        Span::styled(hint.to_owned(), Style::default().fg(MUTED)),
    ]);
    let mut header = vec![filter_line, Line::default()];
    if filtered.is_empty() {
        header.push(Line::from(Span::styled(
            "No sessions match.",
            Style::default().fg(YELLOW),
        )));
    }
    // Title column shrinks with the terminal; id/model/time are fixed.
    let title_w = (usize::from(area.width.min(80).saturating_sub(4)))
        .saturating_sub(36)
        .clamp(8, 32);
    let rows: Vec<PickRow> = filtered
        .iter()
        .map(|&idx| {
            let row = &picker.rows[idx];
            PickRow {
                body: crate::sessions::row_body(row, title_w, now),
                current: row.is_current,
            }
        })
        .collect();
    pick_list(
        f,
        area,
        80,
        " Sessions · filter · ↑↓ · Enter · Ctrl-D del · Esc ",
        &header,
        &rows,
        picker.selected,
        GREEN,
    );
}

/// `/theme` picker overlay: lists every theme in `Theme::ALL` with the
/// currently active one marked. Enter applies immediately (live preview).
pub(super) fn theme_picker_overlay(f: &mut Canvas, area: Rect, v: &View) {
    let all = Theme::ALL;
    let selected = match v.overlay {
        Overlay::Themes(i) => i,
        _ => 0,
    };
    let rows: Vec<PickRow> = all
        .iter()
        .map(|t| PickRow {
            body: t.label().to_owned(),
            current: *t == v.theme,
        })
        .collect();
    pick_list(
        f,
        area,
        40,
        " Themes · ↑↓ · Enter · Esc ",
        &[],
        &rows,
        selected,
        GREEN,
    );
}

/// Ask/reply panel: permission prompts and structured replies. Rendered
/// above the input editor while the harness is waiting on the user.
pub(super) fn ask_overlay(f: &mut Canvas, area: Rect, v: &View) {
    let Some(ask) = v.ask() else { return };
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
    let inner = block.inner(area);
    f.render_widget(block, area);
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
        lines.extend(wrap_text(
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

#[cfg(test)]
mod tests {
    use super::super::Renderer;
    #[allow(clippy::wildcard_imports)]
    use super::*;
    use crate::sessions::SessionRow;
    use crate::ui::state::SessionPickerState;
    use ratatui::backend::TestBackend;
    use yourai_core::prelude::{SessionId, SessionStatus};

    fn sessions_at_epoch(f: &mut Canvas, area: Rect, view: &View) {
        sessions_overlay(f, area, view, 0);
    }
    #[test]
    fn pickers_fit_small_terminals_and_scroll_to_selected_rows() {
        let mut v = View::default();
        v.model_choices = (0..100).map(|i| format!("model-{i:03}")).collect();
        v.overlay = Overlay::Sessions(SessionPickerState {
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
                for draw in [
                    model_picker_overlay,
                    sessions_at_epoch,
                    theme_picker_overlay,
                ] {
                    t.draw(|f| {
                        let mut canvas = Canvas::new(f.area());
                        draw(&mut canvas, f.area(), &v);
                        canvas.paint(f);
                    })
                    .unwrap();
                }
            }
        }
        let mut t = Terminal::new(TestBackend::new(80, 12)).unwrap();
        for (draw, expected) in [
            (sessions_at_epoch as fn(&mut Canvas, Rect, &View), "row-099"),
            (model_picker_overlay, "model-099"),
        ] {
            let previous = std::mem::replace(&mut v.overlay, Overlay::Models(99));
            if expected == "row-099" {
                v.overlay = previous;
            }
            t.draw(|f| {
                let mut canvas = Canvas::new(f.area());
                draw(&mut canvas, f.area(), &v);
                canvas.paint(f);
            })
            .unwrap();
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

    #[test]
    fn sessions_overlay_renders_rows_and_filters() {
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
}
