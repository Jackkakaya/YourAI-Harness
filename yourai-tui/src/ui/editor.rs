use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

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
}
