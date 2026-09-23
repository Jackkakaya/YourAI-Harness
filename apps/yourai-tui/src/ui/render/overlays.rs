//! Dashboard and pickers, constrained to the current terminal.
use super::*;
/// ^B dashboard overlay (replaces the old sidebar content).
pub(super) fn stats_overlay(f: &mut Frame<'_>, area: Rect, v: &View, m: &Metadata, queued: usize) {
    let _ = queued;
    let width = area.width.min(64);
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
        lines.push(label(
            &format!("Current model: ${pin}/{pout} per M in/out"),
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
        if m.yolo { YELLOW } else { TEXT },
    ));
    // Theme line.
    lines.push(label(&format!("theme {}", v.theme.label()), MUTED));
    let lines: Vec<_> = lines
        .into_iter()
        .flat_map(|line| {
            crate::ui::markdown::wrap_spans(line.spans, width.saturating_sub(2) as usize, "")
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

/// `/models` picker overlay.
pub(super) fn model_picker_overlay(f: &mut Frame<'_>, area: Rect, v: &View) {
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
pub(super) fn sessions_overlay(f: &mut Frame<'_>, area: Rect, v: &View) {
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
pub(super) fn theme_picker_overlay(f: &mut Frame<'_>, area: Rect, v: &View) {
    let all = crate::ui::theme::Theme::ALL;
    let selected = match v.overlay {
        Overlay::Themes(i) => i,
        _ => 0,
    };
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
