//! Static terminal wordmark shared by the welcome screen and session launcher.
//! Semantic color slots keep the mark consistent with every configured theme.
use crate::ui::theme::{BRAND_TEAL, BRAND_VIOLET};
use ratatui::prelude::*;

pub(crate) fn wordmark() -> Vec<Line<'static>> {
    const LETTERS: [[&str; 5]; 6] = [
        ["██  ██", "██  ██", " ████ ", "  ██  ", "  ██  "],
        [" ████ ", "██  ██", "██  ██", "██  ██", " ████ "],
        ["██  ██", "██  ██", "██  ██", "██  ██", " ████ "],
        ["█████ ", "██  ██", "█████ ", "██ ██ ", "██  ██"],
        [" ████ ", "██  ██", "██████", "██  ██", "██  ██"],
        ["██████", "  ██  ", "  ██  ", "  ██  ", "██████"],
    ];
    let colors = [
        BRAND_TEAL,
        BRAND_TEAL,
        BRAND_TEAL,
        BRAND_TEAL,
        BRAND_VIOLET,
        BRAND_VIOLET,
    ];
    (0..5)
        .map(|row| {
            let mut spans = Vec::new();
            for (index, letter) in LETTERS.iter().enumerate() {
                if index > 0 {
                    spans.push(Span::raw("  "));
                }
                spans.push(Span::styled(
                    letter[row],
                    Style::default().fg(colors[index]),
                ));
            }
            Line::from(spans).alignment(Alignment::Center)
        })
        .collect()
}
