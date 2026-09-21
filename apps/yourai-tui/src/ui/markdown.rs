//! Markdown is presentation only: no HTML execution, links or task-state mutations.
use super::{
    state::clean,
    theme::{ACCENT, BLUE, GREEN, MUTED, PANEL, TEXT},
};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::prelude::*;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

type Styled = Vec<Span<'static>>;
#[derive(Default)]
struct Table {
    rows: Vec<Vec<Styled>>,
    row: Vec<Styled>,
    cell: Styled,
}
struct Writer {
    lines: Vec<Line<'static>>,
    spans: Styled,
    width: usize,
    styles: Vec<Style>,
    lists: Vec<Option<u64>>,
    quote: usize,
    code: bool,
    code_lang: String,
    code_src: String,
    table: Option<Table>,
    links: Vec<String>,
}
impl Writer {
    fn style(&self) -> Style {
        self.styles
            .last()
            .copied()
            .unwrap_or_else(|| Style::default().fg(TEXT))
    }
    fn text(&mut self, text: &str, style: Style) {
        let span = Span::styled(clean(text), style);
        if let Some(table) = &mut self.table {
            table.cell.push(span);
        } else {
            self.spans.push(span);
        }
    }
    fn flush(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let prefix = if self.code {
            "  │ ".to_owned()
        } else {
            format!(
                "  {}{}",
                "│ ".repeat(self.quote),
                "  ".repeat(self.lists.len().saturating_sub(1))
            )
        };
        self.lines.extend(wrap_spans(
            std::mem::take(&mut self.spans),
            self.width,
            &prefix,
        ));
    }
    fn blank(&mut self) {
        self.flush();
        if self.lines.last().is_some_and(|l| !l.spans.is_empty()) {
            self.lines.push(Line::default());
        }
    }
    fn table(&mut self, table: Table) {
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let width = self.width.saturating_sub(2);
        // On a narrow screen, stack cells rather than losing columns off-screen.
        if width < columns * 6 {
            for row in table.rows {
                for cell in row {
                    self.lines.extend(wrap_spans(cell, self.width, "  │ "));
                }
                self.lines.push(Line::default());
            }
            return;
        }
        let budget = self.width.saturating_sub(columns * 3 + 4);
        let mut sizes = vec![2; columns];
        for row in &table.rows {
            for (i, cell) in row.iter().enumerate() {
                sizes[i] = sizes[i].max(cell.iter().map(|s| s.content.width()).sum::<usize>());
            }
        }
        while sizes.iter().sum::<usize>() > budget {
            let (i, max) = sizes.iter().enumerate().max_by_key(|(_, n)| **n).unwrap();
            if *max <= 2 {
                break;
            }
            sizes[i] -= 1;
        }
        for (row_index, row) in table.rows.into_iter().enumerate() {
            let cells: Vec<_> = (0..columns)
                .map(|i| {
                    let mut spans = row.get(i).cloned().unwrap_or_default();
                    if row_index == 0 {
                        for span in &mut spans {
                            span.style = span.style.fg(ACCENT).bold();
                        }
                    }
                    wrap_spans(spans, sizes[i], "")
                })
                .collect();
            let height = cells.iter().map(Vec::len).max().unwrap_or(1);
            for y in 0..height {
                let mut spans = vec![Span::styled("  │ ", Style::default().fg(MUTED))];
                for (i, cell) in cells.iter().enumerate() {
                    let line = cell.get(y).cloned().unwrap_or_default();
                    let used = line.width();
                    spans.extend(line.spans);
                    spans.push(Span::styled(
                        format!("{} │ ", " ".repeat(sizes[i].saturating_sub(used))),
                        Style::default().fg(MUTED),
                    ));
                }
                self.lines.push(Line::from(spans));
            }
            if row_index == 0 {
                self.lines.push(Line::from(Span::styled(
                    format!(
                        "  ├{}┤",
                        sizes
                            .iter()
                            .map(|n| "─".repeat(n + 2))
                            .collect::<Vec<_>>()
                            .join("┼")
                    ),
                    Style::default().fg(MUTED),
                )));
            }
        }
    }
}
pub fn render(text: &str, width: usize) -> Vec<Line<'static>> {
    let mut w = Writer {
        lines: vec![],
        spans: vec![],
        width,
        styles: vec![],
        lists: vec![],
        quote: 0,
        code: false,
        code_lang: String::new(),
        code_src: String::new(),
        table: None,
        links: vec![],
    };
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { .. } => {
                    w.flush();
                    w.styles.push(w.style().fg(ACCENT).bold());
                }
                Tag::Strong => w.styles.push(w.style().bold()),
                Tag::Emphasis => w.styles.push(w.style().italic()),
                Tag::Strikethrough => w.styles.push(w.style().crossed_out()),
                Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                    w.links.push(clean(&dest_url));
                    w.styles.push(w.style().fg(BLUE).underlined());
                }
                Tag::BlockQuote(_) => {
                    w.flush();
                    w.quote += 1;
                }
                Tag::List(start) => {
                    w.flush();
                    w.lists.push(start);
                }
                Tag::Item => {
                    w.flush();
                    let marker = match w.lists.last_mut() {
                        Some(Some(n)) => {
                            let marker = format!("{n}. ");
                            *n += 1;
                            marker
                        }
                        _ => "• ".into(),
                    };
                    w.text(&marker, Style::default().fg(MUTED));
                }
                Tag::CodeBlock(kind) => {
                    w.flush();
                    w.code_lang = match kind {
                        CodeBlockKind::Fenced(lang) => clean(&lang),
                        _ => String::new(),
                    };
                    w.lines.push(Line::from(Span::styled(
                        format!("  ┌ {}", w.code_lang),
                        Style::default().fg(MUTED),
                    )));
                    w.code = true;
                }
                Tag::Table(_) => {
                    w.flush();
                    w.table = Some(Table::default());
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    if w.table.is_none() {
                        w.flush();
                        if w.lists.is_empty() {
                            w.blank();
                        }
                    }
                }
                TagEnd::Heading(_) => {
                    w.flush();
                    w.styles.pop();
                    w.blank();
                }
                TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough => {
                    w.styles.pop();
                }
                TagEnd::Link | TagEnd::Image => {
                    w.styles.pop();
                    if let Some(url) = w.links.pop() {
                        w.text(&format!(" ({url})"), Style::default().fg(MUTED));
                    }
                }
                TagEnd::BlockQuote(_) => {
                    w.flush();
                    w.quote = w.quote.saturating_sub(1);
                }
                TagEnd::Item => w.flush(),
                TagEnd::List(_) => {
                    w.flush();
                    w.lists.pop();
                }
                TagEnd::CodeBlock => {
                    w.code = false;
                    let src = std::mem::take(&mut w.code_src);
                    let lang = std::mem::take(&mut w.code_lang);
                    match super::syntax::highlight(&src, &lang) {
                        Some(highlighted) => {
                            for line in highlighted {
                                let spans = line
                                    .spans
                                    .into_iter()
                                    .map(|s| Span::styled(s.content, s.style.bg(PANEL)))
                                    .collect();
                                w.lines.extend(wrap_spans(spans, w.width, "  │ "));
                            }
                        }
                        // Unknown/unspecified language: keep the single-color look.
                        None => {
                            for line in src.lines() {
                                w.lines.extend(wrap_spans(
                                    vec![Span::styled(
                                        line.to_owned(),
                                        Style::default().fg(BLUE).bg(PANEL),
                                    )],
                                    w.width,
                                    "  │ ",
                                ));
                            }
                        }
                    }
                    w.lines
                        .push(Line::from(Span::styled("  └", Style::default().fg(MUTED))));
                }
                TagEnd::TableCell => {
                    if let Some(table) = &mut w.table {
                        table.row.push(std::mem::take(&mut table.cell));
                    }
                }
                TagEnd::TableHead | TagEnd::TableRow => {
                    if let Some(table) = &mut w.table {
                        table.rows.push(std::mem::take(&mut table.row));
                    }
                }
                TagEnd::Table => {
                    if let Some(table) = w.table.take() {
                        w.table(table);
                    }
                }
                _ => {}
            },
            Event::Text(text) if w.code => {
                // Buffered whole, then highlighted at TagEnd::CodeBlock.
                w.code_src.push_str(&text);
            }
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                w.text(&text, w.style())
            }
            Event::Code(text) => w.text(&text, w.style().fg(BLUE).bg(PANEL)),
            Event::SoftBreak => w.text(" ", w.style()),
            Event::HardBreak => {
                w.flush();
            }
            Event::Rule => {
                w.flush();
                w.lines.push(Line::from(Span::styled(
                    "─".repeat(width.saturating_sub(2)),
                    Style::default().fg(MUTED),
                )));
            }
            Event::TaskListMarker(checked) => {
                if w.spans.last().is_some_and(|s| s.content == "• ") {
                    w.spans.pop();
                }
                w.text(
                    if checked { "☑ " } else { "☐ " },
                    Style::default().fg(if checked { GREEN } else { MUTED }),
                );
            }
            _ => {}
        }
    }
    w.flush();
    while w.lines.last().is_some_and(|l| l.spans.is_empty()) {
        w.lines.pop();
    }
    w.lines
}
pub(crate) fn wrap_spans(spans: Styled, width: usize, prefix: &str) -> Vec<Line<'static>> {
    let available = width.saturating_sub(prefix.width()).max(1);
    let mut lines = vec![];
    let mut current = vec![Span::styled(prefix.to_owned(), Style::default().fg(MUTED))];
    let mut used = 0;
    for span in spans {
        for g in span.content.graphemes(true) {
            if used + g.width() > available && used > 0 {
                lines.push(Line::from(std::mem::take(&mut current)));
                current.push(Span::styled(prefix.to_owned(), Style::default().fg(MUTED)));
                used = 0;
            }
            if let Some(last) = current.last_mut().filter(|s| s.style == span.style) {
                last.content.to_mut().push_str(g);
            } else {
                current.push(Span::styled(g.to_owned(), span.style));
            }
            used += g.width();
        }
    }
    lines.push(Line::from(current));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    fn plain(lines: &[Line<'_>]) -> String {
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
    fn renders_markdown_styles_tasks_code_and_tables() {
        let lines = render("# Heading\n\n**bold** *italic* ~~removed~~ `inline`\n\n- [x] done\n- [ ] pending\n\n> quote\n\n```rust\nlet x = 1;\n```\n\n| Key | Value |\n| --- | --- |\n| 中文 | **yes** |\n\n[docs](https://example.com)", 60);
        let text = plain(&lines);
        for expected in [
            "Heading",
            "bold",
            "☑ done",
            "☐ pending",
            "let x = 1;",
            "中文",
            "https://example.com",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(!text.contains("**"));
        assert!(!text.contains("```"));
        assert!(lines
            .iter()
            .flat_map(|l| &l.spans)
            .any(|s| s.content.contains("bold") && s.style.add_modifier.contains(Modifier::BOLD)));
        assert!(lines.iter().all(|l| l.width() <= 60));
    }
    #[test]
    fn fenced_code_is_highlighted_and_unknown_fences_fall_back() {
        use super::super::theme::SY_STRING;
        let lines = render(
            "```rust\nlet s = \"hi\";\n```\n\n```wat-lang?\nx = 1\n```",
            60,
        );
        let spans: Vec<&Span<'_>> = lines.iter().flat_map(|l| l.spans.iter()).collect();
        assert!(
            spans.iter().any(|s| s.style.fg == Some(SY_STRING)),
            "rust fence picked up syntax colors"
        );
        assert!(
            spans
                .iter()
                .any(|s| s.content.contains("x = 1") && s.style.fg == Some(BLUE)),
            "unknown fence keeps the legacy single color"
        );
        assert!(lines.iter().all(|l| l.width() <= 60));
    }
    #[test]
    fn narrow_tables_unicode_and_unfinished_streams_do_not_overflow() {
        for width in [10, 20, 80] {
            let lines = render(
                "| 一 | 二 | 三 |\n|---|---|---|\n|🙂🙂|abcdefghi|text|\n\n```rust\nunfinished中文",
                width,
            );
            assert!(
                lines.iter().all(|l| l.width() <= width),
                "{width}: {}",
                plain(&lines)
            );
            assert!(plain(&lines).contains("unfinished") || width == 10);
        }
    }
}
