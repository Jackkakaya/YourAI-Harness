//! Per-item layout cache. Content versions are owned by View, animation by Renderer.
use super::cards::{block_tool, surface_row, tool_expanded, tool_preview, tool_title_row};
use crate::text::elide;
use crate::ui::markdown::wrap_text;
use crate::ui::state::{Item, Role, ToolStatus, View};
use crate::ui::theme::{Theme, ACCENT, CODE_SURFACE, MUTED, RED, TEXT, USER_SURFACE, YELLOW};
use ratatui::prelude::*;
use std::{collections::HashMap, ops::Range, sync::Arc};
use yourai_core::prelude::Level;
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
    lines: Arc<Vec<Line<'static>>>,
}
/// Indexed immutable blocks: revisions share history, scrolling clones only visible rows.
#[derive(Default)]
pub(super) struct LayoutLines {
    blocks: Vec<LayoutBlock>,
    len: usize,
}
struct LayoutBlock {
    id: u64,
    start: usize,
    lines: Arc<Vec<Line<'static>>>,
}
impl LayoutLines {
    fn push(&mut self, id: u64, lines: Arc<Vec<Line<'static>>>) {
        let start = self.len;
        self.len += lines.len();
        self.blocks.push(LayoutBlock { id, start, lines });
    }
    #[cfg(test)]
    pub fn iter(&self) -> impl Iterator<Item = &Line<'static>> {
        self.blocks.iter().flat_map(|block| block.lines.iter())
    }
    pub fn len(&self) -> usize {
        self.len
    }
    /// Stable item identity plus a row within its rendered block. This works
    /// for head eviction as well as tail growth; total row deltas do not.
    pub fn anchor_at(&self, row: usize) -> Option<(u64, usize)> {
        let index = self
            .blocks
            .partition_point(|b| b.start + b.lines.len() <= row);
        self.blocks
            .get(index)
            .map(|b| (b.id, row.saturating_sub(b.start)))
    }
    pub fn locate(&self, (id, row): (u64, usize)) -> Option<usize> {
        // If the item was evicted, use the first surviving block. If folded
        // into a group, use that group's header. Never transfer to an unrelated
        // item merely because it inherited an old numeric index.
        let index = self
            .blocks
            .partition_point(|b| b.id <= id)
            .saturating_sub(1);
        self.blocks.get(index).map(|b| {
            b.start
                + if b.id == id {
                    row.min(b.lines.len().saturating_sub(1))
                } else {
                    0
                }
        })
    }
    pub fn viewport(&self, range: Range<usize>) -> Vec<Line<'static>> {
        let mut visible = Vec::with_capacity(range.end.saturating_sub(range.start));
        let first = self
            .blocks
            .partition_point(|block| block.start + block.lines.len() <= range.start);
        for block in &self.blocks[first..] {
            let start = block.start;
            let lines = &block.lines;
            if start >= range.end {
                break;
            }
            visible.extend(
                lines[range.start.saturating_sub(start)
                    ..range.end.saturating_sub(start).min(lines.len())]
                    .iter()
                    .cloned(),
            );
        }
        visible
    }
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
    ) -> (LayoutLines, Vec<(usize, u64)>) {
        self.turns.clear();
        let mut lines = LayoutLines::default();
        let mut headers = Vec::new();
        let first = v.item_id(0);
        self.entries
            .retain(|id, _| *id >= first && *id < first + v.items().len() as u64);
        let mut collapsed_until = 0;
        let mut expanded_until = 0;
        for (index, item) in v.items().iter().enumerate() {
            if index < collapsed_until {
                continue;
            }
            let id = v.item_id(index);
            if index >= expanded_until && quiet_exploration(item) {
                let end = index
                    + v.items()
                        .iter()
                        .skip(index)
                        .take_while(|item| quiet_exploration(item))
                        .count();
                expanded_until = end;
                if end - index >= 2
                    && !(index..end).any(|i| {
                        v.expanded(v.item_id(i))
                            || (i != index && v.selected() == Some(v.item_id(i)))
                    })
                {
                    headers.push((lines.len(), id));
                    lines.push(
                        id,
                        Arc::new(vec![
                            Line::from(Span::styled(
                                elide(
                                    &format!("  ▸ Read & search · {} operations", end - index),
                                    width,
                                ),
                                Style::default().fg(MUTED),
                            )),
                            Line::default(),
                        ]),
                    );
                    collapsed_until = end;
                    continue;
                }
            }
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
                        lines: Arc::new(rendered),
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
            lines.push(id, self.entries[&id].lines.clone());
        }
        (lines, headers)
    }
}
/// Group only successful, read-only exploration; edits, errors and live work stay visible.
fn quiet_exploration(item: &Item) -> bool {
    matches!(item, Item::Tool(t) if t.status == ToolStatus::Done
        && t.exit_code.is_none_or(|code| code == 0)
        && matches!(t.name.as_str(), "read" | "glob" | "grep" | "search" | "websearch" | "webfetch"))
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
                format!("  {glyph} Thinking")
            } else {
                format!("  {glyph} Thinking · {first_line}")
            };
            lines.push(Line::from(Span::styled(
                elide(&title, width),
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
            lines.push(Line::from(" ".repeat(width)).style(Style::default().bg(USER_SURFACE)));
            for line in text.lines() {
                for row in wrap_text(
                    line,
                    Style::default().fg(TEXT).bold(),
                    width.saturating_sub(2),
                    "  ",
                ) {
                    let padding = width.saturating_sub(row.width());
                    let mut row = row;
                    row.spans.push(Span::raw(" ".repeat(padding)));
                    lines.push(row.style(Style::default().bg(USER_SURFACE)));
                }
            }
            lines.push(Line::from(" ".repeat(width)).style(Style::default().bg(USER_SURFACE)));
        }
        Item::Text { text, .. } => {
            // Markdown already owns the shared two-column body inset.
            lines.extend(crate::ui::markdown::render(text, width));
        }
        Item::Notice { level, text } => {
            let color = match level {
                Level::Error => RED,
                Level::Warning => YELLOW,
                _ => MUTED,
            };
            for line in text.lines() {
                lines.extend(wrap_text(line, Style::default().fg(color), width, "  · "));
            }
        }
        Item::Tool(t) => {
            let mut body = Vec::new();
            if expanded {
                tool_expanded(t, &mut body, width, v.theme);
            } else {
                tool_preview(t, &mut body, width, v.theme, edit_preview_rows);
            }
            if block_tool(t, expanded) {
                lines.push(tool_title_row(t, selected, expanded, width, tick));
                if !body.is_empty() {
                    lines.push(surface_row(Line::default(), width, CODE_SURFACE));
                    lines.extend(
                        body.into_iter()
                            .map(|row| surface_row(row, width, CODE_SURFACE)),
                    );
                }
                lines.push(surface_row(Line::default(), width, CODE_SURFACE));
            } else {
                lines.push(tool_title_row(t, selected, expanded, width, tick));
                lines.extend(body);
            }
        }
    }
    lines.push(Line::default());
    lines
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;
    use crate::ui::theme::DIFF_ADD_BG;
    use serde_json::json;
    use yourai_core::prelude::Out;
    #[test]
    fn substantial_tools_have_surfaces_while_reads_stay_inline() {
        let mut view = View::default();
        for (id, name, input, output) in [
            (
                "read",
                "read",
                json!({"path":"a.rs"}),
                json!({"ok":true,"content":"source"}),
            ),
            (
                "shell",
                "shell",
                json!({"command":"cargo check"}),
                json!({"ok":true,"exit_code":0,"stdout":"Build complete"}),
            ),
        ] {
            view.event(Out::ToolStarted {
                id: id.into(),
                name: name.into(),
                input,
            });
            view.event(Out::ToolDone {
                id: id.into(),
                name: name.into(),
                output,
                is_error: false,
            });
        }
        for width in [30, 80, 120] {
            let read = item_lines(&view, 0, width, 3, 0);
            assert_eq!(read[0].style.bg, None);
            let shell = item_lines(&view, 1, width, 3, 0);
            assert_eq!(shell[0].style.bg, Some(USER_SURFACE));
            let output = shell
                .iter()
                .find(|row| row.to_string().contains("Build complete"))
                .unwrap();
            assert_eq!(output.style.bg, Some(CODE_SURFACE));
            assert_eq!(output.width(), width);
            assert_eq!(
                shell.last().unwrap().style.bg,
                None,
                "blocks end with an unpainted gutter"
            );
        }
        let diff = Line::from("+ change").style(Style::default().bg(DIFF_ADD_BG));
        assert_eq!(
            surface_row(diff, 30, CODE_SURFACE).style.bg,
            Some(DIFF_ADD_BG)
        );
    }

    #[test]
    fn viewport_matches_full_layout_and_reuses_unchanged_blocks() {
        let mut view = View::default();
        view.user("你好，检查 parser", false);
        view.event(Out::Chunk {
            text: "## Result\n\n```rust\nlet x = 1;\n```".into(),
        });
        let mut cache = TimelineCache::default();
        let (before, _) = cache.layout(&view, 40, 3, 0);
        let full = before.iter().cloned().collect::<Vec<_>>();
        for start in 0..=full.len() {
            for end in start..=full.len() {
                assert_eq!(before.viewport(start..end), full[start..end]);
            }
        }
        view.event(Out::Chunk {
            text: "\nMore details".into(),
        });
        let (after, _) = cache.layout(&view, 40, 3, 0);
        assert!(
            Arc::ptr_eq(&before.blocks[0].lines, &after.blocks[0].lines),
            "unchanged question is shared, not copied"
        );
        assert!(!Arc::ptr_eq(
            &before.blocks[1].lines,
            &after.blocks[1].lines
        ));
        let (resized, _) = cache.layout(&view, 20, 3, 0);
        assert!(!Arc::ptr_eq(
            &after.blocks[0].lines,
            &resized.blocks[0].lines
        ));
    }

    #[test]
    fn exploration_groups_expand_without_hiding_failures_or_edits() {
        let mut view = View::default();
        view.user("Investigate", false);
        for (id, name, failed) in [
            ("a", "read", false),
            ("b", "grep", false),
            ("c", "read", true),
            ("d", "edit", false),
        ] {
            view.event(Out::ToolStarted {
                id: id.into(),
                name: name.into(),
                input: json!({"path":id}),
            });
            view.event(Out::ToolDone {
                id: id.into(),
                name: name.into(),
                output: json!({"ok":!failed,"content":"body","error":"denied"}),
                is_error: failed,
            });
        }
        let first_tool = view.item_id(1);
        let mut cache = TimelineCache::default();
        let (folded, headers) = cache.layout(&view, 80, 3, 0);
        assert!(folded
            .iter()
            .any(|line| line.to_string().contains("Read & search · 2 operations")));
        assert_eq!(
            headers.len(),
            3,
            "group, failure, edit all retain navigation"
        );
        view.toggle(first_tool);
        let (expanded, headers) = cache.layout(&view, 80, 3, 0);
        assert!(!expanded
            .iter()
            .any(|line| line.to_string().contains("Read & search")));
        assert_eq!(headers.len(), 4);
        view.toggle(first_tool);
        let (folded, _) = cache.layout(&view, 80, 3, 0);
        assert!(
            folded
                .iter()
                .any(|line| line.to_string().contains("Read & search")),
            "click again collapses the group"
        );
        view.select_next(false);
        let (_, headers) = cache.layout(&view, 80, 3, 0);
        assert!(
            headers.iter().any(|(_, id)| Some(*id) == view.selected()),
            "keyboard selection reveals hidden tools"
        );
    }

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
        assert!(lines
            .viewport(0..lines.len())
            .iter()
            .any(|l| l.to_string().contains("first second")));
        view.clear_timeline();
        let (lines, _) = cache.layout(&view, 80, 3, 24);
        assert_eq!(lines.len(), 0);
        assert!(cache.entries.is_empty());
    }
}
