//! Geometry and key handling shared by the startup launcher and in-session
//! pickers. No rendering here — callers bring their own chrome.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use std::ops::Range;

pub fn centered(area: Rect, width: u16, height: usize) -> Rect {
    let width = width.min(area.width);
    let height = height.min(usize::from(area.height)) as u16;
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

/// Keep the selection visible without converting an unbounded row count to u16.
pub fn visible_rows(len: usize, selected: usize, capacity: u16) -> Range<usize> {
    let capacity = usize::from(capacity);
    let selected = selected.min(len.saturating_sub(1));
    let start = selected.saturating_add(1).saturating_sub(capacity);
    start..start.saturating_add(capacity).min(len)
}

/// Key handling shared by every query-filtered list (the launcher and the
/// `/sessions` overlay): Up/Down and Ctrl-P/N step with clamping at both
/// ends, printable characters extend the query, Backspace removes the last
/// character. Any query edit resets the selection to the top. Returns `true`
/// when the key was consumed; callers keep Enter, Esc, Ctrl-D and friends.
pub fn filter_input(
    key: KeyEvent,
    query: &mut String,
    selected: &mut usize,
    filtered_len: usize,
) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let step = |selected: &mut usize, back: bool| {
        if filtered_len == 0 {
            return;
        }
        *selected = if back {
            selected.saturating_sub(1)
        } else {
            (*selected + 1).min(filtered_len - 1)
        };
    };
    match key.code {
        KeyCode::Up => step(selected, true),
        KeyCode::Down => step(selected, false),
        KeyCode::Char('p') if ctrl => step(selected, true),
        KeyCode::Char('n') if ctrl => step(selected, false),
        KeyCode::Backspace => {
            query.pop();
            *selected = 0;
        }
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            query.push(c);
            *selected = 0;
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;

    #[test]
    fn geometry_fits_even_empty_and_tiny_terminals() {
        for width in [0, 1, 20, 30, 40, 48, 120] {
            for height in [0, 1, 2, 3, 24, 40] {
                let area = Rect::new(0, 0, width, height);
                for desired in [28, 40, 56, 80] {
                    let rect = centered(area, desired, usize::MAX);
                    assert!(rect.right() <= area.right());
                    assert!(rect.bottom() <= area.bottom());
                }
            }
        }
    }

    #[test]
    fn selected_row_remains_visible_in_long_lists() {
        for selected in 0..100_000 {
            let range = visible_rows(100_000, selected, 12);
            assert!(range.contains(&selected));
            assert!(range.len() <= 12);
        }
        assert_eq!(visible_rows(0, 0, 10), 0..0);
        assert!(visible_rows(20, 15, 0).is_empty());
    }

    #[test]
    fn filter_input_steps_clamps_edits_and_passes_through() {
        let key = |code: KeyCode, m: KeyModifiers| KeyEvent::new(code, m);
        let mut query = String::new();
        let mut selected = 0;
        // Empty list: navigation is consumed but cannot move.
        assert!(filter_input(
            key(KeyCode::Down, KeyModifiers::NONE),
            &mut query,
            &mut selected,
            0
        ));
        assert_eq!(selected, 0);
        // Typing edits the query and resets the selection.
        selected = 3;
        assert!(filter_input(
            key(KeyCode::Char('x'), KeyModifiers::NONE),
            &mut query,
            &mut selected,
            5
        ));
        assert_eq!((query.as_str(), selected), ("x", 0));
        // Down/Ctrl-N step forward and clamp at the last row.
        assert!(filter_input(
            key(KeyCode::Down, KeyModifiers::NONE),
            &mut query,
            &mut selected,
            3
        ));
        assert_eq!(selected, 1);
        assert!(filter_input(
            key(KeyCode::Char('n'), KeyModifiers::CONTROL),
            &mut query,
            &mut selected,
            3
        ));
        assert_eq!(selected, 2);
        assert!(filter_input(
            key(KeyCode::Down, KeyModifiers::NONE),
            &mut query,
            &mut selected,
            3
        ));
        assert_eq!(selected, 2, "clamped at len-1");
        // Up/Ctrl-P step back and clamp at the first row.
        assert!(filter_input(
            key(KeyCode::Char('p'), KeyModifiers::CONTROL),
            &mut query,
            &mut selected,
            3
        ));
        assert_eq!(selected, 1);
        assert!(filter_input(
            key(KeyCode::Up, KeyModifiers::NONE),
            &mut query,
            &mut selected,
            3
        ));
        assert_eq!(selected, 0);
        // Backspace edits.
        assert!(filter_input(
            key(KeyCode::Backspace, KeyModifiers::NONE),
            &mut query,
            &mut selected,
            3
        ));
        assert_eq!(query, "");
        // Everything else belongs to the caller.
        for code in [KeyCode::Enter, KeyCode::Esc, KeyCode::PageUp] {
            assert!(!filter_input(
                key(code, KeyModifiers::NONE),
                &mut query,
                &mut selected,
                3
            ));
        }
        assert!(!filter_input(
            key(KeyCode::Char('d'), KeyModifiers::CONTROL),
            &mut query,
            &mut selected,
            3
        ));
    }
}
