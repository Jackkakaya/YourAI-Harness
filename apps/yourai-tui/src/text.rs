//! Shared text plumbing: sanitizing untrusted output, display-width-aware
//! truncation and bounded pretty-printing. Pure functions with no UI or
//! state dependencies, so every layer can use them — the launcher (which
//! runs before the harness exists) through the renderer.
use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Hard cap on any single display text.
pub(crate) const MAX_TEXT: usize = 32_000;

/// Strip terminal controls, including ANSI CSI/OSC sequences, from untrusted output.
pub fn clean(text: &str) -> String {
    let mut result = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                result.push('\n');
            }
            '\t' => result.push_str("    "),
            '\n' => result.push(c),
            _ if !c.is_control() => result.push(c),
            _ => {}
        }
    }
    result
}

/// Sanitize and cap: over-long text is cut and visibly marked.
pub fn bounded(text: &str) -> String {
    let mut out = clean(text);
    if out.chars().count() > MAX_TEXT {
        out = out.chars().take(MAX_TEXT).collect();
        out.push_str("\n[Display shortened in TUI.]");
    }
    out
}

/// Append streaming text to a body without letting it grow past [`MAX_TEXT`].
pub fn append(body: &mut String, text: &str) {
    if body.chars().count() <= MAX_TEXT {
        *body = bounded(&format!("{body}{text}"));
    }
}

/// Append progress text, keeping only the most recent [`MAX_TEXT`] characters
/// (a sliding window: old output falls off the top).
pub fn append_progress(body: &mut String, text: &str) {
    body.push_str(&clean(text));
    let count = body.chars().count();
    if count > MAX_TEXT {
        *body = body.chars().skip(count - MAX_TEXT).collect();
    }
}

/// Pretty JSON, sanitized and bounded.
pub fn pretty(v: &Value) -> String {
    bounded(&serde_json::to_string_pretty(v).unwrap_or_default())
}

/// Head ellipsis at display width (grapheme-safe; CJK counts two columns).
pub fn elide(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text.width() <= width {
        return text.into();
    }
    let mut result = String::new();
    for g in text.graphemes(true) {
        if result.width() + g.width() + 1 > width {
            break;
        }
        result.push_str(g);
    }
    result.push('…');
    result
}

/// Tail ellipsis at display width — for paths and other head-significant text.
pub fn elide_tail(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.into();
    }
    if width == 0 {
        return String::new();
    }
    let mut tail = Vec::new();
    let mut used = 1;
    for g in text.graphemes(true).rev() {
        if used + g.width() > width {
            break;
        }
        used += g.width();
        tail.push(g);
    }
    format!("…{}", tail.into_iter().rev().collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_strips_ansi_and_normalizes_controls() {
        assert_eq!(clean("\x1b[31mred\x1b[0m\x1b]0;bad\x07"), "red");
        assert_eq!(clean("a\rb\r\nc\td"), "a\nb\nc    d");
    }

    #[test]
    fn bounded_marks_truncation() {
        let text = bounded(&"中".repeat(40000));
        assert!(text.contains("Display shortened"));
        assert!(text.chars().count() < 33000);
        let mut progress = "old".repeat(20_000);
        append_progress(&mut progress, "latest progress");
        assert!(progress.ends_with("latest progress"));
        assert_eq!(progress.chars().count(), MAX_TEXT);
    }

    #[test]
    fn elide_counts_display_width_not_chars() {
        // Wide glyphs occupy two columns: 4 CJK chars fill width 8 exactly.
        assert_eq!(elide("你好世界", 8), "你好世界");
        // Cutting to 5 columns keeps 2 glyphs + the ellipsis (2 + 1 + 1 = 4 used, next would overflow).
        let cut = elide("你好世界", 5);
        assert_eq!(cut, "你好…");
        // ASCII behaves by character count.
        assert_eq!(elide("abcdef", 4), "abc…");
        assert_eq!(elide("abc", 0), "");
        // The tail variant keeps the head-truncated path suffix; the
        // ellipsis column counts toward the budget (12 columns total).
        assert_eq!(elide_tail("/very/long/path/to/file.rs", 12), "…/to/file.rs");
    }
}
