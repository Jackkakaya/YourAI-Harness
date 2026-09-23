//! Startup session picker: a tiny alternate-screen TUI loop that runs
//! before `Harness::open`. Triggered by `--resume` with no value.
//!
//! The launcher deliberately avoids the main View/Renderer machinery so it
//! can run before the harness exists and stay ~150 lines. It shares the
//! pure filtering logic with the `/sessions` overlay via `crate::sessions`.
use crate::config::Error;
use crate::sessions::{filter_sessions, relative_time, rows_from, SessionRow};
use crossterm::{
    cursor::Show,
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    prelude::*,
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};
use std::{
    io::{self, Write},
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
    let _screen = Screen::open()?;
    let mut query = String::new();
    let mut selected: usize = 0;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    loop {
        let filtered = filter_sessions(&rows, &query);
        let picked = filtered.get(selected).copied();
        terminal.draw(|f| render(f, &rows, &filtered, &query, selected, picked, now))?;
        if !event::poll(std::time::Duration::from_millis(100))? {
            continue;
        }
        match event::read()? {
            Event::Key(k) if k.kind != KeyEventKind::Release => {
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                match k.code {
                    KeyCode::Char('q') if ctrl => return Ok(Choice::Quit),
                    KeyCode::Esc => return Ok(Choice::New),
                    KeyCode::Enter => {
                        if let Some(idx) = picked {
                            return Ok(Choice::Resume(rows[idx].id.clone()));
                        }
                    }
                    KeyCode::Up => {
                        if !filtered.is_empty() {
                            selected = selected.saturating_sub(1);
                        }
                    }
                    KeyCode::Down => {
                        if !filtered.is_empty() {
                            selected = (selected + 1).min(filtered.len() - 1);
                        }
                    }
                    KeyCode::Char('p') if ctrl => {
                        if !filtered.is_empty() {
                            selected = selected.saturating_sub(1);
                        }
                    }
                    KeyCode::Char('n') if ctrl => {
                        if !filtered.is_empty() {
                            selected = (selected + 1).min(filtered.len() - 1);
                        }
                    }
                    KeyCode::Backspace => {
                        query.pop();
                        selected = 0;
                    }
                    KeyCode::Char(c)
                        if !k
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        query.push(c);
                        selected = 0;
                    }
                    _ => {}
                }
            }
            Event::Resize(_, _) => {}
            _ => {}
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
        for rank in
            crate::picker::visible_rows(filtered.len(), selected, rect.height.saturating_sub(4))
        {
            let idx = filtered[rank];
            let row = &rows[idx];
            let is_picked = picked == Some(idx);
            let marker = if is_picked { "►" } else { " " };
            let title = truncate(&row.title, inner_w.saturating_sub(40));
            let id8 = row.id.0.chars().take(8).collect::<String>();
            let model = if row.model.is_empty() {
                "—".into()
            } else {
                truncate(&row.model, 16)
            };
            let time = relative_time(row.updated_at, now);
            let line = format!("{marker} {title:<28} {id8} · {model:<16} · {time}");
            let color = if rank == selected {
                Color::Cyan
            } else if row.is_current {
                Color::LightGreen
            } else {
                Color::White
            };
            let style = if rank == selected {
                Style::default().fg(color).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(color)
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

fn truncate(s: &str, width: usize) -> String {
    let total = s.chars().count();
    if total <= width {
        return s.to_string();
    }
    let take = width.saturating_sub(1);
    let mut out: String = s.chars().take(take).collect();
    out.push('…');
    out
}

struct Screen;
impl Screen {
    fn open() -> Result<Self, Error> {
        enable_raw_mode()?;
        let result = (|| {
            execute!(
                io::stdout(),
                EnterAlternateScreen,
                EnableBracketedPaste,
                EnableMouseCapture
            )?;
            Ok::<(), io::Error>(())
        })();
        if let Err(e) = result {
            restore();
            return Err(e.into());
        }
        Ok(Self)
    }
}
fn restore() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableMouseCapture,
        Show
    );
    let _ = io::stdout().write_all(b"\x1b[?2026l");
    let _ = io::stdout().flush();
}
impl Drop for Screen {
    fn drop(&mut self) {
        restore();
    }
}

#[cfg(test)]
mod tests {
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
}
