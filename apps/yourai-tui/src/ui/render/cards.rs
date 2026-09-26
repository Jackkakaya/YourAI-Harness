//! Tool cards, diff layout and bounded text wrapping.
use super::spinner_frame;
use crate::text::elide;
use crate::ui::markdown::wrap_text;
use crate::ui::state::{DiffRow, ToolStatus, ToolView};
use crate::ui::theme::{
    mix, Theme, ACCENT, BG, DIFF_ADD_BG, DIFF_DEL_BG, FAINT, GREEN, MUTED, RED, TEXT, USER_SURFACE,
};
use ratatui::prelude::*;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Substantial artifacts get a surface; small read/search events remain inline.
pub(super) fn block_tool(t: &ToolView, expanded: bool) -> bool {
    expanded
        || matches!(t.name.as_str(), "shell" | "edit" | "write")
        || t.status == ToolStatus::Failed
        || t.exit_code.is_some_and(|code| code != 0)
}

/// Fill the complete row while preserving explicit diff/syntax backgrounds.
pub(super) fn surface_row(
    mut row: Line<'static>,
    width: usize,
    background: Color,
) -> Line<'static> {
    let background = row.style.bg.unwrap_or(background);
    row.spans
        .push(Span::raw(" ".repeat(width.saturating_sub(row.width()))));
    row.style = row.style.bg(background);
    row
}

/// Card title: "{▸ cursor} {status glyph} {subject}{pad}{meta}". The glyph
/// carries the status color; the subject is bright and is the only elidable
/// span; the right-hand meta always survives intact.
pub(super) fn tool_title(t: &ToolView, selected: bool, width: usize, tick: u64) -> Line<'static> {
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
        Span::styled(
            subject,
            Style::default()
                .fg(if t.status == ToolStatus::Running || selected {
                    TEXT
                } else {
                    MUTED
                })
                .add_modifier(
                    if selected || t.status == ToolStatus::Running || t.status == ToolStatus::Failed
                    {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    },
                ),
        ),
        Span::raw(" ".repeat(pad)),
        Span::styled(
            meta,
            if t.status == ToolStatus::Failed || t.exit_code.is_some_and(|code| code != 0) {
                Style::default().fg(RED).bold()
            } else {
                Style::default().fg(MUTED)
            },
        ),
    ])
}

/// Title row of a tool card, given a surface when the card renders as a
/// block. The single implementation shared by the layout cache (static
/// frames) and the per-frame animation bypass in `Renderer::draw` — the two
/// must never drift apart.
pub(super) fn tool_title_row(
    t: &ToolView,
    selected: bool,
    expanded: bool,
    width: usize,
    tick: u64,
) -> Line<'static> {
    let title = tool_title(t, selected, width, tick);
    if block_tool(t, expanded) {
        surface_row(title, width, USER_SURFACE)
    } else {
        title
    }
}

/// Right-hand title metadata, from structured state only (never parsed text).
pub(super) fn tool_meta(t: &ToolView) -> String {
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

/// Expand hint appears only when content is hidden.
pub(super) fn more_hint(lines: &mut Vec<Line<'static>>, hidden: usize, width: usize) {
    if hidden > 0 {
        lines.extend(wrap_text(
            &format!("⋯ {hidden} more · Ctrl-O"),
            Style::default().fg(FAINT),
            width,
            "  ",
        ));
    }
}

/// One diff row: + green / − red / context muted, tinted full-width bg;
/// file/hunk headers stay quiet.
pub(super) fn diff_line(line: &str, width: usize, prefix: &str) -> Vec<Line<'static>> {
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
pub(super) fn diff_render(
    rows: &[DiffRow],
    width: usize,
    theme: Theme,
    max_rows: usize,
    show_hunks: bool,
) -> (Vec<Line<'static>>, usize) {
    fn hunk_line(h: &str, width: usize) -> Vec<Line<'static>> {
        wrap_text(h, Style::default().fg(FAINT), width, "  ")
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
pub(super) fn diff_cell(
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
pub(super) fn unified_change_line(
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
pub(super) fn clipped_plain_line(
    ctx: &[Span<'static>],
    width: usize,
    prefix: &'static str,
) -> Line<'static> {
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
pub(super) fn subtle_span(theme: Theme, span: &Span<'static>, bg: Color) -> Span<'static> {
    let fg = mix(
        theme.color(span.style.fg.unwrap_or(TEXT)),
        theme.color(TEXT),
        0.45,
    );
    Span::styled(span.content.clone(), Style::default().fg(fg).bg(bg))
}

pub(super) fn spans_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

/// Clip spans to ≤`budget` columns (grapheme-safe, adjacent same-style
/// graphemes merged); on truncation the tail carries `ellipsis_style` "…".
pub(super) fn clip_spans(
    spans: &[Span<'static>],
    budget: usize,
    ellipsis_style: Style,
) -> Vec<Span<'static>> {
    let (kept, cut) = truncate_spans(spans, budget);
    if !cut {
        return kept;
    }
    let (mut kept, _) = truncate_spans(spans, budget.saturating_sub(1));
    kept.push(Span::styled("…", ellipsis_style));
    kept
}

/// Grapheme-safe span truncation; second return = something was dropped.
pub(super) fn truncate_spans(spans: &[Span<'static>], budget: usize) -> (Vec<Span<'static>>, bool) {
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
pub(super) fn tool_expanded(
    t: &ToolView,
    lines: &mut Vec<Line<'static>>,
    width: usize,
    theme: Theme,
) {
    let prefix = "    ";
    // edit/write/webfetch already have fully structured bodies; repeating their
    // often-large JSON arguments before the useful content only adds noise.
    if !matches!(t.name.as_str(), "edit" | "write" | "webfetch") {
        for line in t.input.lines() {
            lines.extend(wrap_text(line, Style::default().fg(MUTED), width, prefix));
        }
    }
    if t.status == ToolStatus::Running {
        for line in t.progress.lines() {
            lines.extend(wrap_text(
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
                lines.extend(crate::ui::markdown::wrap_spans(
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
                    lines.extend(crate::ui::markdown::wrap_spans(
                        line.spans.clone(),
                        width,
                        prefix,
                    ));
                }
            } else {
                push_wrapped(
                    lines,
                    t.output.lines(),
                    Style::default().fg(TEXT),
                    width,
                    prefix,
                );
            }
        }
        "shell" => {
            push_wrapped(
                lines,
                t.output.lines(),
                Style::default().fg(TEXT),
                width,
                prefix,
            );
            if !t.stderr.trim().is_empty() {
                lines.extend(wrap_text(
                    "── stderr ──",
                    Style::default().fg(FAINT),
                    width,
                    prefix,
                ));
                push_wrapped(
                    lines,
                    t.stderr.lines(),
                    Style::default().fg(MUTED),
                    width,
                    prefix,
                );
            }
        }
        _ => {
            push_wrapped(
                lines,
                t.output.lines(),
                Style::default().fg(MUTED),
                width,
                prefix,
            );
        }
    }
}

/// Type-aware card preview with per-tool line quotas: read renders nothing,
/// shell keeps one tail line, edit/write keep the first change rows,
/// websearch lists sources. Everything else is at most one summary line.
pub(super) fn tool_preview(
    t: &ToolView,
    lines: &mut Vec<Line<'static>>,
    width: usize,
    theme: Theme,
    edit_preview_rows: usize,
) {
    let prefix = "    ";
    match t.name.as_str() {
        "shell" if t.status == ToolStatus::Running => {
            // Last 3 progress lines, following scroll.
            let prog: Vec<&str> = t.progress.lines().collect();
            if prog.is_empty() {
                lines.extend(wrap_text(
                    "waiting for output…",
                    Style::default().fg(MUTED),
                    width,
                    prefix,
                ));
            } else {
                push_wrapped(
                    lines,
                    &prog[prog.len().saturating_sub(3)..],
                    Style::default().fg(MUTED),
                    width,
                    prefix,
                );
            }
        }
        "shell" => {
            let failed = t.status == ToolStatus::Failed || t.exit_code.is_some_and(|c| c != 0);
            // Diagnostics commonly go to stderr while stdout contains build progress.
            let use_stderr = !t.stderr.trim().is_empty() && (failed || t.output.trim().is_empty());
            let out: Vec<&str> = if use_stderr {
                t.stderr.lines().collect()
            } else {
                t.output.lines().collect()
            };
            if failed {
                let start = out.len().saturating_sub(5);
                let source = if use_stderr { "stderr" } else { "stdout" };
                lines.extend(wrap_text(
                    &if start > 0 {
                        format!("{source} · last 5 lines · Ctrl-O to expand")
                    } else {
                        source.to_owned()
                    },
                    Style::default().fg(MUTED),
                    width,
                    prefix,
                ));
                push_wrapped(
                    lines,
                    &out[start..],
                    Style::default().fg(RED),
                    width,
                    prefix,
                );
                if out.is_empty() {
                    lines.extend(wrap_text(
                        "No diagnostic output",
                        Style::default().fg(RED),
                        width,
                        prefix,
                    ));
                }
            } else {
                let start = out.len().saturating_sub(1);
                push_wrapped(
                    lines,
                    &out[start..],
                    Style::default().fg(MUTED),
                    width,
                    prefix,
                );
                if out.is_empty() {
                    lines.extend(wrap_text(
                        "no output",
                        Style::default().fg(MUTED),
                        width,
                        prefix,
                    ));
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
                lines.extend(wrap_text(
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
                    lines.extend(wrap_text(
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
                    lines.extend(crate::ui::markdown::wrap_spans(
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
            push_wrapped(
                lines,
                t.brief.iter().take(3),
                Style::default().fg(MUTED),
                width,
                "  ⏤ ",
            );
            more_hint(lines, t.brief.len().saturating_sub(3), width);
        }
        "webfetch" if !t.brief.is_empty() => {
            push_wrapped(
                lines,
                t.brief.iter().take(3),
                Style::default().fg(MUTED),
                width,
                "  ",
            );
            more_hint(lines, t.brief.len().saturating_sub(3), width);
        }
        _ => {
            if let Some(line) = t
                .brief
                .first()
                .map(String::as_str)
                .or_else(|| t.output.lines().find(|l| !l.trim().is_empty()))
            {
                lines.extend(wrap_text(line, Style::default().fg(MUTED), width, prefix));
            }
        }
    }
}

/// Wrap each row of `rows` and append to `lines`.
fn push_wrapped<S: AsRef<str>>(
    lines: &mut Vec<Line<'static>>,
    rows: impl IntoIterator<Item = S>,
    style: Style,
    width: usize,
    prefix: &str,
) {
    for row in rows {
        lines.extend(wrap_text(row.as_ref(), style, width, prefix));
    }
}

/// Like wrap_text() but applies bg color to the full line width (for diff backgrounds).
pub(super) fn wrap_bg(text: &str, style: Style, width: usize, prefix: &str) -> Vec<Line<'static>> {
    wrap_text(text, style, width, prefix)
        .into_iter()
        .map(|mut line| {
            line.spans.push(Span::styled(
                " ".repeat(width.saturating_sub(line.width())),
                style,
            ));
            line
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::timeline::TimelineCache;
    #[allow(clippy::wildcard_imports)]
    use super::*;
    use crate::ui::state::{Item, View};
    use serde_json::json;
    use yourai_core::prelude::Out;

    fn timeline(v: &View, width: usize) -> (Vec<Line<'static>>, Vec<(usize, u64)>) {
        let (lines, headers) = TimelineCache::default().layout(v, width, 3, 0);
        (lines.viewport(0..lines.len()), headers)
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
        assert!(text.contains("stderr") && text.contains("Ctrl-O to expand"));
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
        assert_eq!(row.width(), 140, "surface pads the complete block width");
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
        assert!(text.contains("⋯ 1 more · Ctrl-O"), "more hint: {text}");
    }
}
