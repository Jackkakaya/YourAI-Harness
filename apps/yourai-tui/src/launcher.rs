//! Startup session picker: a tiny alternate-screen TUI loop that runs
//! before `Harness::open`. Triggered by `--resume` with no value.
//!
//! The launcher deliberately avoids the main View/Renderer machinery so it
//! can run before the harness exists. It shares the screen guard, picker
//! geometry, list key handling, row formatting and truncation with the main
//! UI via `crate::terminal`, `crate::picker` and `crate::sessions` — do not
//! fork those pieces again here.
use crate::config::Error;
use crate::sessions::{filter_sessions, rows_from, SessionRow};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    backend::CrosstermBackend,
    prelude::*,
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};
use std::{
    io::{self},
    time::{SystemTime, UNIX_EPOCH},
};
use yourai_core::prelude::{SessionId, SessionManager};
use yourai_harness::SessionCatalog;

pub enum Choice {
    Resume(SessionId),
    New,
    Quit,
}

pub async fn pick(catalog: &SessionCatalog) -> Result<Choice, Error> {
    let metas = catalog.list_sessions().await?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut rows = rows_from(metas, None);
    // Stable secondary sort by title for ties on updated_at.
    rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.title.cmp(&b.title)));
    if rows.is_empty() {
        return Ok(Choice::New);
    }
    let _screen = crate::terminal::Screen::open()?;
    let mut query = String::new();
    let mut selected: usize = 0;
    let backend = crate::terminal::SizedBackend(CrosstermBackend::new(io::stdout()));
    let mut terminal = Terminal::new(backend)?;
    loop {
        let filtered = filter_sessions(&rows, &query);
        let picked = filtered.get(selected).copied();
        terminal.draw(|f| render(f, &rows, &filtered, &query, selected, picked, now))?;
        if !event::poll(std::time::Duration::from_millis(100))? {
            continue;
        }
        if let Event::Key(k) = event::read()? {
            if k.kind == KeyEventKind::Release {
                continue;
            }
            let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
            match k.code {
                KeyCode::Char('q') if ctrl => return Ok(Choice::Quit),
                KeyCode::Esc => return Ok(Choice::New),
                KeyCode::Enter => {
                    if let Some(idx) = picked {
                        return Ok(Choice::Resume(rows[idx].id.clone()));
                    }
                }
                _ => {
                    crate::picker::filter_input(k, &mut query, &mut selected, filtered.len());
                }
            }
        }
    }
}

fn render(
    f: &mut Frame<'_>,
    rows: &[SessionRow],
    filtered: &[usize],
    query: &str,
    selected: usize,
    picked: Option<usize>,
    now: i64,
) {
    let area = f.area();
    f.render_widget(
        Block::default().style(Style::default().bg(Color::Black).fg(Color::White)),
        area,
    );
    let rect = crate::picker::centered(area, 80, filtered.len().saturating_add(6).min(24));
    let width = rect.width;
    f.render_widget(Clear, rect);
    let mut lines: Vec<Line<'static>> = Vec::new();
    // Search line.
    let hint = if query.is_empty() {
        "type to search · ↑↓ move · Enter resume · Esc new session · Ctrl-Q quit"
    } else {
        ""
    };
    lines.push(Line::from(vec![
        Span::styled("filter ", Style::default().fg(Color::DarkGray)),
        Span::styled(query.to_owned(), Style::default().fg(Color::White)),
        Span::styled(hint.to_owned(), Style::default().fg(Color::DarkGray)),
    ]));
    lines.push(Line::default());
    if filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            "No sessions match. Esc starts a fresh session.",
            Style::default().fg(Color::Yellow),
        )));
    } else {
        let inner_w = (width.saturating_sub(4)) as usize;
        // Same column budget as the /sessions overlay: title shrinks, the
        // id/model/time triple stays fixed.
        let title_w = inner_w.saturating_sub(36).clamp(8, 32);
        for rank in
            crate::picker::visible_rows(filtered.len(), selected, rect.height.saturating_sub(4))
        {
            let idx = filtered[rank];
            let row = &rows[idx];
            let is_picked = picked == Some(idx);
            let marker = if is_picked { "►" } else { " " };
            let line = format!("{marker} {}", crate::sessions::row_body(row, title_w, now));
            let style = if rank == selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            lines.push(Line::from(Span::styled(line, style)));
        }
    }
    f.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(Color::Black).fg(Color::White))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(" Sessions · resume "),
            ),
        rect,
    );
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;

    #[test]
    fn launcher_fits_small_terminals_and_scrolls() {
        let rows: Vec<_> = (0..100)
            .map(|i| SessionRow {
                id: SessionId(format!("session-{i:03}")),
                title: format!("row-{i:03}"),
                model: String::new(),
                updated_at: 0,
                is_current: false,
            })
            .collect();
        let filtered: Vec<_> = (0..100).collect();
        for width in [1, 20, 30, 40, 80] {
            for height in [1, 2, 3, 12, 24] {
                let mut t =
                    Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
                t.draw(|f| render(f, &rows, &filtered, "", 99, Some(99), 0))
                    .unwrap();
                if width == 80 && height >= 12 {
                    let text: String = t
                        .backend()
                        .buffer()
                        .content
                        .iter()
                        .map(|c| c.symbol())
                        .collect();
                    assert!(text.contains("row-099"));
                }
            }
        }
    }

    #[test]
    fn cjk_titles_keep_column_alignment() {
        // The old launcher-local `truncate` counted chars, so CJK titles
        // (2 columns per glyph) overflowed their column. The shared row
        // body must elide by display width and keep the id column stable.
        let rows = vec![SessionRow {
            id: SessionId("d3f40178deadbeef".into()),
            title: "修复中文解析器的边界问题与回归测试".into(),
            model: String::new(),
            updated_at: 0,
            is_current: false,
        }];
        let filtered: Vec<usize> = vec![0];
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        t.draw(|f| render(f, &rows, &filtered, "", 0, Some(0), 0))
            .unwrap();
        let text: String = t
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        // 17 CJK glyphs = 34 columns; title_w is 32, so 15 glyphs (30 cols)
        // plus the ellipsis render, and the id follows in its own column.
        // Wide glyphs pad the trailing cell, so compare space-stripped text.
        let flat: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            flat.contains("修复中文解析器的边界问题与回归…d3f40178"),
            "got: {text}"
        );
    }
}
