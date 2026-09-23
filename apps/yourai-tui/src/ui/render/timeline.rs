//! Per-item layout cache. Content versions are owned by View, animation by Renderer.
use super::*;
use std::collections::HashMap;
#[derive(PartialEq, Eq)]
struct Key {
    version: u64,
    width: usize,
    quota: usize,
    theme: Theme,
    expanded: bool,
    selected: bool,
    thinking: bool,
}
struct Entry {
    key: Key,
    lines: Vec<Line<'static>>,
}
#[derive(Default)]
pub(super) struct TimelineCache {
    entries: HashMap<u64, Entry>,
    pub turns: Vec<(usize, u64)>,
    #[cfg(test)]
    pub builds: usize,
}
impl TimelineCache {
    pub fn layout(
        &mut self,
        v: &View,
        width: usize,
        edit_preview_rows: usize,
        tick: u64,
    ) -> (Vec<Line<'static>>, Vec<(usize, u64)>) {
        self.turns.clear();
        let mut lines = Vec::new();
        let mut headers = Vec::new();
        let first = v.item_id(0);
        self.entries
            .retain(|id, _| *id >= first && *id < first + v.items().len() as u64);
        for (index, item) in v.items().iter().enumerate() {
            let id = v.item_id(index);
            let key = Key {
                version: v.item_version(index),
                width,
                quota: edit_preview_rows,
                theme: v.theme,
                expanded: v.expanded(id),
                selected: v.selected() == Some(id),
                thinking: v.is_thinking_at(index),
            };
            if self.entries.get(&id).is_none_or(|e| e.key != key) {
                let rendered = item_lines(v, index, width, edit_preview_rows, tick);
                self.entries.insert(
                    id,
                    Entry {
                        key,
                        lines: rendered,
                    },
                );
                #[cfg(test)]
                {
                    self.builds += 1;
                }
            }
            if matches!(
                item,
                Item::Text {
                    role: Role::User,
                    ..
                }
            ) {
                self.turns.push((lines.len(), id));
            }
            if View::foldable(item) {
                headers.push((lines.len(), id));
            }
            lines.extend(self.entries[&id].lines.iter().cloned());
        }
        (lines, headers)
    }
}
fn item_lines(
    v: &View,
    index: usize,
    width: usize,
    edit_preview_rows: usize,
    tick: u64,
) -> Vec<Line<'static>> {
    let item = &v.items()[index];
    let id = v.item_id(index);
    let expanded = v.expanded(id);
    let selected = v.selected() == Some(id);
    let mut lines = Vec::new();
    match item {
        Item::Text {
            role: Role::Thinking,
            text,
        } => {
            let chars = text.chars().count();
            let size = if chars >= 1000 {
                format!("{:.1}K", chars as f64 / 1000.0)
            } else {
                format!("{chars}")
            };
            let first_line = text
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .chars()
                .take(40)
                .collect::<String>();
            let is_running = v.is_thinking_at(index);
            let glyph = "✦";
            let color = if selected || is_running {
                ACCENT
            } else {
                MUTED
            };
            let title = if first_line.is_empty() {
                format!("┃ {glyph} Thinking · {size}")
            } else {
                format!("┃ {glyph} Thinking · {size} · {first_line}")
            };
            lines.push(Line::from(Span::styled(
                title,
                Style::default().fg(color).add_modifier(Modifier::ITALIC),
            )));
            if expanded {
                let dim = |line: Line<'static>| {
                    Line::from(
                        line.spans
                            .into_iter()
                            .map(|s| {
                                Span::styled(
                                    s.content,
                                    s.style.fg(MUTED).add_modifier(Modifier::ITALIC),
                                )
                            })
                            .collect::<Vec<_>>(),
                    )
                };
                lines.extend(
                    crate::ui::markdown::render(text, width)
                        .into_iter()
                        .map(dim),
                );
            }
        }
        Item::Text {
            role: Role::User,
            text,
        } => {
            lines.push(Line::from(vec![
                Span::styled("  YOU ", Style::default().fg(ACCENT).bold()),
                Span::styled(
                    "─".repeat(width.saturating_sub(7)),
                    Style::default().fg(BORDER),
                ),
            ]));
            lines.push(Line::from(" ".repeat(width)).style(Style::default().bg(PANEL)));
            for line in text.lines() {
                for row in wrap(
                    line,
                    Style::default().fg(TEXT).bold(),
                    width.saturating_sub(2),
                    "  ",
                ) {
                    let padding = width.saturating_sub(row.width());
                    let mut row = row;
                    row.spans.push(Span::raw(" ".repeat(padding)));
                    lines.push(row.style(Style::default().bg(PANEL)));
                }
            }
            lines.push(Line::from(" ".repeat(width)).style(Style::default().bg(PANEL)));
        }
        Item::Text { text, .. } => lines.extend(crate::ui::markdown::render(text, width)),
        Item::Notice { level, text } => {
            let color = match level {
                Level::Error => RED,
                Level::Warning => ACCENT,
                _ => MUTED,
            };
            for line in text.lines() {
                lines.extend(wrap(line, Style::default().fg(color), width, "  · "));
            }
        }
        Item::Tool(t) => {
            lines.push(tool_title(t, selected, width, tick));
            if expanded {
                tool_expanded(t, &mut lines, width, v.theme);
            } else {
                tool_preview(t, &mut lines, width, v.theme, edit_preview_rows);
            }
        }
    }
    lines.push(Line::default());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn streaming_and_animation_reuse_history_but_changes_invalidate() {
        let mut view = View::default();
        for _ in 0..100 {
            view.event(Out::Message {
                text: "```rust\nfn main() {}\n```".into(),
            });
            view.settle();
        }
        let mut cache = TimelineCache::default();
        cache.layout(&view, 80, 3, 0);
        assert_eq!(cache.builds, 100);
        view.event(Out::ToolStarted {
            id: "run".into(),
            name: "shell".into(),
            input: json!({"command":"tests"}),
        });
        cache.layout(&view, 80, 3, 1);
        assert_eq!(cache.builds, 101);
        for tick in 2..20 {
            cache.layout(&view, 80, 3, tick);
        }
        assert_eq!(cache.builds, 101, "spinner must not parse history");
        view.event(Out::ToolProgress {
            id: "run".into(),
            payload: json!({"stdout":"passed"}),
        });
        cache.layout(&view, 80, 3, 21);
        assert_eq!(cache.builds, 102);
        view.event(Out::Chunk {
            text: "first".into(),
        });
        cache.layout(&view, 80, 3, 22);
        view.event(Out::Chunk {
            text: " second".into(),
        });
        let (lines, _) = cache.layout(&view, 80, 3, 23);
        assert_eq!(cache.builds, 104, "only streaming item is invalidated");
        assert!(lines.iter().any(|l| l.to_string().contains("first second")));
        view.clear_timeline();
        let (lines, _) = cache.layout(&view, 80, 3, 24);
        assert!(lines.is_empty());
        assert!(cache.entries.is_empty());
    }
}
