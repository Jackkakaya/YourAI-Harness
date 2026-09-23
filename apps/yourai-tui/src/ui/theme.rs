//! Semantic palettes: renderers compose only with the baseline (Dark) slot
//! constants below; `Theme::apply` remaps every buffer cell from the baseline
//! slot to the same slot in the active palette. Cached Markdown lines and tool
//! output are always stored in baseline colors, so this lookup is unambiguous
//! as long as all Dark slots stay distinct (enforced by tests).
use ratatui::{buffer::Buffer, style::Color};
use serde::Deserialize;

const fn rgb(value: u32) -> Color {
    Color::Rgb(
        ((value >> 16) & 0xff) as u8,
        ((value >> 8) & 0xff) as u8,
        (value & 0xff) as u8,
    )
}

// Baseline slot constants = the Dark palette. Renderers build every span from
// these (and only these) colors.
pub(crate) const BG: Color = rgb(0x18191B);
pub(crate) const PANEL: Color = rgb(0x292C30);
pub(crate) const TEXT: Color = rgb(0xF1F0EA);
pub(crate) const MUTED: Color = rgb(0xB0B1AA);
pub(crate) const FAINT: Color = rgb(0x6C6D6A);
pub(crate) const YELLOW: Color = rgb(0xD6B583);
pub(crate) const ACCENT: Color = rgb(0xB7C9AD);
pub(crate) const GREEN: Color = rgb(0x9DBD9E);
pub(crate) const RED: Color = rgb(0xD8948F);
pub(crate) const BLUE: Color = rgb(0xA6B9D0);
pub(crate) const CYAN: Color = rgb(0x9DBFC0);
pub(crate) const BORDER: Color = rgb(0x555A60);
pub(crate) const DIFF_ADD_BG: Color = rgb(0x2C322F);
pub(crate) const DIFF_DEL_BG: Color = rgb(0x352B2C);
// Syntax highlight slots (Dark baseline values, derived in `Palette::derive`
// with fixed mixes; keep in sync: test `dark_baseline_slots_match_palette`).
pub(crate) const SY_KEYWORD: Color = rgb(0xC9AC9D);
pub(crate) const SY_STRING: Color = rgb(0xD7A589);
pub(crate) const SY_FUNCTION: Color = rgb(0xE2D0B1);
pub(crate) const SY_TYPE: Color = rgb(0xA7C3B8);
pub(crate) const SY_NUMBER: Color = rgb(0xA4C0A2);
pub(crate) const SY_COMMENT: Color = rgb(0xA7B6A5);
pub(crate) const SY_OPERATOR: Color = rgb(0xE8DBC6);
pub(crate) const SY_PUNCT: Color = rgb(0xD1D1CA);
pub(crate) const SY_VARIABLE: Color = rgb(0xCFD7DE);

/// Lookup ordering must match `Palette::slots()` exactly.
pub(crate) const SLOTS: [Color; 23] = [
    BG,
    PANEL,
    TEXT,
    MUTED,
    FAINT,
    YELLOW,
    ACCENT,
    GREEN,
    RED,
    BLUE,
    CYAN,
    BORDER,
    DIFF_ADD_BG,
    DIFF_DEL_BG,
    SY_KEYWORD,
    SY_STRING,
    SY_FUNCTION,
    SY_TYPE,
    SY_NUMBER,
    SY_COMMENT,
    SY_OPERATOR,
    SY_PUNCT,
    SY_VARIABLE,
];

/// Linear interpolation between two RGB colors; `t = 0` keeps `a`, `t = 1` is `b`.
/// Shared by the pulse animation, the welcome gradient and palette derivation.
pub(crate) fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    let (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) = (a, b) else {
        return if t < 0.5 { a } else { b };
    };
    let blend = |a: u8, b: u8| {
        (a as f32 + (b as f32 - a as f32) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color::Rgb(blend(ar, br), blend(ag, bg), blend(ab, bb))
}
/// Mix `from` toward `toward` by factor `t` (0=from, 1=toward).
pub(crate) fn mix(from: Color, toward: Color, t: f32) -> Color {
    lerp_color(from, toward, t)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub bg: Color,
    pub panel: Color,
    pub text: Color,
    pub muted: Color,
    pub faint: Color,
    pub yellow: Color,
    pub accent: Color,
    pub green: Color,
    pub red: Color,
    pub blue: Color,
    pub cyan: Color,
    pub border: Color,
    pub diff_add_bg: Color,
    pub diff_del_bg: Color,
    pub sy_keyword: Color,
    pub sy_string: Color,
    pub sy_function: Color,
    pub sy_type: Color,
    pub sy_number: Color,
    pub sy_comment: Color,
    pub sy_operator: Color,
    pub sy_punct: Color,
    pub sy_variable: Color,
}
impl Palette {
    /// Same slot order as `SLOTS`.
    fn slots(self) -> [Color; 23] {
        [
            self.bg,
            self.panel,
            self.text,
            self.muted,
            self.faint,
            self.yellow,
            self.accent,
            self.green,
            self.red,
            self.blue,
            self.cyan,
            self.border,
            self.diff_add_bg,
            self.diff_del_bg,
            self.sy_keyword,
            self.sy_string,
            self.sy_function,
            self.sy_type,
            self.sy_number,
            self.sy_comment,
            self.sy_operator,
            self.sy_punct,
            self.sy_variable,
        ]
    }
    /// Build from the 11 base IDE colors; faint, diff surfaces and the nine
    /// syntax colors are derived with shared formulas so they track every
    /// palette automatically (see tests `derived_slots_match_formulas` and
    /// `syntax_slots_track_their_formulas`):
    /// faint = mix(muted, bg, 0.45), diffs = mix(green/red, bg, 0.85).
    /// Baseline (Dark) results are pinned as the SY_* constants above.
    /// The positional list mirrors the compact palette table in the design document.
    #[allow(clippy::too_many_arguments)]
    fn derive(
        bg: u32,
        panel: u32,
        text: u32,
        muted: u32,
        accent: u32,
        green: u32,
        red: u32,
        yellow: u32,
        blue: u32,
        cyan: u32,
        border: u32,
    ) -> Self {
        let (bg_c, muted_c, green_c, red_c) = (rgb(bg), rgb(muted), rgb(green), rgb(red));
        let (text_c, accent_c, yellow_c, blue_c, cyan_c) =
            (rgb(text), rgb(accent), rgb(yellow), rgb(blue), rgb(cyan));
        Self {
            bg: bg_c,
            panel: rgb(panel),
            text: text_c,
            muted: muted_c,
            faint: mix(muted_c, bg_c, 0.45),
            yellow: yellow_c,
            accent: accent_c,
            green: green_c,
            red: red_c,
            blue: blue_c,
            cyan: cyan_c,
            border: rgb(border),
            diff_add_bg: mix(green_c, bg_c, 0.85),
            diff_del_bg: mix(red_c, bg_c, 0.85),
            sy_keyword: mix(accent_c, red_c, 0.55),
            sy_string: mix(yellow_c, red_c, 0.5),
            sy_function: mix(yellow_c, text_c, 0.45),
            sy_type: mix(cyan_c, accent_c, 0.4),
            sy_number: mix(green_c, accent_c, 0.28),
            sy_comment: mix(muted_c, green_c, 0.45),
            sy_operator: mix(text_c, yellow_c, 0.35),
            sy_punct: mix(muted_c, text_c, 0.5),
            sy_variable: mix(blue_c, text_c, 0.55),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Theme {
    /// Follows the OS preference once at startup; falls back to Dark.
    #[default]
    System,
    Dark,
    Light,
    OneDark,
    Monokai,
    SolarizedDark,
    SolarizedLight,
    Nord,
    Dracula,
    Catppuccin,
    TokyoNight,
    Gruvbox,
}
impl Theme {
    pub const ALL: &'static [Theme] = &[
        Theme::System,
        Theme::Dark,
        Theme::Light,
        Theme::OneDark,
        Theme::Monokai,
        Theme::SolarizedDark,
        Theme::SolarizedLight,
        Theme::Nord,
        Theme::Dracula,
        Theme::Catppuccin,
        Theme::TokyoNight,
        Theme::Gruvbox,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Dark => "dark",
            Self::Light => "light",
            Self::OneDark => "one-dark",
            Self::Monokai => "monokai",
            Self::SolarizedDark => "solarized-dark",
            Self::SolarizedLight => "solarized-light",
            Self::Nord => "nord",
            Self::Dracula => "dracula",
            Self::Catppuccin => "catppuccin",
            Self::TokyoNight => "tokyo-night",
            Self::Gruvbox => "gruvbox",
        }
    }
    /// Concrete theme `System` resolves to; concrete themes are themselves.
    pub fn effective(self) -> Self {
        if self == Self::System {
            system_resolved()
        } else {
            self
        }
    }
    pub fn next(self) -> Self {
        let at = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(at + 1) % Self::ALL.len()]
    }
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.name() == name)
    }
    pub fn palette(self) -> Palette {
        match self.effective() {
            Self::Dark | Self::System => Palette::derive(
                0x18191B, 0x292C30, 0xF1F0EA, 0xB0B1AA, 0xB7C9AD, 0x9DBD9E, 0xD8948F, 0xD6B583,
                0xA6B9D0, 0x9DBFC0, 0x555A60,
            ),
            Self::Light => Palette::derive(
                0xF7F6F2, 0xEAE8E1, 0x252823, 0x60665B, 0x4F684D, 0x3F704C, 0xA8433C, 0x876622,
                0x4A6787, 0x3C7175, 0xB3B7AD,
            ),
            Self::OneDark => Palette::derive(
                0x282C34, 0x21252B, 0xABB2BF, 0x5C6370, 0xC678DD, 0x98C379, 0xE06C75, 0xE5C07B,
                0x61AFEF, 0x56B6C2, 0x3B4048,
            ),
            Self::Monokai => Palette::derive(
                0x272822, 0x3E3D32, 0xF8F8F2, 0x75715E, 0xFD971F, 0xA6E22E, 0xF92672, 0xE6DB74,
                0x66D9EF, 0xA1EFE4, 0x49483E,
            ),
            Self::SolarizedDark => Palette::derive(
                0x002B36, 0x073642, 0x93A1A1, 0x586E75, 0x268BD2, 0x859900, 0xDC322F, 0xB58900,
                0x268BD2, 0x2AA198, 0x0E4B5B,
            ),
            Self::SolarizedLight => Palette::derive(
                0xFDF6E3, 0xEEE8D5, 0x657B83, 0x93A1A1, 0x268BD2, 0x859900, 0xDC322F, 0xB58900,
                0x268BD2, 0x2AA198, 0xD8CFAF,
            ),
            Self::Nord => Palette::derive(
                0x2E3440, 0x3B4252, 0xECEFF4, 0xA5B1C5, 0x88C0D0, 0xA3BE8C, 0xBF616A, 0xEBCB8B,
                0x81A1C1, 0x8FBCBB, 0x4C566A,
            ),
            Self::Dracula => Palette::derive(
                0x282A36, 0x343746, 0xF8F8F2, 0xA2A8C5, 0xBD93F9, 0x50FA7B, 0xFF5555, 0xF1FA8C,
                0x8BE9FD, 0x62B6CB, 0x626785,
            ),
            Self::Catppuccin => Palette::derive(
                0x1E1E2E, 0x181825, 0xCDD6F4, 0xA6ADC8, 0xCBA6F7, 0xA6E3A1, 0xF38BA8, 0xF9E2AF,
                0x89B4FA, 0x94E2D5, 0x45475A,
            ),
            Self::TokyoNight => Palette::derive(
                0x1A1B26, 0x1F2335, 0xC0CAF5, 0x565F89, 0x7AA2F7, 0x9ECE6A, 0xF7768E, 0xE0AF68,
                0x7DCFFF, 0x73DACA, 0x3B4261,
            ),
            Self::Gruvbox => Palette::derive(
                0x282828, 0x32302F, 0xEBDDB2, 0x928374, 0xFE8019, 0xB8BB26, 0xFB4934, 0xFABD2F,
                0x83A598, 0x8EC07C, 0x504945,
            ),
        }
    }
    /// Map a baseline (Dark) slot color to this theme's palette; colors outside
    /// the baseline set (pulse blends, gradient steps) pass through unchanged.
    pub fn color(self, color: Color) -> Color {
        let Some(index) = SLOTS.iter().position(|c| *c == color) else {
            return color;
        };
        self.palette().slots()[index]
    }
    pub fn label(self) -> String {
        match self {
            Self::System => format!("system ({})", self.effective().name()),
            _ => self.name().into(),
        }
    }
    pub fn apply(self, buffer: &mut Buffer) {
        if self.effective() == Self::Dark {
            return;
        }
        let palette = self.palette().slots();
        for cell in &mut buffer.content {
            if let Some(index) = SLOTS.iter().position(|c| *c == cell.fg) {
                cell.fg = palette[index];
            }
            if let Some(index) = SLOTS.iter().position(|c| *c == cell.bg) {
                cell.bg = palette[index];
            }
        }
    }
}

use std::sync::atomic::{AtomicU8, Ordering};
static RESOLVED: AtomicU8 = AtomicU8::new(0);
fn system_resolved() -> Theme {
    match RESOLVED.load(Ordering::Relaxed) {
        1 => Theme::Light,
        _ => Theme::Dark,
    }
}
/// Startup hook: store the OS probe result so `System` resolves for the run.
pub(crate) fn set_system_theme(theme: Theme) {
    RESOLVED.store(
        match theme {
            Theme::Light => 1,
            _ => 0,
        },
        Ordering::Relaxed,
    );
}
/// One-shot OS preference probe. Every step may fail (headless, missing
/// command); total failure silently resolves to Dark — never panics, never
/// blocks startup. `cmd`/`env` are injected so tests cover every branch.
pub(crate) fn detect_system_theme(
    cmd: impl Fn(&str, &[&str]) -> Option<String>,
    env: impl Fn(&str) -> Option<String>,
) -> Theme {
    // macOS: `defaults read -g AppleInterfaceStyle` prints "Dark" in dark mode
    // and errors out in light mode.
    if let Some(out) = cmd("defaults", &["read", "-g", "AppleInterfaceStyle"]) {
        return if out.contains("Dark") {
            Theme::Dark
        } else {
            Theme::Light
        };
    }
    // GNOME: 'prefer-dark' / 'default'.
    if let Some(out) = cmd(
        "gsettings",
        &["get", "org.gnome.desktop.interface", "color-scheme"],
    ) {
        return if out.contains("dark") {
            Theme::Dark
        } else {
            Theme::Light
        };
    }
    // COLORFGBG: "...;N" terminal background hint; dark backgrounds are low.
    if let Some(value) = env("COLORFGBG") {
        if let Some(n) = value
            .rsplit(';')
            .next()
            .and_then(|s| s.trim().parse::<u32>().ok())
        {
            return if n >= 8 { Theme::Light } else { Theme::Dark };
        }
    }
    Theme::Dark
}
/// Probe the real system with short-lived commands, then bind `System`.
pub(crate) fn resolve_system_from_os() {
    let cmd = |program: &str, args: &[&str]| {
        std::process::Command::new(program)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let env = |name: &str| std::env::var(name).ok();
    set_system_theme(detect_system_theme(cmd, env));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_baseline_slots_are_distinct_and_match_palette() {
        // apply() remaps by value: equal slots would make the lookup ambiguous.
        let mut unique = std::collections::HashSet::new();
        for slot in SLOTS {
            assert!(unique.insert(slot), "duplicate baseline slot {slot:?}");
        }
        assert_eq!(Theme::Dark.palette().slots(), SLOTS);
    }

    #[test]
    fn syntax_slots_track_their_formulas() {
        for theme in Theme::ALL {
            let p = theme.palette();
            let checks = [
                ("sy_keyword", p.sy_keyword, mix(p.accent, p.red, 0.55)),
                ("sy_string", p.sy_string, mix(p.yellow, p.red, 0.5)),
                ("sy_function", p.sy_function, mix(p.yellow, p.text, 0.45)),
                ("sy_type", p.sy_type, mix(p.cyan, p.accent, 0.4)),
                ("sy_number", p.sy_number, mix(p.green, p.accent, 0.28)),
                ("sy_comment", p.sy_comment, mix(p.muted, p.green, 0.45)),
                ("sy_operator", p.sy_operator, mix(p.text, p.yellow, 0.35)),
                ("sy_punct", p.sy_punct, mix(p.muted, p.text, 0.5)),
                ("sy_variable", p.sy_variable, mix(p.blue, p.text, 0.55)),
            ];
            for (name, actual, expected) in checks {
                assert_eq!(actual, expected, "{} {name}", theme.name());
            }
        }
    }

    #[test]
    fn syntax_slots_stay_visible_against_background() {
        // Syntax accents are deliberately subtler than message text; 2.0 keeps
        // every scope legible on bg without competing with the diff surfaces.
        const MIN: f64 = 2.0;
        for theme in Theme::ALL {
            let p = theme.palette();
            let slots = [
                ("sy_keyword", p.sy_keyword),
                ("sy_string", p.sy_string),
                ("sy_function", p.sy_function),
                ("sy_type", p.sy_type),
                ("sy_number", p.sy_number),
                ("sy_comment", p.sy_comment),
                ("sy_operator", p.sy_operator),
                ("sy_punct", p.sy_punct),
                ("sy_variable", p.sy_variable),
            ];
            for (name, fg) in slots {
                let ratio = contrast(fg, p.bg);
                assert!(
                    ratio >= MIN,
                    "{}: {name} on bg contrast {ratio:.2} < {MIN}",
                    theme.name()
                );
            }
        }
    }

    #[test]
    fn derived_slots_match_formulas() {
        for theme in Theme::ALL {
            let p = theme.palette();
            assert_eq!(p.faint, mix(p.muted, p.bg, 0.45), "{} faint", theme.name());
            assert_eq!(
                p.diff_add_bg,
                mix(p.green, p.bg, 0.85),
                "{} diff_add_bg",
                theme.name()
            );
            assert_eq!(
                p.diff_del_bg,
                mix(p.red, p.bg, 0.85),
                "{} diff_del_bg",
                theme.name()
            );
        }
    }

    #[test]
    fn all_palettes_are_complete_and_names_roundtrip() {
        assert_eq!(Theme::ALL.len(), 12);
        for theme in Theme::ALL {
            let parsed = Theme::parse(theme.name()).unwrap();
            assert_eq!(*theme, parsed);
            let next_idx = (Theme::ALL.iter().position(|t| t == theme).unwrap() + 1) % 12;
            assert_eq!(theme.next(), Theme::ALL[next_idx]);
            // palette() must not panic for any theme.
            let _ = theme.palette();
        }
        assert_eq!(Theme::parse("DARK"), None);
    }

    fn luminance(c: Color) -> f64 {
        let Color::Rgb(r, g, b) = c else {
            return 0.0;
        };
        let channel = |v: u8| {
            let s = v as f64 / 255.0;
            if s <= 0.03928 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
    }
    fn contrast(a: Color, b: Color) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        let (hi, lo) = (la.max(lb), la.min(lb));
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn foreground_slots_keep_readable_contrast() {
        // §9 contrast guard: TEXT|ACCENT|GREEN|RED|YELLOW|BLUE vs BG|PANEL ≥ 3.0
        const MIN: f64 = 3.0;
        // Intentionally low-emphasis pairs (e.g. red on a dark warm panel) are exempt.
        const EXEMPT: &[(&str, &str, &str)] = &[
            ("monokai", "red", "panel"),
            ("solarized-dark", "red", "panel"),
            ("solarized-light", "green", "bg"),
            ("solarized-light", "green", "panel"),
            ("solarized-light", "yellow", "bg"),
            ("solarized-light", "yellow", "panel"),
            ("nord", "red", "panel"),
        ];
        for theme in Theme::ALL {
            let p = theme.palette();
            let fgs = [
                ("text", p.text),
                ("accent", p.accent),
                ("green", p.green),
                ("red", p.red),
                ("yellow", p.yellow),
                ("blue", p.blue),
            ];
            for (name, fg) in fgs {
                for (surface, sname) in [(p.bg, "bg"), (p.panel, "panel")] {
                    if EXEMPT
                        .iter()
                        .any(|e| e.0 == theme.name() && e.1 == name && e.2 == sname)
                    {
                        continue;
                    }
                    let ratio = contrast(fg, surface);
                    assert!(
                        ratio >= MIN,
                        "{}: {name} on {sname} contrast {ratio:.2} < {MIN}",
                        theme.name()
                    );
                }
            }
        }
    }

    #[test]
    fn detect_system_theme_prefers_macos_then_gnome_then_env() {
        let none = |_: &str, _: &[&str]| None;
        let noenv = |_: &str| None;
        // macOS dark / light.
        let mac_dark = |_: &str, _: &[&str]| Some("Dark\n".into());
        assert_eq!(detect_system_theme(mac_dark, noenv), Theme::Dark);
        let mac_light = |_: &str, _: &[&str]| Some("Light\n".into());
        assert_eq!(detect_system_theme(mac_light, noenv), Theme::Light);
        // GNOME probes used when macOS probe is missing.
        let gnome =
            |program: &str, _: &[&str]| (program == "gsettings").then(|| "'prefer-dark'\n".into());
        assert_eq!(detect_system_theme(gnome, noenv), Theme::Dark);
        let gnome_light =
            |program: &str, _: &[&str]| (program == "gsettings").then(|| "'default'\n".into());
        assert_eq!(detect_system_theme(gnome_light, noenv), Theme::Light);
        // COLORFGBG tail decides; 7 and below fall back to Dark.
        let env = |_: &str| Some("0;15".into());
        assert_eq!(detect_system_theme(none, env), Theme::Light);
        let env = |_: &str| Some("15;0".into());
        assert_eq!(detect_system_theme(none, env), Theme::Dark);
        // Total failure path is silent Dark, not a panic.
        assert_eq!(detect_system_theme(none, noenv), Theme::Dark);
        let env = |_: &str| Some("garbage".into());
        assert_eq!(detect_system_theme(none, env), Theme::Dark);
    }

    #[test]
    fn system_resolves_to_detected_and_labels_resolution() {
        set_system_theme(Theme::Light);
        assert_eq!(Theme::System.effective(), Theme::Light);
        assert_eq!(Theme::System.label(), "system (light)");
        set_system_theme(Theme::Dark);
        assert_eq!(Theme::System.effective(), Theme::Dark);
        assert_eq!(Theme::System.label(), "system (dark)");
    }

    #[test]
    fn remap_only_touches_baseline_slots() {
        let custom = Color::Rgb(1, 2, 3);
        for theme in Theme::ALL {
            assert_eq!(theme.color(custom), custom);
            for slot in SLOTS {
                // Dark identity-maps; every other theme must change baseline slots.
                if theme.effective() != Theme::Dark {
                    assert_ne!(
                        theme.color(slot),
                        slot,
                        "{} remaps baseline {slot:?}",
                        theme.name()
                    );
                }
            }
        }
        // Dark is the identity mapping.
        for slot in SLOTS {
            assert_eq!(Theme::Dark.color(slot), slot);
        }
    }
}
