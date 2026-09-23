//! Per-item layout cache. Content versions are owned by View, animation by Renderer.
use super::*;
use std::{collections::HashMap, ops::Range, sync::Arc};
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
struct ResultEntry {
    width: usize,
    versions: Vec<(u64, u64)>,
    lines: Arc<Vec<Line<'static>>>,
    links: Vec<(usize, u64)>,
}
/// Indexed immutable blocks: revisions share history, scrolling clones only visible rows.
#[derive(Default)]
pub(super) struct LayoutLines {
    blocks: Vec<(usize, Arc<Vec<Line<'static>>>)>,
    len: usize,
}
impl LayoutLines {
    fn push(&mut self, lines: Arc<Vec<Line<'static>>>) {
        let start = self.len;
        self.len += lines.len();
        self.blocks.push((start, lines));
    }
    #[cfg(test)]
    pub fn iter(&self) -> impl Iterator<Item = &Line<'static>> {
        self.blocks.iter().flat_map(|(_, lines)| lines.iter())
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn viewport(&self, range: Range<usize>) -> Vec<Line<'static>> {
        let mut visible = Vec::with_capacity(range.end.saturating_sub(range.start));
        let first = self
            .blocks
            .partition_point(|(start, lines)| start + lines.len() <= range.start);
        for (start, lines) in &self.blocks[first..] {
            if *start >= range.end {
                break;
            }
            visible.extend(
                lines[range.start.saturating_sub(*start)
                    ..range.end.saturating_sub(*start).min(lines.len())]
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
    pub result_links: Vec<(usize, u64)>,
    pub result_starts: Vec<(usize, u64)>,
    results: HashMap<u64, ResultEntry>,
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
        self.result_links.clear();
        self.result_starts.clear();
        let mut lines = LayoutLines::default();
        let mut headers = Vec::new();
        let first = v.item_id(0);
        self.entries
            .retain(|id, _| *id >= first && *id < first + v.items().len() as u64);
        self.results
            .retain(|id, _| *id >= first && *id < first + v.items().len() as u64);
        let mut results = results::Results::default();
        let mut collapsed_until = 0;
        let mut expanded_until = 0;
        for (index, item) in v.items().iter().enumerate() {
            if matches!(
                item,
                Item::Text {
                    role: Role::User,
                    ..
                }
            ) {
                self.append_results(&results, &mut lines, width);
                results = results::Results::default();
            }
            results.observe(v.item_id(index), v.item_version(index), item);
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
                    lines.push(Arc::new(vec![
                        Line::from(Span::styled(
                            elide(
                                &format!(
                                    "  ▸ Explored · {} read/search operations · click to expand",
                                    end - index
                                ),
                                width,
                            ),
                            Style::default().fg(MUTED),
                        )),
                        Line::default(),
                    ]));
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
            lines.push(self.entries[&id].lines.clone());
        }
        if !v.active && v.asks_empty() {
            self.append_results(&results, &mut lines, width);
        }
        (lines, headers)
    }
    fn append_results(
        &mut self,
        results: &results::Results<'_>,
        lines: &mut LayoutLines,
        width: usize,
    ) {
        let Some(&(id, _)) = results.versions.first() else {
            return;
        };
        if self
            .results
            .get(&id)
            .is_none_or(|entry| entry.width != width || entry.versions != results.versions)
        {
            let (rows, links) = results.render(width);
            self.results.insert(
                id,
                ResultEntry {
                    width,
                    versions: results.versions.clone(),
                    lines: Arc::new(rows),
                    links,
                },
            );
        }
        let entry = &self.results[&id];
        self.result_starts.push((lines.len(), id));
        self.result_links.extend(
            entry
                .links
                .iter()
                .map(|&(line, id)| (line + lines.len(), id)),
        );
        lines.push(entry.lines.clone());
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
                for row in wrap(
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
            Arc::ptr_eq(&before.blocks[0].1, &after.blocks[0].1),
            "unchanged question is shared, not copied"
        );
        assert!(!Arc::ptr_eq(&before.blocks[1].1, &after.blocks[1].1));
        let (resized, _) = cache.layout(&view, 20, 3, 0);
        assert!(!Arc::ptr_eq(&after.blocks[0].1, &resized.blocks[0].1));
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
            .any(|line| line.to_string().contains("2 read/search")));
        assert_eq!(
            headers.len(),
            3,
            "group, failure, edit all retain navigation"
        );
        view.toggle(first_tool);
        let (expanded, headers) = cache.layout(&view, 80, 3, 0);
        assert!(!expanded
            .iter()
            .any(|line| line.to_string().contains("Explored")));
        assert_eq!(headers.len(), 4);
        view.toggle(first_tool);
        let (folded, _) = cache.layout(&view, 80, 3, 0);
        assert!(
            folded
                .iter()
                .any(|line| line.to_string().contains("Explored")),
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
