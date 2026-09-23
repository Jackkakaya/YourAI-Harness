//! Copy from a frozen viewport so incoming output cannot change the selected text.
use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Modifier},
};
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
pub struct Selection {
    pub screen: Option<Buffer>,
    snapshot: Option<Buffer>,
    region: Rect,
    anchor: Position,
    head: Position,
    dragged: bool,
}
impl Selection {
    pub fn clear(&mut self) {
        self.snapshot = None;
        self.dragged = false;
    }
    pub fn begin(&mut self, point: Position, region: Rect) {
        self.clear();
        if region.contains(point) {
            self.snapshot = self.screen.clone();
            self.region = region;
            self.anchor = point;
            self.head = point;
        }
    }
    pub fn drag(&mut self, point: Position) {
        if self.snapshot.is_none() {
            return;
        }
        self.head = Position::new(
            point
                .x
                .clamp(self.region.x, self.region.right().saturating_sub(1)),
            point
                .y
                .clamp(self.region.y, self.region.bottom().saturating_sub(1)),
        );
        self.dragged |= self.head != self.anchor;
    }
    pub fn active(&self) -> bool {
        self.snapshot.is_some() && self.dragged
    }
    fn range(&self) -> (Position, Position) {
        if (self.anchor.y, self.anchor.x) <= (self.head.y, self.head.x) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
    fn selected(&self, x: u16, y: u16, width: u16) -> bool {
        let (start, end) = self.range();
        y >= start.y
            && y <= end.y
            && (y != start.y || x.saturating_add(width) > start.x)
            && (y != end.y || x <= end.x)
    }
    pub fn text(&self) -> Option<String> {
        if !self.active() {
            return None;
        }
        let buffer = self.snapshot.as_ref()?;
        let (start, end) = self.range();
        let mut lines = Vec::new();
        for y in start.y..=end.y {
            let mut line = String::new();
            let mut x = self.region.x;
            while x < self.region.right() {
                let symbol = buffer[(x, y)].symbol();
                let width = symbol.width().max(1) as u16;
                if self.selected(x, y, width) {
                    line.push_str(symbol);
                }
                x += width;
            }
            lines.push(line.trim_end().to_owned());
        }
        Some(lines.join("\n"))
    }
    pub fn render(&mut self, buffer: &mut Buffer, foreground: Color, background: Color) {
        if self.screen.as_ref().is_some_and(|s| s.area != buffer.area) {
            self.clear();
        }
        match &mut self.screen {
            Some(screen) => screen.clone_from(buffer),
            None => self.screen = Some(buffer.clone()),
        }
        if !self.active() {
            return;
        }
        let snapshot = self.snapshot.as_ref().unwrap();
        for y in self.region.y..self.region.bottom() {
            let mut x = self.region.x;
            while x < self.region.right() {
                let width = snapshot[(x, y)].symbol().width().max(1) as u16;
                for cell_x in x..(x + width).min(self.region.right()) {
                    buffer[(cell_x, y)] = snapshot[(cell_x, y)].clone();
                    if self.selected(x, y, width) {
                        let cell = &mut buffer[(cell_x, y)];
                        cell.fg = foreground;
                        cell.bg = background;
                        cell.modifier.remove(Modifier::REVERSED);
                    }
                }
                x += width;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;
    #[test]
    fn reverse_unicode_selection_is_stable_while_streaming() {
        let area = Rect::new(0, 0, 12, 2);
        let mut buffer = Buffer::empty(area);
        buffer.set_string(0, 0, "你好 abc", Style::default());
        buffer.set_string(0, 1, "next", Style::default());
        let mut selection = Selection::default();
        selection.render(&mut buffer, Color::Black, Color::White);
        selection.begin(Position::new(3, 1), area);
        selection.drag(Position::new(1, 0));
        assert_eq!(selection.text().as_deref(), Some("你好 abc\nnext"));
        buffer.set_string(0, 0, "changed", Style::default());
        selection.render(&mut buffer, Color::Black, Color::White);
        assert_eq!(buffer[(0, 0)].symbol(), "你");
        assert_eq!(selection.text().as_deref(), Some("你好 abc\nnext"));
        selection.clear();
        assert!(selection.text().is_none());
    }
}
