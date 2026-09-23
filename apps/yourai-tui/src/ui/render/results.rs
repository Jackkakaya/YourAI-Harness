//! A navigable record of tool evidence, never a model-generated completion claim.
use super::*;
use crate::ui::state::ToolView;

#[derive(Default)]
pub(super) struct Results<'a> {
    tools: Vec<(u64, &'a ToolView)>,
    pub versions: Vec<(u64, u64)>,
}
impl<'a> Results<'a> {
    pub fn observe(&mut self, id: u64, version: u64, item: &'a Item) {
        if let Item::Tool(tool) = item {
            if matches!(tool.name.as_str(), "edit" | "write" | "shell") || failed(tool) {
                self.tools.push((id, tool));
                self.versions.push((id, version));
            }
        }
    }

    pub fn render(&self, width: usize) -> (Vec<Line<'static>>, Vec<(usize, u64)>) {
        if self.tools.is_empty() {
            return (vec![], vec![]);
        }
        let mut lines = vec![Line::from(Span::styled(
            "  Recorded actions",
            Style::default().fg(TEXT).bold(),
        ))];
        let mut links = vec![];
        let failures: Vec<_> = self
            .tools
            .iter()
            .copied()
            .filter(|(_, t)| failed(t) || t.status == ToolStatus::Interrupted)
            .collect();
        let files: Vec<_> = self
            .tools
            .iter()
            .copied()
            .filter(|(_, t)| {
                matches!(t.name.as_str(), "edit" | "write")
                    && !failed(t)
                    && t.status != ToolStatus::Interrupted
            })
            .collect();
        let commands: Vec<_> = self
            .tools
            .iter()
            .copied()
            .filter(|(_, t)| t.name == "shell" && !failed(t) && t.status != ToolStatus::Interrupted)
            .collect();
        for (title, items, color) in [
            ("Failed / stopped", failures, RED),
            ("File operations", files, TEXT),
            ("Commands", commands, TEXT),
        ] {
            if items.is_empty() {
                continue;
            }
            lines.extend(wrap(
                &format!("{title} · {}", items.len()),
                Style::default().fg(color).bold(),
                width,
                "  ",
            ));
            for &(id, tool) in items.iter().take(3) {
                let status = match tool.status {
                    ToolStatus::Running => "running".to_owned(),
                    ToolStatus::Interrupted => "stopped".to_owned(),
                    ToolStatus::Failed => "failed".to_owned(),
                    ToolStatus::Done if tool.name == "shell" => tool
                        .exit_code
                        .map(|c| format!("exit {c}"))
                        .unwrap_or_else(|| "exit unknown".into()),
                    ToolStatus::Done => "done".into(),
                };
                let subject = tool
                    .summary
                    .lines()
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&tool.name);
                let prefix = format!("    {status} · ");
                let budget = width.saturating_sub(prefix.width());
                let subject = if matches!(tool.name.as_str(), "write" | "edit") {
                    elide_tail(subject, budget)
                } else {
                    elide(subject, budget)
                };
                links.push((lines.len(), id));
                lines.push(Line::from(vec![
                    Span::styled(
                        prefix,
                        Style::default().fg(if failed(tool) { RED } else { MUTED }),
                    ),
                    Span::styled(subject, Style::default().fg(color)),
                ]));
            }
            if items.len() > 3 {
                links.push((lines.len(), items[3].0));
                lines.extend(wrap(
                    &format!("… {} more · click to inspect", items.len() - 3),
                    Style::default().fg(MUTED),
                    width,
                    "    ",
                ));
            }
        }
        lines.extend(wrap(
            "Click a row to open its diff or log",
            Style::default().fg(MUTED),
            width,
            "  ",
        ));
        lines.push(Line::default());
        (lines, links)
    }
}
fn failed(tool: &ToolView) -> bool {
    tool.status == ToolStatus::Failed || tool.exit_code.is_some_and(|code| code != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn evidence_preserves_unknown_exits_failures_and_long_paths() {
        let mut view = View::default();
        for (id, name, subject, output) in [
            (
                "a",
                "write",
                "src/很长的文件路径/implementation.rs",
                json!({"ok":true}),
            ),
            (
                "b",
                "shell",
                "cargo test",
                json!({"ok":true,"stdout":"partial"}),
            ),
            (
                "c",
                "shell",
                "cargo check",
                json!({"ok":false,"exit_code":1,"stderr":"compile failed"}),
            ),
        ] {
            view.event(Out::ToolStarted {
                id: id.into(),
                name: name.into(),
                input: json!({"path":subject,"command":subject}),
            });
            view.event(Out::ToolDone {
                id: id.into(),
                name: name.into(),
                is_error: output["ok"] == false,
                output,
            });
        }
        let mut results = Results::default();
        for (i, item) in view.items().iter().enumerate() {
            results.observe(view.item_id(i), view.item_version(i), item);
        }
        for width in [28, 40, 80] {
            let (lines, links) = results.render(width);
            let text = lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("Failed / stopped"));
            assert!(text.contains("exit unknown"));
            assert!(!text.contains("tests passed"));
            assert!(lines.iter().all(|line| line.width() <= width), "{text}");
            assert_eq!(links.len(), 3);
        }
    }
}
