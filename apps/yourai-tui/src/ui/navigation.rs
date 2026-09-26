//! Reading position and pending navigation have one owner. Every direct scroll
//! or follow operation supersedes pending intents; callers cannot forget to
//! clear a renderer-side target. Layout resolves intents before presentation.
#[derive(Default)]
pub(super) struct Navigation {
    offset: usize,
    intent: Option<Intent>,
}
enum Intent {
    Anchor(u64, usize),
    Reveal(u64),
    Turn(u64),
}
impl Navigation {
    pub fn offset(&self) -> usize {
        self.offset
    }
    pub fn follow(&mut self) {
        self.set_offset(0);
    }
    pub fn set_offset(&mut self, offset: usize) {
        self.offset = offset;
        self.intent = None;
    }
    pub fn scroll(&mut self, rows: usize, up: bool, maximum: usize) {
        self.set_offset(if up {
            self.offset.saturating_add(rows).min(maximum)
        } else {
            self.offset.saturating_sub(rows)
        });
    }
    pub fn anchor(&mut self, id: u64, row: usize) {
        self.intent = Some(Intent::Anchor(id, row));
    }
    pub fn reveal(&mut self, id: Option<u64>) {
        self.intent = id.map(Intent::Reveal);
    }
    pub fn turn(&mut self, id: Option<u64>) {
        self.intent = id.map(Intent::Turn);
    }
    pub fn jump(&mut self, turns: &[(usize, u64)], total: usize, height: usize, previous: bool) {
        // Relative commands compose against the pending destination, not the
        // last presented offset. Absolute commands still replace the intent.
        let start = match self.intent {
            Some(Intent::Turn(id)) => turns
                .iter()
                .find(|(_, key)| *key == id)
                .map(|(line, _)| *line),
            _ => None,
        }
        .unwrap_or_else(|| total.saturating_sub(self.offset + height));
        let target = if previous {
            turns
                .iter()
                .rev()
                .find(|(line, _)| *line < start)
                .or_else(|| turns.first())
        } else {
            turns.iter().find(|(line, _)| *line > start)
        };
        if let Some((_, id)) = target {
            self.turn(Some(*id));
        } else if !previous {
            self.follow();
        }
    }
    /// Restore an item-based reading anchor without discarding a newer explicit
    /// navigation intent. resolve() applies that intent against the new layout.
    pub fn preserve_line(&mut self, line: usize, total: usize, height: usize) {
        self.offset = total.saturating_sub(height + line);
    }
    pub fn resolve(
        &mut self,
        total: usize,
        height: usize,
        headers: &[(usize, u64)],
        turns: &[(usize, u64)],
    ) {
        let target = match self.intent.take() {
            Some(Intent::Anchor(id, row)) => headers
                .iter()
                .find(|(_, key)| *key == id)
                .map(|(line, _)| (*line, row)),
            Some(Intent::Turn(id)) => turns
                .iter()
                .find(|(_, key)| *key == id)
                .map(|(line, _)| (*line, 0)),
            Some(Intent::Reveal(id)) => {
                headers
                    .iter()
                    .find(|(_, key)| *key == id)
                    .and_then(|(line, _)| {
                        let end = total.saturating_sub(self.offset);
                        (*line < end.saturating_sub(height) || *line >= end).then_some((*line, 0))
                    })
            }
            None => None,
        };
        if let Some((line, row)) = target {
            self.offset = total.saturating_sub(height + line.saturating_sub(row));
        }
        self.offset = self.offset.min(total.saturating_sub(height));
    }
}

#[cfg(test)]
mod tests {
    use super::Navigation;
    #[test]
    fn latest_user_intent_wins_including_follow_and_manual_scroll() {
        let mut nav = Navigation::default();
        nav.turn(Some(1));
        nav.follow();
        nav.resolve(100, 10, &[], &[(0, 1)]);
        assert_eq!(nav.offset(), 0);
        nav.anchor(1, 4);
        nav.scroll(3, true, 90);
        nav.resolve(100, 10, &[(0, 1)], &[]);
        assert_eq!(nav.offset(), 3);
        nav.reveal(Some(1));
        nav.turn(Some(2));
        nav.resolve(100, 10, &[(0, 1)], &[(50, 2)]);
        assert_eq!(nav.offset(), 40);
        nav.jump(&[(0, 1), (50, 2)], 100, 10, false);
        nav.resolve(100, 10, &[], &[(0, 1), (50, 2)]);
        assert_eq!(nav.offset(), 0);
    }

    #[test]
    fn relative_turn_navigation_accumulates_before_layout() {
        let turns = [(0, 1), (30, 2), (60, 3)];
        let mut nav = Navigation::default();
        nav.jump(&turns, 100, 10, true);
        nav.jump(&turns, 100, 10, true);
        nav.resolve(100, 10, &[], &turns);
        assert_eq!(nav.offset(), 60);
        nav.jump(&turns, 100, 10, true);
        nav.jump(&turns, 100, 10, false);
        nav.resolve(100, 10, &[], &turns);
        assert_eq!(nav.offset(), 60);
        nav.turn(Some(3));
        nav.jump(&turns, 100, 10, true);
        nav.resolve(100, 10, &[], &turns);
        assert_eq!(nav.offset(), 60);
    }
}
