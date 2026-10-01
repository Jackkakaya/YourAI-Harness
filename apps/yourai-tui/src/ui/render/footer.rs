//! Footer line composition and its small formatting helpers.
use super::super::{
    state::View,
    theme::{BORDER, GREEN, MUTED, RED, TEXT, YELLOW},
    Metadata,
};
use crate::text::elide_tail;
use ratatui::prelude::*;
use unicode_width::UnicodeWidthStr;

pub(super) fn footer_lines(
    width: usize,
    v: &View,
    m: &Metadata,
    queued: usize,
) -> Vec<Line<'static>> {
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
        .session
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
    let title = v.session.title.as_deref().unwrap_or("New session");
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

fn ctx_pressure(v: &View) -> Option<f64> {
    let usage = v.session.context_usage.as_ref()?;
    let used = usage.estimated_tokens;
    usage
        .context_window
        .filter(|w| *w > 0)
        .map(|w| used as f64 / w as f64)
}
pub(super) fn ctx_color(ratio: f64) -> Color {
    if ratio >= 0.85 {
        RED
    } else if ratio >= 0.70 {
        YELLOW
    } else {
        GREEN
    }
}
pub(super) fn ctx_bar(ratio: f64) -> String {
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
pub(super) fn tokens(value: u64) -> String {
    if value < 1_000_000 {
        format!("{:.1}K", value as f64 / 1000.0)
    } else {
        compact_number(value as f64)
    }
}

pub(super) fn label(s: &str, color: Color) -> Line<'static> {
    Line::from(Span::styled(format!("  {s}"), Style::default().fg(color)))
}
