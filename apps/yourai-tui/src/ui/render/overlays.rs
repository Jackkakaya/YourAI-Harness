//! Dashboard and pickers, constrained to the current terminal.
use super::footer::{ctx_bar, ctx_color, label, tokens};
use super::Canvas;
use super::Metadata;
use crate::ui::markdown::wrap_text;
use crate::ui::overlay::Overlay;
use crate::ui::state::{Ask, View};
use crate::ui::theme::{Theme, ACCENT, FOCUS_SURFACE, GREEN, MUTED, PANEL, RED, TEXT, YELLOW};
use ratatui::{
    prelude::*,
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};
use serde_json::Value;
/// ^B dashboard overlay (replaces the old sidebar content).
pub(super) fn stats_overlay(f: &mut Canvas, area: Rect, v: &View, m: &Metadata) {
    let width = area.width.min(64);
    let mut lines: Vec<Line<'static>> = vec![];
    // Session section.
    lines.push(label(
        &match &v.model.effort {
            Some(effort) => format!("{} · {}", v.model.label, effort),
            None => v.model.label.clone(),
        },
        TEXT,
    ));
    lines.push(label(&format!("cwd {}", abbreviate_home(&m.cwd)), MUTED));
    if let Some(git) = v.git.stats_line() {
        lines.push(label(&git, MUTED));
    }
    lines.push(label(
        &format!(
            "session {} · {} calls",
            m.session.chars().take(8).collect::<String>(),
            v.session.model_metrics.calls
        ),
        MUTED,
    ));
    lines.push(Line::default());
    // Context section.
    lines.push(label("Context · next request estimate", TEXT).style(Style::default().bold()));
    if v.session.compaction.is_some() {
        lines.push(label("Recalculating after context maintenance…", MUTED));
    } else if let Some(usage) = &v.session.context_usage {
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
                &format!("{} estimated · limit unknown", tokens(used)),
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
    lines.push(label("Includes system prompt, tools and history", MUTED));
    lines.push(label("Not billed token usage", MUTED));
    if let Some(result) = &v.session.last_compaction {
        lines.push(Line::default());
        lines.push(label("Last context maintenance", TEXT).style(Style::default().bold()));
        lines.push(label(
            &format!(
                "{} summarized · {} kept · {} outputs cleared",
                result.summarized_messages, result.retained_messages, result.pruned_outputs
            ),
            MUTED,
        ));
        lines.push(label(
            &format!(
                "{} summary calls · {}",
                result.model_calls,
                if result.stop_reason.is_some() {
                    "continuation stopped"
                } else {
                    "complete"
                }
            ),
            MUTED,
        ));
    }
    let metrics = &v.session.model_metrics.requests;
    if metrics.journal_errors > 0 {
        lines.push(label("Request log write failed", RED));
    }
    lines.push(Line::default());
    // Requests section.
    lines.push(label("Requests · this run", TEXT).style(Style::default().bold()));
    lines.push(label(
        &format!(
            "{} calls · {} in last 60s",
            v.session.model_metrics.calls, metrics.attempts_last_minute
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
        .map(|choice| PickRow {
            body: match &choice.effort {
                Some(effort) => format!("{} · {}", choice.label, effort),
                None => choice.label.clone(),
            },
            current: choice.label == v.model.label,
        })
        .collect();
    pick_list(
        f,
        area,
        56,
        " Models · ↑↓ · Enter · Tab effort · Esc ",
        &[],
        &rows,
        selected,
        TEXT,
    );
}

/// Thinking-effort sub-picker for the highlighted `/models` entry.
pub(super) fn effort_picker_overlay(f: &mut Canvas, area: Rect, v: &View) {
    let Overlay::Effort { model, selected } = v.overlay else {
        return;
    };
    let Some(choice) = v.model_choices.get(model) else {
        return;
    };
    let header = vec![
        Line::from(Span::styled(
            crate::text::elide_tail(&choice.label, 40),
            Style::default().fg(TEXT),
        )),
        Line::default(),
    ];
    let rows: Vec<PickRow> = crate::models::EFFORT_CHOICES
        .iter()
        .map(|level| PickRow {
            body: match level {
                None => "config default".to_string(),
                Some(keyword) => (*keyword).to_string(),
            },
            // Marks the entry's effective level; "config default" is current
            // when no explicit effort applies.
            current: *level == choice.effort.as_deref(),
        })
        .collect();
    pick_list(
        f,
        area,
        44,
        " Thinking effort · ↑↓ · Enter · Esc back ",
        &header,
        &rows,
        selected,
        GREEN,
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
        let text = format!("Delete this session permanently?\n{}\n{}\nThis cannot be undone.\nY delete · N / Esc keep", row.title, row.id.as_str());
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
    if ask.permission() {
        permission_overlay(f, area, v, ask);
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(PANEL))
        .title(" Reply · plain text; /json for structured replies ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let parts = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);
    let mut lines = Vec::new();
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

/// Permission approval: what the tool wants to do, then a y/a/n choice list.
/// Free-text editing is intentionally absent — one keypress answers. Terminals
/// too short for the full list fall back to a compact one-line form; the
/// y/a/n keys work identically either way.
fn permission_overlay(f: &mut Canvas, area: Rect, _v: &View, ask: &Ask) {
    let tool = ask.payload["tool_name"].as_str().unwrap_or("tool");
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(PANEL))
        .title(" Approval required ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let compact = inner.height < 6;
    let parts = if compact {
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)])
            .split(inner)
            .to_vec()
    } else {
        Layout::vertical([
            Constraint::Min(2),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(inner)
        .to_vec()
    };
    let width = parts[0].width as usize;
    let mut lines = vec![Line::from(Span::styled(
        format!("Allow {tool}?"),
        Style::default().fg(ACCENT).bold(),
    ))];
    if !compact {
        // The default reason carries no information; only unusual ones (e.g.
        // doom-loop repeats) earn a row.
        if let Some(reason) = ask.payload["reason"]
            .as_str()
            .filter(|r| !r.is_empty() && *r != "Tool permission")
        {
            lines.push(Line::from(Span::styled(reason, Style::default().fg(MUTED))));
        }
    }
    // The tool's arguments as compact key: value rows — not raw JSON.
    if let Some(input) = ask.payload.get("input").and_then(Value::as_object) {
        let budget = parts[0].height.saturating_sub(lines.len() as u16) as usize;
        let mut shown = 0;
        for (key, value) in input {
            if shown >= budget {
                lines.push(Line::from(Span::styled("…", Style::default().fg(MUTED))));
                break;
            }
            let text = match value {
                Value::String(s) => s.clone(),
                Value::Null => continue,
                other => {
                    let compact = serde_json::to_string(other).unwrap_or_default();
                    if compact.len() > 160 {
                        format!("{}…", compact.chars().take(160).collect::<String>())
                    } else {
                        compact
                    }
                }
            };
            if text.trim().is_empty() {
                continue;
            }
            lines.extend(wrap_text(
                &format!("{key}: {text}"),
                Style::default().fg(TEXT),
                width,
                "  ",
            ));
            shown += 1;
        }
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
    if compact {
        f.render_widget(
            Paragraph::new("y once · a always · n deny").style(Style::default().fg(MUTED)),
            parts[1],
        );
        return;
    }
    let selected = ask.permission_choice.min(2);
    let rows = [
        ("y", "Allow once"),
        ("a", "Allow always · this session"),
        ("n", "Deny"),
    ];
    for (row, (key, label)) in rows.iter().enumerate() {
        let at = selected == row;
        let mut spans = vec![Span::styled(
            if at { "▸ " } else { "  " },
            Style::default().fg(ACCENT),
        )];
        spans.push(Span::styled(
            format!("{key}  "),
            Style::default().fg(if at { ACCENT } else { MUTED }).bold(),
        ));
        spans.push(Span::styled(
            *label,
            Style::default()
                .fg(if at { TEXT } else { MUTED })
                .add_modifier(if at {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ));
        f.render_widget(
            Paragraph::new(Line::from(spans)).style(if at {
                Style::default().bg(FOCUS_SURFACE)
            } else {
                Style::default().bg(PANEL)
            }),
            Rect::new(parts[1].x, parts[1].y + row as u16, parts[1].width, 1),
        );
    }
    f.render_widget(
        Paragraph::new("y/a/n direct · ↑↓ Enter · Alt-PgUp/PgDn details · Esc interrupts")
            .style(Style::default().fg(MUTED)),
        parts[2],
    );
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
        v.model_choices = (0..100)
            .map(|i| crate::models::ModelChoice {
                id: format!("model-{i:03}"),
                label: format!("model-{i:03}"),
                variant: None,
                effort: None,
            })
            .collect();
        v.overlay = Overlay::Sessions(SessionPickerState {
            pending_delete: None,
            rows: (0..100)
                .map(|i| crate::sessions::SessionRow {
                    id: yourai_core::prelude::SessionId::from(format!("session-{i:03}")),
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
    fn effort_picker_renders_entry_levels_and_survives_small_terminals() {
        let mut v = View::default();
        v.model_choices = vec![crate::models::ModelChoice {
            id: "gateway/kimi-k3".into(),
            variant: Some("deep".into()),
            label: "gateway/kimi-k3 · deep".into(),
            effort: Some("high".into()),
        }];
        v.overlay = Overlay::Effort {
            model: 0,
            selected: 5,
        };
        for width in [1, 20, 44, 80] {
            for height in [1, 2, 3, 8, 24] {
                let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
                t.draw(|f| {
                    let mut canvas = Canvas::new(f.area());
                    effort_picker_overlay(&mut canvas, f.area(), &v);
                    canvas.paint(f);
                })
                .unwrap();
            }
        }
        let mut t = Terminal::new(TestBackend::new(60, 16)).unwrap();
        t.draw(|f| {
            let mut canvas = Canvas::new(f.area());
            effort_picker_overlay(&mut canvas, f.area(), &v);
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
        assert!(text.contains("kimi-k3"), "entry label is the header");
        assert!(text.contains("config default"));
        assert!(text.contains("xhigh"), "levels are listed");
        // The current row is marked and the effort rides along in the row body.
        assert!(text.contains("high"));
    }

    #[test]
    fn model_picker_rows_carry_the_effective_effort() {
        let mut v = View::default();
        v.model_choices = vec![
            crate::models::ModelChoice {
                id: "gateway/kimi-k3".into(),
                variant: None,
                label: "gateway/kimi-k3".into(),
                effort: Some("high".into()),
            },
            crate::models::ModelChoice {
                id: "gateway/glm-4.6".into(),
                variant: None,
                label: "gateway/glm-4.6".into(),
                effort: None,
            },
        ];
        v.overlay = Overlay::Models(0);
        let mut t = Terminal::new(TestBackend::new(60, 12)).unwrap();
        t.draw(|f| {
            let mut canvas = Canvas::new(f.area());
            model_picker_overlay(&mut canvas, f.area(), &v);
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
            text.contains("gateway/kimi-k3 · high"),
            "effort is appended to the row: {text}"
        );
        assert!(text.contains("gateway/glm-4.6"));
        assert!(text.contains("Tab effort"), "hint mentions the sub-picker");
    }

    #[test]
    fn stats_overlay_shows_the_git_workspace_line() {
        let mut v = View::default();
        v.model.label = "gateway/kimi-k3".into();
        v.git = crate::git::GitContext {
            workspace: Some("YourAI-Harness".into()),
            worktree: Some("feat-x".into()),
            branch: Some("main".into()),
        };
        let m = Metadata {
            session: "s".into(),
            cwd: "/w/feat-x".into(),
            trusted_shell: false,
            yolo: false,
        };
        let mut t = Terminal::new(TestBackend::new(70, 26)).unwrap();
        t.draw(|f| {
            let mut canvas = Canvas::new(f.area());
            stats_overlay(&mut canvas, f.area(), &v, &m);
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
            text.contains("git YourAI-Harness · worktree feat-x · main"),
            "git line missing: {text}"
        );
        // Outside a repository the line stays quiet.
        v.git = crate::git::GitContext::default();
        t.draw(|f| {
            let mut canvas = Canvas::new(f.area());
            stats_overlay(&mut canvas, f.area(), &v, &m);
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
        assert!(!text.contains("git "), "unexpected git line: {text}");
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
                    id: SessionId::from("d3f40178deadbeef"),
                    title: "Fix parser off-by-one".into(),
                    model: "kimi-k3".into(),
                    updated_at: now - 2 * 3600,
                    is_current: true,
                },
                SessionRow {
                    id: SessionId::from("9a1b2c3ddeadd00d"),
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
