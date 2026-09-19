mod clipboard;
mod commands;
mod editor;
mod markdown;
mod render;
mod selection;
mod state;
pub(crate) mod theme;
use crate::config::Error;
use crossterm::{
    cursor::Show,
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, prelude::*};
use render::{Metadata, Renderer};
use state::View;
use std::{
    io::{self, Stdout},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use yourai_core::prelude::*;
use yourai_harness::{Harness, SessionHost};
struct Screen(Terminal<CrosstermBackend<Stdout>>);
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
            Terminal::new(CrosstermBackend::new(io::stdout()))
        })();
        match result {
            Ok(t) => Ok(Self(t)),
            Err(e) => {
                restore();
                Err(e.into())
            }
        }
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
}
impl Drop for Screen {
    fn drop(&mut self) {
        restore();
    }
}
fn drive(
    host: Arc<SessionHost>,
    tx: mpsc::UnboundedSender<Out>,
    cancel: CancellationToken,
) -> JoinHandle<Result<(), YourAiError>> {
    tokio::spawn(async move { host.serve(TurnLimits::default(), &tx, &cancel).await })
}

async fn restore_history(h: &Harness, v: &mut View) -> Result<(), YourAiError> {
    let id = h.host.context().id;
    let mut after = 0;
    loop {
        let page = h
            .sessions
            .read_messages(
                &id,
                MessageQuery {
                    after,
                    active_only: false,
                    limit: 256,
                },
            )
            .await?;
        // Include compacted originals, but not generated summaries/API sidecars.
        v.restore(page.messages);
        match page.next {
            Some(next) if next > after => after = next,
            _ => break,
        }
    }
    let usage = h.usage.session_usage(&id).await?;
    v.recorded_responses = usage.request_count;
    v.usage = Usage {
        input_tokens: usage.total_input_tokens,
        output_tokens: usage.total_output_tokens,
        total_tokens: usage.total_tokens,
    };
    v.settle();
    v.follow();
    Ok(())
}
fn edit(e: &mut editor::Editor, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('j') if ctrl => e.insert("\n"),
        KeyCode::Enter
            if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
        {
            e.insert("\n")
        }
        KeyCode::Char('p') if ctrl => e.history(true),
        KeyCode::Char('n') if ctrl => e.history(false),
        KeyCode::Left => e.left(),
        KeyCode::Right => e.right(),
        KeyCode::Up => e.vertical(-1),
        KeyCode::Down => e.vertical(1),
        KeyCode::Home => e.home(),
        KeyCode::End => e.end(),
        KeyCode::Backspace => e.backspace(),
        KeyCode::Delete => e.delete(),
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            e.insert(&c.to_string())
        }
        KeyCode::Tab => e.insert("    "),
        _ => {}
    }
}
fn copy_selection(renderer: &Renderer, task: &mut Option<JoinHandle<std::io::Result<()>>>) {
    if let Some(text) = renderer.selection.text().filter(|text| !text.is_empty()) {
        if let Some(previous) = task.take() {
            previous.abort();
        }
        *task = Some(tokio::spawn(clipboard::copy(text)));
    }
}
pub async fn run(
    h: &Harness,
    model: &str,
    trusted_shell: bool,
    yolo: bool,
    theme: theme::Theme,
) -> Result<(), Error> {
    // Load clean persistent records before opening the alternate screen.
    let mut view = View::default();
    view.theme = theme;
    restore_history(h, &mut view).await?;
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
    let mut screen = Screen::open()?;
    let mut renderer = Renderer::default();
    let context = h.host.context();
    let meta = Metadata {
        model: model.into(),
        session: context.id.0,
        cwd: context.cwd.to_string_lossy().into(),
        trusted_shell,
        yolo,
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let mut driver = Some(drive(h.host.clone(), tx.clone(), cancel.clone()));
    let mut compact: Option<JoinHandle<()>> = None;
    let mut clipboard_task: Option<JoinHandle<std::io::Result<()>>> = None;
    let mut context_refreshed = Instant::now() - Duration::from_secs(2);
    let ui_future = async {
        let mut tick = tokio::time::interval(Duration::from_millis(40));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            for _ in 0..256 {
                match rx.try_recv() {
                    Ok(e) => view.event(e),
                    Err(_) => break,
                }
            }
            // A fast producer may finish with more than one frame of events queued.
            // Drain them before clearing stream identities and pending interactions.
            if driver.as_ref().is_some_and(|t| t.is_finished()) && rx.is_empty() {
                match driver.take().unwrap().await {
                    Ok(Err(YourAiError::Aborted(reason))) => view.notice(
                        Level::Info,
                        format!("Stopped: {reason}. Queued inputs remain; /continue resumes them."),
                    ),
                    Ok(Err(e)) => view.notice(
                        Level::Error,
                        format!("Execution failed: {e}. /continue retries pending inputs."),
                    ),
                    Err(e) => view.notice(Level::Error, format!("Driver failed: {e}")),
                    _ => {}
                }
                view.settle();
            }
            let active = !matches!(h.host.status(), SessionStatus::Idle | SessionStatus::Closed);
            if active && !view.active {
                view.active = true;
                view.since = Some(Instant::now());
            }
            if !active && rx.is_empty() && (view.active || !view.asks.is_empty()) {
                view.idle();
            }
            if compact.as_ref().is_some_and(|t| t.is_finished()) {
                if let Err(e) = compact.take().unwrap().await {
                    view.notice(Level::Error, format!("Compact failed: {e}"));
                }
                if let Ok(u) = h.usage.session_usage(&h.host.context().id).await {
                    view.usage = Usage {
                        input_tokens: u.total_input_tokens,
                        output_tokens: u.total_output_tokens,
                        total_tokens: u.total_tokens,
                    };
                }
            }
            for _ in 0..32 {
                if !event::poll(Duration::ZERO)? {
                    break;
                }
                match event::read()? {
                    Event::Resize(_, _) => {}
                    Event::Mouse(e) if !view.help => match e.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            renderer.begin_selection(e.column, e.row)
                        }
                        MouseEventKind::Drag(MouseButton::Left) => {
                            renderer.selection.drag(Position::new(e.column, e.row))
                        }
                        MouseEventKind::Up(MouseButton::Left) => {
                            renderer.selection.drag(Position::new(e.column, e.row));
                            if renderer.selection.active() {
                                copy_selection(&renderer, &mut clipboard_task);
                            } else {
                                renderer.selection.clear();
                                renderer.click(&mut view, e.column, e.row);
                            }
                        }
                        MouseEventKind::ScrollUp => {
                            renderer.selection.clear();
                            renderer.scroll(&mut view, e.column, e.row, true)
                        }
                        MouseEventKind::ScrollDown => {
                            renderer.selection.clear();
                            renderer.scroll(&mut view, e.column, e.row, false)
                        }
                        _ => {}
                    },
                    Event::Mouse(_) => {}
                    Event::Paste(text) => {
                        if let Some(a) = view.asks.front_mut() {
                            a.editor.insert(&text);
                        } else {
                            view.editor.insert(&text);
                        }
                    }
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                        let alt = key.modifiers.contains(KeyModifiers::ALT);
                        if key.code == KeyCode::Char('q') && ctrl {
                            return Ok(());
                        }
                        if matches!(key.code, KeyCode::Char('c' | 'C'))
                            && (alt || (ctrl && key.modifiers.contains(KeyModifiers::SHIFT)))
                        {
                            copy_selection(&renderer, &mut clipboard_task);
                            continue;
                        }
                        if key.code == KeyCode::Esc && renderer.selection.active() {
                            renderer.selection.clear();
                            continue;
                        }
                        renderer.selection.clear();
                        if view.help {
                            if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::F(1)) {
                                view.help = false;
                            }
                            continue;
                        }
                        view.commands.sync(&view.editor.text, view.asks.is_empty());
                        let commands = view.commands.items();
                        if !commands.is_empty() && !ctrl && !alt {
                            match key.code {
                                KeyCode::Up => {
                                    view.commands.step(true);
                                    continue;
                                }
                                KeyCode::Down => {
                                    view.commands.step(false);
                                    continue;
                                }
                                KeyCode::Esc => {
                                    view.commands.dismiss();
                                    continue;
                                }
                                KeyCode::Tab | KeyCode::Enter
                                    if !key.modifiers.contains(KeyModifiers::SHIFT) =>
                                {
                                    let command =
                                        commands[view.commands.selected.min(commands.len() - 1)];
                                    view.editor.take();
                                    view.editor.insert(command.text);
                                    if command.argument {
                                        view.editor.insert(" ");
                                    }
                                    if key.code == KeyCode::Tab || command.argument {
                                        continue;
                                    }
                                    view.commands.dismiss();
                                }
                                _ => {}
                            }
                        }
                        match key.code {
                            KeyCode::F(1) => view.help = true,
                            KeyCode::F(6) => {
                                view.select_next(key.modifiers.contains(KeyModifiers::SHIFT));
                                renderer.reveal(view.selected);
                            }
                            KeyCode::Char('t') if ctrl => {
                                view.todos_expanded = !view.todos_expanded;
                            }
                            KeyCode::Char('o') if ctrl => {
                                view.toggle_recent(false);
                                renderer.reveal(view.selected);
                            }
                            KeyCode::Char('r') if ctrl => {
                                view.toggle_recent(true);
                                renderer.reveal(view.selected);
                            }
                            KeyCode::Char('b') if ctrl => view.sidebar = !view.sidebar,
                            KeyCode::Char('y') if ctrl => view.theme = view.theme.next(),
                            KeyCode::End if ctrl => renderer.follow(&mut view),
                            KeyCode::PageUp if alt && !view.asks.is_empty() => {
                                view.asks.front_mut().unwrap().scroll =
                                    view.asks.front().unwrap().scroll.saturating_sub(5)
                            }
                            KeyCode::PageDown if alt && !view.asks.is_empty() => {
                                view.asks.front_mut().unwrap().scroll =
                                    view.asks.front().unwrap().scroll.saturating_add(5)
                            }
                            KeyCode::PageUp if alt => {
                                view.todo_scroll = view.todo_scroll.saturating_sub(5)
                            }
                            KeyCode::PageDown if alt => {
                                view.todo_scroll = view.todo_scroll.saturating_add(5)
                            }
                            KeyCode::PageUp => view.scroll = view.scroll.saturating_add(10),
                            KeyCode::PageDown => {
                                view.scroll = view.scroll.saturating_sub(10);
                                if view.scroll == 0 {
                                    renderer.follow(&mut view);
                                }
                            }
                            KeyCode::Esc | KeyCode::Char('c')
                                if key.code == KeyCode::Esc || ctrl =>
                            {
                                h.host.interrupt();
                                view.asks.clear();
                                view.notice(Level::Info, "Cancellation requested.");
                            }
                            KeyCode::Enter
                                if !key
                                    .modifiers
                                    .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
                            {
                                if let Some(ask) = view.asks.front_mut() {
                                    match ask.answer() {
                                        Ok(payload) => match h.host.submit(In::Reply {
                                            id: ask.id.clone(),
                                            payload,
                                        }) {
                                            Ok(()) => {
                                                view.asks.pop_front();
                                                view.notice(Level::Info, "Reply sent.");
                                            }
                                            Err(e) => {
                                                view.asks.pop_front();
                                                view.notice(
                                                    Level::Warning,
                                                    format!("Reply was not accepted: {e}"),
                                                );
                                            }
                                        },
                                        Err(e) => ask.error = Some(e),
                                    }
                                    continue;
                                }
                                let text = view.editor.text.clone();
                                if text.trim().is_empty() {
                                    continue;
                                }
                                if text.trim() == "/theme" || text.trim().starts_with("/theme ") {
                                    let name = text.trim().strip_prefix("/theme").unwrap().trim();
                                    if name.is_empty() {
                                        view.theme = view.theme.next();
                                    } else if let Some(theme) = theme::Theme::parse(name) {
                                        view.theme = theme;
                                    } else {
                                        view.notice(
                                            Level::Warning,
                                            "Themes: dark, light, nord, dracula. /theme NAME",
                                        );
                                    }
                                    view.editor.take();
                                    continue;
                                }
                                match text.trim() {
                                    "/quit" => return Ok(()),
                                    "/help" => {
                                        view.help = true;
                                        view.editor.take();
                                        continue;
                                    }
                                    "/clear" => {
                                        view.clear_timeline();
                                        view.editor.take();
                                        view.notice(
                                            Level::Info,
                                            "Screen cleared; saved history unchanged.",
                                        );
                                        continue;
                                    }
                                    "/continue" => {
                                        if driver.is_none() {
                                            driver = Some(drive(
                                                h.host.clone(),
                                                tx.clone(),
                                                cancel.clone(),
                                            ));
                                        }
                                        view.editor.take();
                                        continue;
                                    }
                                    "/compact" => {
                                        if compact.is_some() || view.active {
                                            view.notice(
                                                Level::Warning,
                                                "Compact requires an idle session.",
                                            );
                                            continue;
                                        }
                                        view.editor.take();
                                        let host = h.host.clone();
                                        let tx = tx.clone();
                                        let token = cancel.clone();
                                        compact = Some(tokio::spawn(async move {
                                            let result = host
                                                .compact(
                                                    CompactionRequest::new(
                                                        CompactionTrigger::Manual,
                                                    ),
                                                    &token,
                                                )
                                                .await;
                                            let(level,message)=match result{Ok(r)=>(Level::Info,format!("Context {:?}: {} -> {} estimated tokens. {}",r.action,r.tokens_before,r.tokens_after,r.reason)),Err(e)=>(Level::Error,format!("Compact failed: {e}"))};
                                            let _ = tx.send(Out::Notice { level, message });
                                        }));
                                        continue;
                                    }
                                    _ => {}
                                }
                                if text.starts_with('/') && !text.starts_with("/queue ") {
                                    view.notice(
                                        Level::Warning,
                                        "Unknown command. /help lists commands.",
                                    );
                                    continue;
                                }
                                let queued = text.starts_with("/queue ");
                                let body = text.strip_prefix("/queue ").unwrap_or(&text);
                                if body.trim().is_empty() {
                                    continue;
                                }
                                let input = if queued {
                                    In::follow_up(body)
                                } else {
                                    In::user_text(body)
                                };
                                match h.host.submit(input) {
                                    Ok(()) => {
                                        view.editor.remember(&text);
                                        view.editor.take();
                                        view.user(body, queued);
                                        renderer.follow(&mut view);
                                        if driver.is_none() {
                                            driver = Some(drive(
                                                h.host.clone(),
                                                tx.clone(),
                                                cancel.clone(),
                                            ));
                                        }
                                    }
                                    Err(e) => view.notice(Level::Error, e.to_string()),
                                }
                            }
                            _ => {
                                if let Some(ask) = view.asks.front_mut() {
                                    edit(&mut ask.editor, key);
                                } else {
                                    edit(&mut view.editor, key);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            if let Some(tasks) = &h.tasks {
                view.set_todos(
                    tasks
                        .list()
                        .into_iter()
                        .map(|t| state::Todo {
                            id: t.id,
                            text: state::bounded(&t.subject),
                            completed: t.completed,
                        })
                        .collect(),
                );
            }
            if clipboard_task.as_ref().is_some_and(|t| t.is_finished()) {
                let message = match clipboard_task.take().unwrap().await {
                    Ok(Ok(())) => "✓ 已复制".into(),
                    Ok(Err(e)) => format!("Copy failed: {e}"),
                    Err(e) => format!("Copy failed: {e}"),
                };
                view.toast = Some((message, Instant::now()));
            }
            view.model_metrics = h.budget.snapshot();
            if view.sidebar && context_refreshed.elapsed() >= Duration::from_secs(1) {
                view.context_usage = h.host.context_usage().ok();
                if let Ok(usage) = h.usage.session_usage(&h.host.context().id).await {
                    view.recorded_responses = usage.request_count;
                }
                context_refreshed = Instant::now();
            }
            screen.0.draw(|f| {
                renderer.draw(
                    f,
                    &mut view,
                    &meta,
                    &h.host.status(),
                    h.host.queued(),
                    compact.is_some(),
                )
            })?;
        }
    };
    let result = ui_future.await;
    if let Some(task) = clipboard_task {
        task.abort();
    }
    cancel.cancel();
    h.host.interrupt();
    if let Some(task) = compact {
        let _ = task.await;
    }
    if let Some(task) = driver {
        let _ = task.await;
    }
    result
}
