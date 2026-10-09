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
    busy: Option<(&str, Color)>,
) -> Vec<Line<'static>> {
    let muted = Style::default().fg(MUTED);
    let permission = permission_label(m.yolo, m.trusted_shell);
    let rate = v
        .session
        .model_metrics
        .requests
        .last_output_tokens_per_second
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(compact_number);
    // Unknown metrics are omitted, not shown as "—" placeholders: the row
    // stays quiet until there is something real to say (OpenCode parity).
    // Fields carry an optional trailing span so the ctx percentage can ride
    // with its pressure bar ("ctx 42% ▓▓▓░░░░░░░"), colored by pressure.
    let mut fields: Vec<(usize, String, Option<Span<'static>>)> = Vec::new();
    if let Some(ratio) = ctx_pressure(v) {
        let pct = if ratio * 100.0 > 999.0 {
            ">999%".into()
        } else {
            format!("{:.0}%", ratio * 100.0)
        };
        // The bar costs 11 cells; narrower rows keep the bare percentage.
        let bar = (width >= 60).then(|| {
            Span::styled(
                format!(" {}", ctx_bar(ratio)),
                Style::default().fg(ctx_color(ratio)),
            )
        });
        fields.push((1, format!("ctx {pct}"), bar));
    }
    let label_reserve = if width >= 60 { 16 } else { 8 };
    let metrics_budget = width.saturating_sub(label_reserve + permission.width() + 6);
    let mut omitted = false;
    let mut optional: Vec<(usize, String, Option<Span<'static>>)> = Vec::new();
    if v.usage().total_tokens > 0 {
        optional.push((0, format!("tok {}", tokens(v.usage().total_tokens)), None));
    }
    if let Some(rate) = &rate {
        optional.push((2, format!("{rate} tok/s"), None));
    }
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
            None,
        ));
    }
    for field in optional {
        let used: usize = fields
            .iter()
            .map(|(_, text, extra)| {
                text.width() + extra.as_ref().map_or(0, |span| span.content.width()) + 3
            })
            .sum();
        if used + field.1.width() <= metrics_budget {
            fields.push(field);
        } else {
            omitted = true;
        }
    }
    fields.sort_by_key(|(order, _, _)| *order);
    let text_style = Style::default().fg(TEXT);
    let mut metrics_spans: Vec<Span<'static>> = Vec::new();
    for (i, (_, text, extra)) in fields.into_iter().enumerate() {
        if i > 0 {
            metrics_spans.push(Span::styled(" · ", text_style));
        }
        metrics_spans.push(Span::styled(text, text_style));
        if let Some(extra) = extra {
            metrics_spans.push(extra);
        }
    }
    let metrics_width: usize = metrics_spans.iter().map(|s| s.content.width()).sum();
    let overflow = if omitted { " …" } else { "" };
    let divider = if metrics_spans.is_empty() && overflow.is_empty() {
        ""
    } else {
        " · "
    };
    let right_width = metrics_width + overflow.width() + divider.width() + permission.width();
    let left_width = width.saturating_sub(right_width + 2);
    // Busy: the single status row carries spinner + activity on the left;
    // the title/path resume when idle. One row below the composer, total.
    let (left_spans, left_used) = match busy {
        Some((status, color)) => {
            let text = elide_tail(status, left_width);
            let used = text.width();
            (vec![Span::styled(text, Style::default().fg(color))], used)
        }
        None => {
            let title = v.session.title.as_deref().unwrap_or("New session");
            // The git branch rides at the end of the path segment, e.g.
            // "title · ~/repo · main"; the cwd itself already names a
            // linked worktree directory when one is in use.
            let git_label = v.git.footer_label().and_then(|branch| {
                let budget = left_width.saturating_sub(8);
                (budget > 3).then(|| format!(" · {}", elide_tail(branch, budget - 3)))
            });
            let git_width = git_label.as_deref().map_or(0, str::width);
            // A partial title competes with the path and conveys little: show it whole or omit it.
            let show_title = !(v.session.todos.panel_open() && width >= 100)
                && !omitted
                && title.width() + 3 + m.cwd.width() + git_width <= left_width;
            let title_label = if show_title {
                format!("{title} · ")
            } else {
                String::new()
            };
            let path = elide_tail(
                &m.cwd,
                left_width
                    .saturating_sub(title_label.width())
                    .saturating_sub(git_width),
            );
            let used = title_label.width() + path.width() + git_width;
            let mut spans = vec![
                Span::styled(title_label, Style::default().fg(TEXT).bold()),
                Span::styled(path, muted),
            ];
            if let Some(git_label) = git_label {
                spans.push(Span::styled(git_label, Style::default().fg(TEXT)));
            }
            (spans, used)
        }
    };
    let gap = width.saturating_sub(left_used + right_width);
    let mut spans = left_spans;
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(metrics_spans);
    spans.push(Span::styled(overflow, muted));
    spans.push(Span::styled(divider, Style::default().fg(BORDER)));
    spans.push(Span::styled(
        permission,
        if m.yolo {
            Style::default().fg(YELLOW).bold()
        } else {
            muted
        },
    ));
    vec![Line::from(spans)]
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

#[cfg(test)]
mod tests {
    use super::super::Metadata;
    #[allow(clippy::wildcard_imports)]
    use super::*;
    #[test]
    fn long_unicode_branches_preserve_context_and_permission_on_narrow_screens() {
        let mut v = View::default();
        v.session.context_usage = Some(yourai_harness::runtime::ContextUsage {
            estimated_tokens: 500,
            context_window: Some(1000),
            input_budget: Some(900),
            output_reserve: 100,
        });
        for branch in [
            "feature/long-branch-".repeat(10),
            "功能/非常长的分支名字".repeat(10),
        ] {
            v.git.branch = Some(branch);
            for width in [30, 35, 60, 120] {
                for (yolo, expected) in [(false, "ask"), (true, "YOLO")] {
                    let mut m = meta();
                    m.yolo = yolo;
                    let line = footer_lines(width, &v, &m, 0, None).remove(0);
                    assert!(line.width() <= width, "{} > {width}", line.width());
                    let mut buffer = Buffer::empty(Rect::new(0, 0, width as u16, 1));
                    buffer.set_line(0, 0, &line, width as u16);
                    let visible: String =
                        (0..width as u16).map(|x| buffer[(x, 0)].symbol()).collect();
                    assert!(visible.contains(expected), "{visible}");
                    assert!(visible.contains("ctx 50%"), "{visible}");
                }
            }
        }
    }

    fn meta() -> Metadata {
        Metadata {
            session: "s".into(),
            cwd: "/codejk/YourAI-Harness".into(),
            trusted_shell: false,
            yolo: false,
        }
    }
    fn rendered(width: usize, v: &View) -> String {
        footer_lines(width, v, &meta(), 0, None)
            .remove(0)
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect()
    }

    #[test]
    fn footer_appends_branch_and_drops_the_title_first() {
        let mut v = View::default();
        v.session.title = Some("A very long session title".into());
        assert!(!rendered(100, &v).contains("main"));
        v.git = crate::git::GitContext {
            workspace: Some("YourAI-Harness".into()),
            worktree: None,
            branch: Some("main".into()),
        };
        // Wide terminal: title · path · branch all fit.
        let wide = rendered(100, &v);
        assert!(wide.contains("A very long session title"), "{wide}");
        assert!(wide.contains("/codejk/YourAI-Harness"), "{wide}");
        assert!(wide.contains("· main"), "{wide}");
        // Narrow terminal: the title yields so the path and branch survive.
        let narrow = rendered(30, &v);
        assert!(!narrow.contains("A very long session title"), "{narrow}");
        assert!(narrow.contains("· main"), "{narrow}");
    }
}
