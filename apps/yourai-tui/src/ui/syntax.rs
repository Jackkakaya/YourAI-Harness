//! File-language syntax highlighting via syntect. All emitted colors are
//! baseline palette slots (theme.rs), so `Theme::apply` remaps highlighted
//! code exactly like any other span: switch themes and highlighted code
//! follows without re-highlighting.
use super::theme::{
    SY_COMMENT, SY_FUNCTION, SY_KEYWORD, SY_NUMBER, SY_OPERATOR, SY_PUNCT, SY_STRING, SY_TYPE,
    SY_VARIABLE, TEXT,
};
use ratatui::prelude::*;
use std::sync::LazyLock;
use syntect::{
    easy::HighlightLines,
    highlighting::{
        Color as SynColor, ScopeSelectors, StyleModifier, Theme, ThemeItem, ThemeSettings,
    },
    parsing::{SyntaxDefinition, SyntaxReference, SyntaxSet},
};

const TOML_SYNTAX: &str = r#"%YAML 1.2
---
name: TOML
file_extensions: [toml]
scope: source.toml
contexts:
  main:
    - match: '#.*$'
      scope: comment.line.number-sign.toml
    - match: '"'
      scope: punctuation.definition.string.begin.toml
      push: double_string
    - match: "'"
      scope: punctuation.definition.string.begin.toml
      push: single_string
    - match: '\b(true|false)\b'
      scope: constant.language.boolean.toml
    - match: '\b[+-]?(0x[0-9A-Fa-f_]+|0o[0-7_]+|0b[01_]+|[0-9][0-9_]*(\.[0-9_]+)?([eE][+-]?[0-9_]+)?)\b'
      scope: constant.numeric.toml
    - match: '\[\[?'
      scope: punctuation.definition.table.begin.toml
    - match: '\]\]?'
      scope: punctuation.definition.table.end.toml
    - match: '[A-Za-z0-9_-]+(?=\s*=)'
      scope: variable.other.member.toml
    - match: '[=.,{}]'
      scope: punctuation.separator.toml
  double_string:
    - meta_scope: string.quoted.double.toml
    - match: '\\.'
      scope: constant.character.escape.toml
    - match: '"'
      scope: punctuation.definition.string.end.toml
      pop: true
  single_string:
    - meta_scope: string.quoted.single.toml
    - match: "'"
      scope: punctuation.definition.string.end.toml
      pop: true
"#;

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(|| {
    let mut builder = SyntaxSet::load_defaults_newlines().into_builder();
    builder.add(
        SyntaxDefinition::load_from_str(TOML_SYNTAX, true, Some("TOML"))
            .expect("embedded TOML syntax is valid"),
    );
    builder.build()
});
static THEME: LazyLock<Theme> = LazyLock::new(build_theme);

fn slot(color: Color) -> SynColor {
    let Color::Rgb(r, g, b) = color else {
        unreachable!("palette slots are rgb");
    };
    SynColor { r, g, b, a: 255 }
}

fn item(scopes: &str, fg: Color) -> ThemeItem {
    ThemeItem {
        scope: scopes
            .parse::<ScopeSelectors>()
            .expect("valid scope selector"),
        style: StyleModifier {
            foreground: Some(slot(fg)),
            background: None,
            font_style: None,
        },
    }
}

/// One syntect theme whose scope colors are the nine baseline syntax slots;
/// unmatched scopes inherit the plain TEXT foreground.
fn build_theme() -> Theme {
    Theme {
        name: Some("yourai-slots".into()),
        author: None,
        settings: ThemeSettings {
            foreground: Some(slot(TEXT)),
            ..Default::default()
        },
        scopes: vec![
            item("comment, punctuation.definition.comment", SY_COMMENT),
            item("string, meta.interpolation", SY_STRING),
            item("constant.numeric", SY_NUMBER),
            item("constant.language, constant.other, constant.character", SY_NUMBER),
            item("keyword, storage.modifier, storage.type", SY_KEYWORD),
            item("keyword.operator", SY_OPERATOR),
            item(
                "entity.name.function, support.function, variable.function, meta.function-call, support.macro, entity.name.function.macro",
                SY_FUNCTION,
            ),
            item(
                "entity.name.type, entity.name.struct, entity.name.enum, entity.name.trait, entity.name.impl, entity.name.class, support.type, support.class, entity.name.namespace, entity.name.module",
                SY_TYPE,
            ),
            item("entity.name.attribute, meta.annotation", SY_FUNCTION),
            item("variable, support.variable, variable.parameter", SY_VARIABLE),
            item("entity.name, support.constant", SY_TYPE),
            item("punctuation", SY_PUNCT),
            item("meta.tag, entity.name.tag", SY_KEYWORD),
            item("markup.heading", SY_FUNCTION),
            item("markup.raw, markup.inserted", SY_STRING),
        ],
    }
}

/// Recognize a fence language ("rust"), an extension ("rs") or a path
/// ("src/main.rs", "Cargo.toml").
fn resolve(hint: &str) -> Option<&'static SyntaxReference> {
    let hint = hint.trim();
    if hint.is_empty() {
        return None;
    }
    let lower = hint.to_lowercase();
    let base = lower.rsplit(['/', '\\']).next().unwrap_or(lower.as_str());
    let ext = base
        .rsplit_once('.')
        .map(|(_, e)| e)
        .filter(|_| base.contains('.'))
        .unwrap_or(base);
    let alias = match ext {
        "rust" => "rs",
        "python" => "py",
        "javascript" => "js",
        "typescript" => "ts",
        "shell" | "bash" | "zsh" => "sh",
        "shellsession" | "console" => "sh",
        "c++" => "cpp",
        "c#" => "cs",
        "objective-c" => "m",
        "yml" => "yaml",
        "markdown" => "md",
        "docker" => "dockerfile",
        other => other,
    };
    SYNTAXES
        .find_syntax_by_extension(alias)
        .or_else(|| SYNTAXES.find_syntax_by_name(hint))
}

/// Highlight `code`; one `Line` per source line, fg = baseline syntax slots.
/// Returns `None` when `hint` resolves to no grammar (caller falls back to a
/// single color). A grammar-internal error aborts highlighting rather than
/// emitting a half-styled block.
pub(crate) fn highlight(code: &str, hint: &str) -> Option<Vec<Line<'static>>> {
    let syntax = resolve(hint)?;
    let mut highlighter = HighlightLines::new(syntax, &THEME);
    let mut out = Vec::new();
    for line in code.lines() {
        let ranges = highlighter.highlight_line(line, &SYNTAXES).ok()?;
        let spans = ranges
            .into_iter()
            .map(|(style, text)| {
                let SynColor { r, g, b, .. } = style.foreground;
                Span::styled(text.to_string(), Style::default().fg(Color::Rgb(r, g, b)))
            })
            .collect::<Vec<_>>();
        out.push(Line::from(spans));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::super::theme::SLOTS;
    use super::*;

    fn fgs(lines: &[Line<'static>]) -> Vec<Color> {
        lines
            .iter()
            .flat_map(|l| l.spans.iter().filter_map(|s| s.style.fg))
            .collect()
    }

    #[test]
    fn rust_code_pickles_keyword_and_string_colors() {
        let lines = highlight("fn main() {\n    let s = \"hi\";\n}\n", "rust").unwrap();
        let colors = fgs(&lines);
        assert!(colors.contains(&SY_KEYWORD), "keyword colored: {colors:?}");
        assert!(colors.contains(&SY_STRING), "string colored: {colors:?}");
        assert!(
            colors
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                >= 3
        );
    }

    #[test]
    fn hint_accepts_language_extension_and_path() {
        assert!(highlight("fn f() {}\n", "Rust").is_some());
        assert!(highlight("fn f() {}\n", "rs").is_some());
        assert!(highlight("fn f() {}\n", "crates/x/src/main.rs").is_some());
        assert!(highlight("x = 1\n", "python").is_some());
        assert!(highlight("{}", "json").is_some());
        assert!(highlight("[package]\nname = \"demo\"\n", "Cargo.toml").is_some());
    }

    #[test]
    fn toml_colors_keys_strings_numbers_and_comments() {
        let lines = highlight(
            "# config\n[package]\nname = \"demo\"\nedition = 2021\npublish = false\n",
            "toml",
        )
        .unwrap();
        let colors = fgs(&lines);
        for expected in [SY_COMMENT, SY_STRING, SY_NUMBER, SY_VARIABLE] {
            assert!(
                colors.contains(&expected),
                "missing {expected:?}: {colors:?}"
            );
        }
    }

    #[test]
    fn unknown_or_empty_hint_falls_back() {
        assert!(highlight("hello", "").is_none());
        assert!(highlight("hello", "  ").is_none());
        assert!(highlight("hello", "definitely-not-a-language-123").is_none());
    }

    #[test]
    fn emitted_colors_are_baseline_slots_only() {
        // Remap invariant: Theme::apply() works by slot lookup, so highlighted
        // code must not introduce off-slot colors.
        let code = "// c\nuse std::io;\nfn f(x: u32) -> &'static str { \"s\" }\n";
        for hint in ["rust", "python", "json", "yaml", "sh", "toml"] {
            let Some(lines) = highlight(code, hint) else {
                panic!("no grammar for {hint}");
            };
            for color in fgs(&lines) {
                assert!(SLOTS.contains(&color), "{hint} emitted off-slot {color:?}");
            }
        }
    }
}
