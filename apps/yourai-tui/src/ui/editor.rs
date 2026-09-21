use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, PartialEq, Eq)]
enum WordClass {
    Whitespace,
    Word,
    Punct,
}

/// Classify a grapheme for readline-style word boundaries: whitespace, word
/// (alphanumeric + underscore), or punctuation. Multi-char graphemes are
/// classified by their first scalar.
fn classify(g: &str) -> WordClass {
    match g.chars().next() {
        Some(c) if c.is_whitespace() => WordClass::Whitespace,
        Some(c) if c.is_alphanumeric() || c == '_' => WordClass::Word,
        _ => WordClass::Punct,
    }
}

#[derive(Default)]
pub struct Editor {
    pub text: String,
    pub cursor: usize,
    history: Vec<String>,
    position: Option<usize>,
    draft: String,
}
impl Editor {
    pub fn insert(&mut self, text: &str) {
        let text = super::state::clean(text);
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
        // Insertion may merge graphemes (combining marks / ZWJ sequences).
        while self.cursor < self.text.len()
            && !self
                .text
                .grapheme_indices(true)
                .any(|(i, _)| i == self.cursor)
        {
            self.cursor += self.text[self.cursor..].chars().next().unwrap().len_utf8();
        }
    }
    pub fn left(&mut self) {
        self.cursor = self
            .text
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .take_while(|i| *i < self.cursor)
            .last()
            .unwrap_or(0);
    }
    pub fn right(&mut self) {
        self.cursor = self
            .text
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .find(|i| *i > self.cursor)
            .unwrap_or(self.text.len());
    }
    pub fn backspace(&mut self) {
        let end = self.cursor;
        self.left();
        self.text.replace_range(self.cursor..end, "");
    }
    pub fn delete(&mut self) {
        let start = self.cursor;
        self.right();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }
    pub fn home(&mut self) {
        self.cursor = self.text[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
    }
    pub fn end(&mut self) {
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map(|i| self.cursor + i)
            .unwrap_or(self.text.len());
    }
    /// Move the cursor left one readline word (Alt-B / Ctrl-Left).
    /// Skips leading whitespace, then one run of a single non-whitespace class.
    pub fn word_left(&mut self) {
        let graphemes: Vec<(usize, &str)> = self.text.grapheme_indices(true).collect();
        let mut idx = graphemes
            .iter()
            .position(|(i, _)| *i == self.cursor)
            .unwrap_or(graphemes.len());
        while idx > 0 && classify(graphemes[idx - 1].1) == WordClass::Whitespace {
            idx -= 1;
        }
        if idx > 0 {
            let c = classify(graphemes[idx - 1].1);
            while idx > 0 && classify(graphemes[idx - 1].1) == c {
                idx -= 1;
            }
        }
        self.cursor = graphemes
            .get(idx)
            .map(|(i, _)| *i)
            .unwrap_or(self.text.len());
    }
    /// Move the cursor right one readline word (Alt-F / Ctrl-Right).
    /// Skips leading whitespace, then one run of a single non-whitespace class.
    pub fn word_right(&mut self) {
        let graphemes: Vec<(usize, &str)> = self.text.grapheme_indices(true).collect();
        let mut idx = graphemes
            .iter()
            .position(|(i, _)| *i == self.cursor)
            .unwrap_or(graphemes.len());
        while idx < graphemes.len() && classify(graphemes[idx].1) == WordClass::Whitespace {
            idx += 1;
        }
        if idx < graphemes.len() {
            let c = classify(graphemes[idx].1);
            while idx < graphemes.len() && classify(graphemes[idx].1) == c {
                idx += 1;
            }
        }
        self.cursor = graphemes
            .get(idx)
            .map(|(i, _)| *i)
            .unwrap_or(self.text.len());
    }
    /// Delete backward one word using readline word boundaries
    /// (Alt-Backspace / Ctrl-Backspace).
    pub fn delete_word_back(&mut self) {
        let end = self.cursor;
        self.word_left();
        self.text.replace_range(self.cursor..end, "");
    }
    /// Delete forward one word using readline word boundaries (Alt-D /
    /// Ctrl-Delete).
    pub fn delete_word_fwd(&mut self) {
        let start = self.cursor;
        self.word_right();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }
    /// Bash-style `unix-word-rubout` (Ctrl-W): delete backward using
    /// whitespace as the only word boundary.
    pub fn unix_word_rubout(&mut self) {
        let graphemes: Vec<(usize, &str)> = self.text.grapheme_indices(true).collect();
        let mut idx = graphemes
            .iter()
            .position(|(i, _)| *i == self.cursor)
            .unwrap_or(graphemes.len());
        while idx > 0 && classify(graphemes[idx - 1].1) == WordClass::Whitespace {
            idx -= 1;
        }
        while idx > 0 && classify(graphemes[idx - 1].1) != WordClass::Whitespace {
            idx -= 1;
        }
        let new_cursor = graphemes
            .get(idx)
            .map(|(i, _)| *i)
            .unwrap_or(self.text.len());
        self.text.replace_range(new_cursor..self.cursor, "");
        self.cursor = new_cursor;
    }
    /// Delete from cursor to start of line (Ctrl-U / `unix-line-discard`).
    pub fn kill_to_start(&mut self) {
        let end = self.cursor;
        self.home();
        self.text.replace_range(self.cursor..end, "");
    }
    /// Delete from cursor to end of line (Ctrl-K / `kill-line`).
    pub fn kill_to_end(&mut self) {
        let start = self.cursor;
        self.end();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }
    /// True iff the cursor sits on the first logical line of the buffer. Used
    /// to decide whether Up navigates history or moves the cursor up a row.
    pub fn on_first_line(&self) -> bool {
        !self.text[..self.cursor].contains('\n')
    }
    /// True iff the cursor sits on the last logical line of the buffer. Used
    /// to decide whether Down navigates history or moves the cursor down a row.
    pub fn on_last_line(&self) -> bool {
        !self.text[self.cursor..].contains('\n')
    }
    pub fn vertical(&mut self, delta: isize) {
        let start = self.text[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let col = self.text[start..self.cursor].width();
        let target = if delta < 0 {
            if start == 0 {
                return;
            }
            self.text[..start - 1]
                .rfind('\n')
                .map(|i| i + 1)
                .unwrap_or(0)
        } else {
            let Some(end) = self.text[self.cursor..].find('\n') else {
                return;
            };
            self.cursor + end + 1
        };
        self.cursor = target;
        let mut width = 0;
        for g in self.text[target..].graphemes(true) {
            if g == "\n" || width + g.width() > col {
                break;
            }
            width += g.width();
            self.cursor += g.len();
        }
    }
    pub fn set(&mut self, text: String) {
        self.text = text;
        self.cursor = self.text.len();
    }
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        self.position = None;
        std::mem::take(&mut self.text)
    }
    pub fn remember(&mut self, text: &str) {
        if !text.is_empty() && self.history.last().is_none_or(|s| s != text) {
            self.history.push(text.into());
        }
        if self.history.len() > 100 {
            self.history.remove(0);
        }
        self.position = None;
    }
    pub fn history(&mut self, previous: bool) {
        if self.history.is_empty() {
            return;
        }
        if previous {
            let n = match self.position {
                Some(n) => n.saturating_sub(1),
                None => {
                    self.draft = self.text.clone();
                    self.history.len() - 1
                }
            };
            self.position = Some(n);
            self.set(self.history[n].clone());
        } else if let Some(n) = self.position {
            if n + 1 < self.history.len() {
                self.position = Some(n + 1);
                self.set(self.history[n + 1].clone());
            } else {
                self.position = None;
                self.set(self.draft.clone());
            }
        }
    }
    /// Wrapped editor lines and the cursor cell, using the same grapheme layout.
    pub fn layout(&self, width: usize) -> (Vec<String>, usize, usize) {
        let width = width.max(2);
        let (mut lines, mut col) = (vec![String::new()], 0);
        let mut cursor = (0, 0);
        for (offset, g) in self.text.grapheme_indices(true) {
            if g != "\n" && col + g.width() > width {
                lines.push(String::new());
                col = 0;
            }
            if offset == self.cursor {
                cursor = (lines.len() - 1, col);
            }
            if g == "\n" {
                lines.push(String::new());
                col = 0;
            } else {
                lines.last_mut().unwrap().push_str(g);
                col += g.width();
            }
        }
        if self.cursor == self.text.len() {
            if col >= width {
                lines.push(String::new());
                col = 0;
            }
            cursor = (lines.len() - 1, col);
        }
        (lines, cursor.0, cursor.1)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_editing_and_multiline_paste() {
        let mut e = Editor::default();
        e.insert("你👩‍💻e\u{301}\nnext");
        e.home();
        e.backspace();
        assert_eq!(e.text, "你👩‍💻e\u{301}next");
        e.left();
        e.backspace();
        assert_eq!(e.text, "你e\u{301}next");
        e.home();
        e.right();
        e.delete();
        assert_eq!(e.text, "你next");
    }
    #[test]
    fn history_restores_draft() {
        let mut e = Editor::default();
        e.remember("old");
        e.insert("draft");
        e.history(true);
        assert_eq!(e.text, "old");
        e.history(false);
        assert_eq!(e.text, "draft");
    }
    #[test]
    fn wrapping_cursor_and_vertical_movement() {
        let mut e = Editor::default();
        e.insert("中文\nabc");
        let (_, row, col) = e.layout(4);
        assert_eq!((row, col), (1, 3));
        e.vertical(-1);
        assert_eq!(e.cursor, "中".len());
        e.end();
        assert_eq!(e.cursor, "中文".len());
    }
    #[test]
    fn word_movement_respects_word_classes() {
        let mut e = Editor::default();
        e.insert("foo, bar");
        e.home(); // insert leaves the cursor at the end; rewind to test fwd-word.
                  // forward-word from start: end of "foo".
        e.word_right();
        assert_eq!(e.cursor, "foo".len());
        // forward-word: end of ",".
        e.word_right();
        assert_eq!(e.cursor, "foo,".len());
        // forward-word: skip space, end of "bar".
        e.word_right();
        assert_eq!(e.cursor, "foo, bar".len());
        // backward-word: start of "bar".
        e.word_left();
        assert_eq!(e.cursor, "foo, ".len());
        // backward-word: start of ",".
        e.word_left();
        assert_eq!(e.cursor, "foo".len());
        // backward-word: start of "foo".
        e.word_left();
        assert_eq!(e.cursor, 0);
    }
    #[test]
    fn word_movement_skips_leading_whitespace() {
        let mut e = Editor::default();
        e.insert("  foo");
        e.word_right();
        assert_eq!(e.cursor, "  foo".len());
        e.word_left();
        assert_eq!(e.cursor, "  ".len());
    }
    #[test]
    fn delete_word_back_vs_unix_word_rubout() {
        // Alt+Backspace (readline backward-kill-word) stops at word boundaries.
        let mut e = Editor::default();
        e.insert("foo.bar");
        e.delete_word_back();
        assert_eq!(e.text, "foo.");
        assert_eq!(e.cursor, "foo.".len());
        // Ctrl+W (unix-word-rubout) deletes the whole whitespace-delimited chunk.
        let mut e = Editor::default();
        e.insert("foo.bar");
        e.unix_word_rubout();
        assert_eq!(e.text, "");
        // Ctrl+W stops at whitespace.
        let mut e = Editor::default();
        e.insert("foo, bar");
        e.unix_word_rubout();
        assert_eq!(e.text, "foo, ");
        e.unix_word_rubout();
        assert_eq!(e.text, "");
    }
    #[test]
    fn delete_word_fwd_kills_forward_word() {
        let mut e = Editor::default();
        e.insert("foo, bar");
        e.home(); // start of line so there is a word ahead to kill.
        e.delete_word_fwd();
        assert_eq!(e.text, ", bar");
        assert_eq!(e.cursor, 0);
    }
    #[test]
    fn kill_to_start_and_end_of_line() {
        let mut e = Editor::default();
        e.insert("hello\nworld");
        // cursor at end; Ctrl+U clears only the current line.
        e.kill_to_start();
        assert_eq!(e.text, "hello\n");
        assert_eq!(e.cursor, "hello\n".len());
        // Move to start of "hello" line, Ctrl+K clears to end of line.
        e.home();
        e.vertical(-1);
        e.kill_to_end();
        assert_eq!(e.text, "\n");
        assert_eq!(e.cursor, 0);
    }
    #[test]
    fn on_first_last_line_reflects_cursor_row() {
        let mut e = Editor::default();
        e.insert("aaa\nbbb\nccc");
        assert!(!e.on_first_line());
        assert!(e.on_last_line());
        e.home();
        e.vertical(-1);
        e.vertical(-1);
        assert!(e.on_first_line());
        assert!(!e.on_last_line());
    }
    #[test]
    fn up_down_history_only_on_boundary_lines() {
        let mut e = Editor::default();
        e.remember("older");
        // Single line: Up navigates history.
        e.insert("draft");
        assert!(e.on_first_line() && e.on_last_line());
        // emulate `edit` Up handler
        if e.on_first_line() {
            e.history(true);
        } else {
            e.vertical(-1);
        }
        assert_eq!(e.text, "older");
        if e.on_last_line() {
            e.history(false);
        } else {
            e.vertical(1);
        }
        assert_eq!(e.text, "draft");
        // Multiline: Up on second line moves cursor, not history.
        let mut e = Editor::default();
        e.remember("older");
        e.insert("line1\nline2");
        assert_eq!(e.cursor, "line1\nline2".len());
        // cursor on last line; Up should move to first line.
        if e.on_first_line() {
            e.history(true);
        } else {
            e.vertical(-1);
        }
        assert_eq!(e.cursor, "line1".len());
        assert_eq!(e.text, "line1\nline2");
        // Now on first line; Up navigates history.
        if e.on_first_line() {
            e.history(true);
        } else {
            e.vertical(-1);
        }
        assert_eq!(e.text, "older");
    }
}
