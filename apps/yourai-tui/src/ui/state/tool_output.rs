//! Typed tool-output projections and bounded syntax/diff previews.
use super::*;
/// Count +/- lines in a unified diff; ---/+++ file headers excluded.
pub(super) fn diff_stats(diff: &str) -> (usize, usize) {
    let (mut adds, mut dels) = (0, 0);
    for line in diff.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            adds += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            dels += 1;
        }
    }
    (adds, dels)
}

/// Parse a unified diff into width-agnostic display rows. Each hunk's old
/// side (context+deletions) and new side (context+additions) are highlighted
/// as continuous text, so multiline scopes within a hunk resolve correctly.
/// `"-a,b"`/`"+c,d"` hunk offsets drive per-cell line numbers.
pub(super) fn build_diff_rows(diff: &str, path: &str) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    let mut lines = diff.lines().peekable();
    // Skip the ---/+++ file header block.
    while lines.peek().is_some_and(|l| !l.starts_with("@@")) {
        lines.next();
    }
    while let Some(header) = lines.next() {
        let Some((mut old_no, mut new_no)) = hunk_offsets(header) else {
            continue; // stray line before/between hunks (\ No newline…, etc.)
        };
        rows.push(DiffRow::hunk(header));
        let mut body: Vec<&str> = Vec::new();
        while let Some(l) = lines.peek() {
            if l.starts_with("@@") {
                break;
            }
            body.push(lines.next().unwrap());
        }
        let plain = |s: &str| vec![Span::styled(s.to_owned(), ratatui::style::Style::default())];
        let side = |keep: fn(char) -> bool| -> Vec<Vec<Span<'static>>> {
            let text = body
                .iter()
                .filter(|l| keep(l.chars().next().unwrap_or(' ')))
                .map(|l| l.get(1..).unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n");
            match crate::ui::syntax::highlight(&text, path) {
                Some(hl) => hl.into_iter().map(|l| l.spans).collect(),
                None => text.lines().map(plain).collect(),
            }
        };
        let old_hl = side(|c| c != '+');
        let new_hl = side(|c| c != '-');
        let (mut oi, mut ni) = (0usize, 0usize);
        let mut pend_old: Vec<(usize, Vec<Span<'static>>)> = Vec::new();
        let mut pend_new: Vec<(usize, Vec<Span<'static>>)> = Vec::new();
        let flush = |rows: &mut Vec<DiffRow>, pend_old: &mut Vec<_>, pend_new: &mut Vec<_>| {
            let n = pend_old.len().max(pend_new.len());
            for i in 0..n {
                rows.push(DiffRow::change(
                    pend_old.get(i).cloned(),
                    pend_new.get(i).cloned(),
                ));
            }
            pend_old.clear();
            pend_new.clear();
        };
        for line in body {
            let (kind, _) = line.split_at(1.min(line.len()));
            match kind {
                " " => {
                    flush(&mut rows, &mut pend_old, &mut pend_new);
                    let spans = old_hl.get(oi).cloned().unwrap_or_default();
                    rows.push(DiffRow::ctx(spans));
                    oi += 1;
                    ni += 1;
                    old_no += 1;
                    new_no += 1;
                }
                "-" => {
                    pend_old.push((old_no, old_hl.get(oi).cloned().unwrap_or_default()));
                    old_no += 1;
                    oi += 1;
                }
                "+" => {
                    pend_new.push((new_no, new_hl.get(ni).cloned().unwrap_or_default()));
                    new_no += 1;
                    ni += 1;
                }
                _ => {} // "\ No newline at end of file" and friends
            }
        }
        flush(&mut rows, &mut pend_old, &mut pend_new);
    }
    rows
}
/// "@@ -40,6 +41,7 @@" → (40, 41).
pub(super) fn hunk_offsets(header: &str) -> Option<(usize, usize)> {
    let inner = header.strip_prefix("@@")?.split("@@").next()?;
    let mut old = None;
    let mut new = None;
    for part in inner.split_whitespace() {
        if let Some(rest) = part.strip_prefix('-') {
            old = rest.split(',').next()?.parse().ok();
        } else if let Some(rest) = part.strip_prefix('+') {
            new = rest.split(',').next()?.parse().ok();
        }
    }
    Some((old?, new?))
}
/// read output lines look like "12|code"; split the numeric gutter.
pub(super) fn split_gutter(line: &str) -> (&str, &str) {
    let Some((num, rest)) = line.split_once('|') else {
        return ("", line);
    };
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return ("", line);
    }
    (num, rest)
}
/// Keep highlighting work bounded: write/read inputs are not size-capped
/// upstream the way card bodies are (MAX_TEXT).
pub(super) fn capped(code: &str) -> &str {
    let mut end = code.len().min(MAX_TEXT);
    while !code.is_char_boundary(end) {
        end -= 1;
    }
    &code[..end]
}
/// Highlight read output, preserving source line numbers as a gutter span.
pub(super) fn hl_read(content: &str, path: &str) -> Vec<Line<'static>> {
    use crate::ui::theme::FAINT;
    use ratatui::{style::Style, text::Span};
    let content = capped(content);
    let mut nums = Vec::new();
    let mut src = String::new();
    for line in content.lines() {
        let (num, code) = split_gutter(line);
        nums.push(num);
        src.push_str(code);
        src.push('\n');
    }
    let plain: Vec<Line<'static>> = src.lines().map(|l| Line::from(l.to_owned())).collect();
    let highlighted = crate::ui::syntax::highlight(&src, path).unwrap_or(plain);
    highlighted
        .into_iter()
        .zip(nums)
        .map(|(mut line, num)| {
            let mut spans = vec![Span::styled(
                format!("{num:>4} "),
                Style::default().fg(FAINT),
            )];
            spans.append(&mut line.spans);
            Line::from(spans)
        })
        .collect()
}
/// Highlight file content with a fixed gutter (write renders "+ " ghost-diff).
pub(super) fn hl_lines(code: &str, hint: &str, gutter: &'static str) -> Vec<Line<'static>> {
    use crate::ui::theme::GREEN;
    use ratatui::{style::Style, text::Span};
    let code = capped(code);
    let plain: Vec<Line<'static>> = code.lines().map(|l| Line::from(l.to_owned())).collect();
    let highlighted = crate::ui::syntax::highlight(code, hint).unwrap_or(plain);
    highlighted
        .into_iter()
        .map(|mut line| {
            let mut spans = vec![Span::styled(gutter.to_owned(), Style::default().fg(GREEN))];
            spans.append(&mut line.spans);
            Line::from(spans)
        })
        .collect()
}
/// websearch content → "title — host" rows. Exa has returned several
/// equivalent shapes over time (Title/URL blocks, markdown links, JSON result
/// arrays and bare URLs), so accept all of them and deduplicate by URL.
pub(super) fn search_brief(content: &str) -> Vec<String> {
    fn push(rows: &mut Vec<String>, seen: &mut HashSet<String>, title: Option<&str>, url: &str) {
        let url = url
            .trim()
            .trim_matches(|c: char| matches!(c, '<' | '>' | ')' | ']' | ','));
        if rows.len() >= 24 || !seen.insert(url.to_owned()) {
            return;
        }
        let host = url_host(url);
        let title = title.map(str::trim).filter(|s| !s.is_empty());
        let row = match (title, host) {
            (Some(t), Some(h)) => format!("{t} — {h}"),
            (Some(t), None) => t.to_owned(),
            (None, Some(h)) => format!("{h} · {url}"),
            (None, None) => url.to_owned(),
        };
        rows.push(bounded(&row));
    }
    fn json_results(value: &Value, rows: &mut Vec<String>, seen: &mut HashSet<String>) {
        match value {
            Value::Array(values) => {
                for value in values {
                    json_results(value, rows, seen);
                }
            }
            Value::Object(map) => {
                if let Some(url) = map
                    .get("url")
                    .or_else(|| map.get("link"))
                    .and_then(Value::as_str)
                {
                    let title = map
                        .get("title")
                        .or_else(|| map.get("name"))
                        .and_then(Value::as_str);
                    push(rows, seen, title, url);
                }
                for key in ["results", "items", "data", "content"] {
                    if let Some(child) = map.get(key) {
                        json_results(child, rows, seen);
                    }
                }
            }
            Value::String(text) => parse_text(text, rows, seen),
            _ => {}
        }
    }
    fn markdown_link(line: &str) -> Option<(&str, &str)> {
        let open = line.find('[')?;
        let middle = line[open + 1..].find("](")? + open + 1;
        let close = line[middle + 2..].find(')')? + middle + 2;
        Some((&line[open + 1..middle], &line[middle + 2..close]))
    }
    fn bare_url(line: &str) -> Option<&str> {
        let start = line.find("https://").or_else(|| line.find("http://"))?;
        line[start..].split_whitespace().next()
    }
    fn parse_text(text: &str, rows: &mut Vec<String>, seen: &mut HashSet<String>) {
        let mut title: Option<String> = None;
        for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let lower = line.to_ascii_lowercase();
            if let Some((_, rest)) = line.split_once(':') {
                if lower.starts_with("title:") || lower.starts_with("name:") {
                    title = Some(rest.trim().to_owned());
                    continue;
                }
                if lower.starts_with("url:") || lower.starts_with("link:") {
                    push(rows, seen, title.take().as_deref(), rest);
                    continue;
                }
            }
            if let Some((label, url)) = markdown_link(line) {
                push(rows, seen, Some(label), url);
                title = None;
            } else if let Some(url) = bare_url(line) {
                push(rows, seen, title.take().as_deref(), url);
            } else if line.starts_with('#') {
                title = Some(line.trim_start_matches('#').trim().to_owned());
            }
        }
    }

    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    if let Ok(value) = serde_json::from_str::<Value>(content) {
        json_results(&value, &mut rows, &mut seen);
    } else {
        parse_text(content, &mut rows, &mut seen);
    }
    rows
}

pub(super) fn url_host(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()?
        .host_str()
        .filter(|host| !host.is_empty())
        .map(str::to_owned)
}

/// A quiet two-line page teaser for webfetch previews. Markdown headings and
/// common HTML tags are stripped enough to avoid showing markup as the summary.
pub(super) fn page_brief(content: &str, format: &str) -> Vec<String> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let cleaned = if format == "markdown" {
                line.trim_start_matches('#').trim()
            } else if format == "html" {
                line.trim_matches(|c| c == '<' || c == '>').trim()
            } else {
                line
            };
            (!cleaned.is_empty()).then(|| bounded(cleaned))
        })
        .take(3)
        .collect()
}

/// Flatten structured tool errors into stable human-readable diagnostics.
pub(super) fn format_error(error: &Value, envelope: &Value) -> String {
    fn scalar(value: &Value) -> Option<String> {
        match value {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }
    fn fields(value: &Value, lines: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for key in ["message", "reason", "detail", "details", "code", "status"] {
                    if let Some(text) = map.get(key).and_then(scalar) {
                        let label = if key == "message" { "Error" } else { key };
                        lines.push(format!("{}: {text}", capitalize(label)));
                    }
                }
                if lines.is_empty() {
                    for (key, value) in map.iter().take(8) {
                        if let Some(text) = scalar(value) {
                            lines.push(format!("{}: {text}", capitalize(key)));
                        }
                    }
                }
            }
            Value::Array(values) => {
                for value in values.iter().take(8) {
                    if let Some(text) = scalar(value) {
                        lines.push(format!("Error: {text}"));
                    } else {
                        fields(value, lines);
                    }
                }
            }
            _ => {
                if let Some(text) = scalar(value) {
                    lines.push(format!("Error: {text}"));
                }
            }
        }
    }
    fn capitalize(text: &str) -> String {
        let mut chars = text.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => String::new(),
        }
    }

    let mut lines = Vec::new();
    fields(error, &mut lines);
    if lines.is_empty() {
        lines.push("Tool failed".into());
    }
    if let Some(tool) = envelope.get("tool").and_then(Value::as_str) {
        lines.push(format!("Tool: {tool}"));
    }
    bounded(&lines.join("\n"))
}

/// Lossless structured fallback (within the display cap); expansion keeps values.
pub(super) fn summarize_output(v: &Value) -> String {
    match v {
        Value::String(s) => bounded(s),
        _ => pretty(v),
    }
}
