//! Geometry shared by the startup launcher and in-session pickers.
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

#[cfg(test)]
mod tests {
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
}
