//! Semantic palette mapping also recolors cached Markdown and tool output.
use super::render::{ACCENT, BG, BLUE, BORDER, GREEN, MUTED, PANEL, RED, TEXT};
use ratatui::{buffer::Buffer, style::Color};
use serde::Deserialize;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Dark,
    Light,
    Nord,
    Dracula,
}
impl Theme {
    pub fn name(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Nord => "nord",
            Self::Dracula => "dracula",
        }
    }
    pub fn next(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Nord,
            Self::Nord => Self::Dracula,
            Self::Dracula => Self::Dark,
        }
    }
    pub fn parse(name: &str) -> Option<Self> {
        [Self::Dark, Self::Light, Self::Nord, Self::Dracula]
            .into_iter()
            .find(|t| t.name() == name)
    }
    pub fn color(self, color: Color) -> Color {
        let colors = [BG, PANEL, TEXT, MUTED, ACCENT, GREEN, RED, BLUE, BORDER];
        let Some(index) = colors.iter().position(|c| *c == color) else {
            return color;
        };
        let palette = match self {
            Self::Dark => return color,
            Self::Light => [
                0xf7f8fa, 0xe9edf2, 0x202633, 0x596579, 0x955000, 0x227443, 0xb62e43, 0x275da8,
                0xb8c1cf,
            ],
            Self::Nord => [
                0x2e3440, 0x3b4252, 0xeceff4, 0xa5b1c5, 0x88c0d0, 0xa3be8c, 0xbf616a, 0x81a1c1,
                0x58667e,
            ],
            Self::Dracula => [
                0x282a36, 0x343746, 0xf8f8f2, 0xa2a8c5, 0xbd93f9, 0x50fa7b, 0xff5555, 0x8be9fd,
                0x626785,
            ],
        };
        let rgb = palette[index];
        Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
    }
    pub fn apply(self, buffer: &mut Buffer) {
        if self == Self::Dark {
            return;
        }
        for cell in &mut buffer.content {
            cell.fg = self.color(cell.fg);
            cell.bg = self.color(cell.bg);
        }
    }
}
