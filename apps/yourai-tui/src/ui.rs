mod clipboard;
mod commands;
mod editor;
mod markdown;
mod render;
mod selection;
mod state;
pub(crate) mod syntax;
pub(crate) mod theme;
use crate::config::{Config, Error};
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
use state::{derive_title, Item, Role, View};
use std::{
    io::{self, Stdout, Write},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use yourai_core::prelude::*;
use yourai_harness::{Harness, HarnessConfig, SessionHost};

/// Buffers each frame and wraps it in synchronized-output markers so the
/// terminal paints the whole frame atomically instead of ~1KB LineWriter
/// chunks (which tear while streaming). Unknown private modes are ignored
/// by terminals without synchronized-output support.
struct SyncWriter {
    inner: Stdout,
    buffer: Vec<u8>,
}
impl Write for SyncWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return self.inner.flush();
        }
        let mut frame = Vec::with_capacity(self.buffer.len() + 16);
        frame.extend_from_slice(b"\x1b[?2026h");
        frame.append(&mut self.buffer);
        frame.extend_from_slice(b"\x1b[?2026l");
        self.inner.write_all(&frame)?;
        self.inner.flush()
    }
}

struct Screen(Terminal<CrosstermBackend<SyncWriter>>);
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
            Terminal::new(CrosstermBackend::new(SyncWriter {
                inner: io::stdout(),
                buffer: Vec::with_capacity(1 << 16),
            }))
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
    // Ensure no synchronized output is still open after a crash/exit.
    let _ = io::stdout().write_all(b"\x1b[?2026l");
    let _ = io::stdout().flush();
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
    limits: TurnLimits,
) -> JoinHandle<Result<(), YourAiError>> {
    tokio::spawn(async move { host.serve(limits, &tx, &cancel).await })
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
    // Restore the session title; backfill older sessions from their first prompt.
    if let Ok(mut meta) = h.sessions.load_session(&id).await {
        v.title = meta
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned);
        if v.title.is_none() {
            let first_user = v.items().iter().find_map(|i| match i {
                Item::Text {
                    role: Role::User,
                    text,
                } => Some(text.as_str()),
                _ => None,
            });
            if let Some(title) = first_user.and_then(derive_title) {
                meta.title = Some(title.clone());
                v.title = Some(title);
                if let Err(e) = h.sessions.save_session(&meta).await {
                    v.title = None;
                    v.notice(Level::Warning, format!("Session title not saved: {e}"));
                }
            }
        }
    }
    v.settle();
    v.follow();
    Ok(())
}
fn edit(e: &mut editor::Editor, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        // Newline insertion.
        KeyCode::Char('j') if ctrl => e.insert("\n"),
        KeyCode::Enter
            if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
        {
            e.insert("\n")
        }
        // History navigation: Ctrl-P / Ctrl-N (readline) and Up / Down when
        // the cursor sits on the first / last logical line (shell behaviour).
        // In the middle of a multiline buffer, Up / Down move across rows.
        KeyCode::Char('p') if ctrl => e.history(true),
        KeyCode::Char('n') if ctrl => e.history(false),
        KeyCode::Up => {
            if e.on_first_line() {
                e.history(true);
            } else {
                e.vertical(-1);
            }
        }
        KeyCode::Down => {
            if e.on_last_line() {
                e.history(false);
            } else {
                e.vertical(1);
            }
        }
        // Word-level movement (Alt-B/F, Ctrl-Left/Right).
        KeyCode::Left if ctrl || alt => e.word_left(),
        KeyCode::Right if ctrl || alt => e.word_right(),
        KeyCode::Char('b') if alt => e.word_left(),
        KeyCode::Char('f') if alt => e.word_right(),
        // Character movement (readline Ctrl-B / Ctrl-F).
        KeyCode::Char('b') if ctrl => e.left(),
        KeyCode::Char('f') if ctrl => e.right(),
        // Line movement (readline Ctrl-A / Ctrl-E).
        KeyCode::Char('a') if ctrl => e.home(),
        KeyCode::Char('e') if ctrl => e.end(),
        // Deletion: Ctrl-H = backspace, Ctrl-D = forward delete, Ctrl-W =
        // unix-word-rubout (whitespace-delimited), Ctrl-U / Ctrl-K kill to
        // line start / end, Alt-D / Ctrl-Delete kill a word forward,
        // Alt-Backspace / Ctrl-Backspace kill a word backward.
        KeyCode::Char('h') if ctrl => e.backspace(),
        KeyCode::Char('d') if ctrl => e.delete(),
        KeyCode::Char('w') if ctrl => e.unix_word_rubout(),
        KeyCode::Char('u') if ctrl => e.kill_to_start(),
        KeyCode::Char('k') if ctrl => e.kill_to_end(),
        KeyCode::Char('d') if alt => e.delete_word_fwd(),
        KeyCode::Backspace if ctrl || alt => e.delete_word_back(),
        KeyCode::Delete if ctrl => e.delete_word_fwd(),
        // Plain movement / editing.
        KeyCode::Left => e.left(),
        KeyCode::Right => e.right(),
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

/// Everything a frame depends on. Redrawing only when this changes keeps the
/// physical cursor untouched while idle: ratatui re-emits show/move-cursor on
/// every `draw`, and terminals restart the cursor blink on each one, so a
/// fixed-FPS loop makes the block cursor strobed even on a static screen.
#[derive(PartialEq)]
struct FrameSnap {
    revision: u64,
    editor_text: String,
    editor_cursor: usize,
    ask: Option<(String, usize, bool, String, usize)>,
    status: std::mem::Discriminant<SessionStatus>,
    queued: usize,
    active: bool,
    compacting: bool,
    help: bool,
    stats: bool,
    model_picker: Option<usize>,
    session_picker: Option<(String, usize)>,
    theme_picker: Option<usize>,
    theme: theme::Theme,
    todos: Vec<state::Todo>,
    todo_panel: bool,
    todo_scroll: usize,
    toast: Option<String>,
    // Busy-phase animation (pulse color, elapsed seconds, retry countdown):
    // quantize to 100ms so idle turns redraw at ~10fps instead of every tick.
    time_quantum: Option<u64>,
}
impl FrameSnap {
    fn capture(v: &View, status: &SessionStatus, queued: usize, compacting: bool) -> Self {
        let busy = v.active || compacting || !v.asks_empty();
        Self {
            revision: v.revision,
            editor_text: v.editor.text.clone(),
            editor_cursor: v.editor.cursor,
            ask: v.ask().map(|a| {
                (
                    a.id.clone(),
                    a.scroll,
                    a.error.is_some(),
                    a.editor.text.clone(),
                    a.editor.cursor,
                )
            }),
            status: std::mem::discriminant(status),
            queued,
            active: v.active,
            compacting,
            help: v.help,
            stats: v.stats,
            model_picker: v.model_picker,
            session_picker: v
                .session_picker
                .as_ref()
                .map(|s| (s.query.clone(), s.selected)),
            theme_picker: v.theme_picker,
            theme: v.theme,
            todos: v.todos.clone(),
            todo_panel: v.todo_panel,
            todo_scroll: v.todo_scroll,
            toast: v
                .toast
                .as_ref()
                .filter(|(_, since)| since.elapsed().as_secs() < 3)
                .map(|(message, _)| message.clone()),
            time_quantum: busy.then(|| {
                // Wall-clock anchored so compacting-without-a-turn still animates.
                // With the pulse running, repaint at 10fps; while an approval is
                // open only the seconds counter moves, so 1fps is enough.
                let step_ms: u128 = if v.asks_empty() { 100 } else { 1000 };
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| (d.as_millis() / step_ms) as u64)
                    .unwrap_or(0)
            }),
        }
    }
}
/// Run the interactive client with its assembled runtime and UI configuration.
/// These values stay explicit at the binary boundary to avoid another mirrored config type.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    harness: Harness,
    model: &str,
    trusted_shell: bool,
    yolo: bool,
    theme: theme::Theme,
    limits: TurnLimits,
    config: Arc<std::sync::Mutex<Config>>,
    choices: Vec<crate::models::ModelChoice>,
    config_template: HarnessConfig,
) -> Result<(String, Vec<In>), Error> {
    // Load clean persistent records before opening the alternate screen.
    let mut view = View::default();
    view.theme = theme;
    view.model_label = model.into();
    view.model_choices = choices.iter().map(|c| c.label.clone()).collect();
    view.pricing = {
        let cfg = config.lock().map_err(|e| Error::from(e.to_string()))?;
        let p = cfg.pricing();
        match (p.input, p.output) {
            (Some(i), Some(o)) => Some((i, o)),
            _ => None,
        }
    };
    restore_history(&harness, &mut view).await?;
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
    let mut screen = Screen::open()?;
    let mut renderer = Renderer::default();
    let context = harness.host.context();
    let mut meta = Metadata {
        session: context.id.0,
        cwd: context.cwd.to_string_lossy().into(),
        trusted_shell,
        yolo,
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut cancel = CancellationToken::new();
    let mut harness_opt: Option<Harness> = Some(harness);
    let mut driver = Some({
        let h = harness_opt.as_ref().unwrap();
        drive(h.host.clone(), tx.clone(), cancel.clone(), limits.clone())
    });
    let mut compact: Option<JoinHandle<()>> = None;
    let mut clipboard_task: Option<JoinHandle<std::io::Result<()>>> = None;
    let mut context_refreshed = Instant::now() - Duration::from_secs(2);
    let mut pending_switch: Option<SessionId> = None;
    let ui_future = async {
        let mut tick = tokio::time::interval(Duration::from_millis(40));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_snap: Option<FrameSnap> = None;
        loop {
            tick.tick().await;
            // Prepare and restore the target before shutting down the current
            // session, so failure leaves the current driver and view usable.
            if let Some(new_id) = pending_switch.take() {
                // Prepare the target before stopping the old session. A failed
                // resolve/open leaves the current session and its UI intact.
                let prepared = prepare_session(&config, &config_template, new_id);
                let new_h = match prepared {
                    Ok((hc, model)) => Harness::open(hc, model).await.map_err(Error::from),
                    Err(e) => Err(e),
                };
                let new_h = match new_h {
                    Ok(h) => h,
                    Err(e) => {
                        view.notice(Level::Error, format!("Could not open session: {e}"));
                        continue;
                    }
                };
                let mut fresh = View::default();
                fresh.theme = view.theme;
                fresh.model_label = view.model_label.clone();
                fresh.model_choices = view.model_choices.clone();
                fresh.pricing = view.pricing;
                if let Err(e) = restore_history(&new_h, &mut fresh).await {
                    let _ = new_h.close().await;
                    view.notice(Level::Error, format!("Could not restore history: {e}"));
                    continue;
                }
                cancel.cancel();
                if let Some(h_old) = harness_opt.as_ref() {
                    h_old.host.interrupt();
                }
                if let Some(task) = driver.take() {
                    let _ = task.await;
                }
                if let Some(task) = compact.take() {
                    let _ = task.await;
                }
                while rx.try_recv().is_ok() {}
                if let Some(old) = harness_opt.take() {
                    match old.close().await {
                        Ok(discarded) if !discarded.is_empty() => fresh.notice(
                            Level::Warning,
                            format!(
                                "{} queued inputs from previous session discarded.",
                                discarded.len()
                            ),
                        ),
                        Err(e) => fresh.notice(
                            Level::Error,
                            format!("Could not close previous session: {e}"),
                        ),
                        _ => {}
                    }
                }
                view = fresh;
                let ctx = new_h.host.context();
                meta.session = ctx.id.0;
                meta.cwd = ctx.cwd.to_string_lossy().into();
                cancel = CancellationToken::new();
                driver = Some(drive(
                    new_h.host.clone(),
                    tx.clone(),
                    cancel.clone(),
                    limits.clone(),
                ));
                context_refreshed = Instant::now() - Duration::from_secs(2);
                harness_opt = Some(new_h);
                renderer = Renderer::default();
                last_snap = None;
                view.notice(Level::Info, "Session switched.");
            }
            let h = harness_opt.as_ref().expect("harness missing");
            let mut drained = 0u32;
            for _ in 0..256 {
                match rx.try_recv() {
                    Ok(e) => {
                        drained += 1;
                        view.event(e);
                    }
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
            if !active && rx.is_empty() && (view.active || !view.asks_empty()) {
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
            let mut inputs = 0u32;
            for _ in 0..32 {
                if !event::poll(Duration::ZERO)? {
                    break;
                }
                let received = event::read()?;
                // Hovering and key releases leave the frame unchanged.
                if !matches!(&received, Event::Mouse(m) if m.kind == MouseEventKind::Moved)
                    && !matches!(&received, Event::Key(k) if k.kind == KeyEventKind::Release)
                {
                    inputs += 1;
                }
                match received {
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
                        if let Some(a) = view.ask_mut() {
                            a.editor.insert(&text);
                        } else {
                            view.editor.insert(&text);
                        }
                    }
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                        let alt = key.modifiers.contains(KeyModifiers::ALT);
                        if key.code == KeyCode::Char('q') && ctrl {
                            return Ok::<(), Error>(());
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
                        // Model picker keyboard routing (before command menu).
                        if let Some(idx) = view.model_picker {
                            match key.code {
                                KeyCode::Esc => {
                                    view.model_picker = None;
                                    continue;
                                }
                                KeyCode::Up => {
                                    view.model_picker = Some(idx.saturating_sub(1));
                                    continue;
                                }
                                KeyCode::Down => {
                                    view.model_picker =
                                        Some((idx + 1).min(choices.len().saturating_sub(1)));
                                    continue;
                                }
                                KeyCode::Enter => {
                                    let choice = &choices[idx.min(choices.len() - 1)];
                                    match switch_model(
                                        &config,
                                        h,
                                        &mut view,
                                        &choice.id,
                                        choice.variant.clone(),
                                    )
                                    .await
                                    {
                                        Ok(label) => {
                                            view.notice(
                                                Level::Info,
                                                format!("Model switched to {label}"),
                                            );
                                        }
                                        Err(e) => {
                                            view.notice(
                                                Level::Error,
                                                format!("Model switch failed: {e}"),
                                            );
                                        }
                                    }
                                    view.model_picker = None;
                                    continue;
                                }
                                _ => continue,
                            }
                        }
                        // Session picker keyboard routing (after models picker, before commands).
                        let mut session_delete: Option<SessionId> = None;
                        if let Some(picker) = view.session_picker.as_mut() {
                            let filtered =
                                crate::sessions::filter_sessions(&picker.rows, &picker.query);
                            match key.code {
                                KeyCode::Esc => {
                                    view.session_picker = None;
                                    continue;
                                }
                                KeyCode::Up => {
                                    picker.selected = picker.selected.saturating_sub(1);
                                    continue;
                                }
                                KeyCode::Down => {
                                    let max = filtered.len().saturating_sub(1);
                                    picker.selected = (picker.selected + 1).min(max);
                                    continue;
                                }
                                KeyCode::Char('p') if ctrl => {
                                    picker.selected = picker.selected.saturating_sub(1);
                                    continue;
                                }
                                KeyCode::Char('n') if ctrl => {
                                    let max = filtered.len().saturating_sub(1);
                                    picker.selected = (picker.selected + 1).min(max);
                                    continue;
                                }
                                KeyCode::Backspace => {
                                    picker.query.pop();
                                    picker.selected = 0;
                                    continue;
                                }
                                KeyCode::Char('d') if ctrl => {
                                    // Ctrl-D deletes the selected session (not the current one).
                                    if let Some(&idx) = filtered.get(picker.selected) {
                                        let row = &picker.rows[idx];
                                        if row.is_current {
                                            view.notice(
                                                Level::Warning,
                                                "Cannot delete the current session.",
                                            );
                                            continue;
                                        } else {
                                            session_delete = Some(row.id.clone());
                                        }
                                    } else {
                                        continue;
                                    }
                                }
                                KeyCode::Enter => {
                                    // Resolve selection against the (possibly filtered) list.
                                    let pick_idx = filtered.get(picker.selected).copied();
                                    let target_id = pick_idx.map(|i| picker.rows[i].id.clone());
                                    // Drop the overlay before awaiting, so re-entry is clean.
                                    view.session_picker = None;
                                    if let Some(id) = target_id {
                                        // Guard: refuse while a turn is in flight.
                                        if view.active || !view.asks_empty() {
                                            view.notice(
                                                Level::Warning,
                                                "Wait for the current turn, or press Esc to cancel it first.",
                                            );
                                        } else if id == h.host.context().id {
                                            view.notice(Level::Info, "Already on this session.");
                                        } else {
                                            // Schedule the swap for the next loop iteration;
                                            // the actual close/re-open happens outside the
                                            // key handler to avoid holding a borrow of `h`.
                                            pending_switch = Some(id);
                                        }
                                    }
                                    continue;
                                }
                                KeyCode::Char(c)
                                    if !key
                                        .modifiers
                                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                                {
                                    picker.query.push(c);
                                    picker.selected = 0;
                                    continue;
                                }
                                _ => continue,
                            }
                        }
                        // Handle session deletion outside the picker borrow.
                        if let Some(delete_id) = session_delete.take() {
                            match h.sessions.delete_session(&delete_id).await {
                                Ok(()) => {
                                    view.notice(Level::Info, "Session deleted.");
                                    if let Some(picker) = view.session_picker.as_mut() {
                                        picker.rows.retain(|r| r.id != delete_id);
                                        let max = crate::sessions::filter_sessions(
                                            &picker.rows,
                                            &picker.query,
                                        )
                                        .len()
                                        .saturating_sub(1);
                                        picker.selected = picker.selected.min(max);
                                    }
                                }
                                Err(e) => {
                                    view.notice(Level::Error, format!("Delete failed: {e}"));
                                }
                            }
                            continue;
                        }
                        // Theme picker keyboard routing (after sessions, before commands).
                        if let Some(idx) = view.theme_picker {
                            let count = theme::Theme::ALL.len();
                            match key.code {
                                KeyCode::Esc => {
                                    view.theme_picker = None;
                                    continue;
                                }
                                KeyCode::Up => {
                                    view.theme_picker = Some(idx.saturating_sub(1));
                                    continue;
                                }
                                KeyCode::Down => {
                                    view.theme_picker =
                                        Some((idx + 1).min(count.saturating_sub(1)));
                                    continue;
                                }
                                KeyCode::Char('p') if ctrl => {
                                    view.theme_picker = Some(idx.saturating_sub(1));
                                    continue;
                                }
                                KeyCode::Char('n') if ctrl => {
                                    view.theme_picker =
                                        Some((idx + 1).min(count.saturating_sub(1)));
                                    continue;
                                }
                                KeyCode::Enter => {
                                    let picked = theme::Theme::ALL
                                        .get(idx)
                                        .copied()
                                        .unwrap_or(theme::Theme::System);
                                    view.theme = picked;
                                    view.theme_picker = None;
                                    continue;
                                }
                                _ => continue,
                            }
                        }
                        view.commands.sync(&view.editor.text, view.asks_empty());
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
                                renderer.reveal(view.selected());
                            }
                            KeyCode::Char('t') if ctrl => {
                                view.todo_panel = !view.todo_panel;
                            }
                            KeyCode::Char('o') if ctrl => {
                                view.toggle_recent(false);
                                renderer.reveal(view.selected());
                            }
                            KeyCode::Char('r') if ctrl => {
                                view.toggle_recent(true);
                                renderer.reveal(view.selected());
                            }
                            KeyCode::Char('b') if ctrl => view.stats = !view.stats,
                            KeyCode::Char('y') if ctrl => view.theme = view.theme.next(),
                            KeyCode::End if ctrl => renderer.follow(&mut view),
                            KeyCode::PageUp if alt && !view.asks_empty() => {
                                if let Some(ask) = view.ask_mut() {
                                    ask.scroll = ask.scroll.saturating_sub(5);
                                }
                            }
                            KeyCode::PageDown if alt && !view.asks_empty() => {
                                if let Some(ask) = view.ask_mut() {
                                    ask.scroll = ask.scroll.saturating_add(5);
                                }
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
                                view.dismiss_asks();
                                view.notice(Level::Info, "Cancellation requested.");
                            }
                            KeyCode::Enter
                                if !key
                                    .modifiers
                                    .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
                            {
                                if let Some(ask) = view.ask_mut() {
                                    match ask.answer() {
                                        Ok(payload) => match h.host.submit(In::Reply {
                                            id: ask.id.clone(),
                                            payload,
                                        }) {
                                            Ok(()) => {
                                                view.dismiss_ask();
                                                view.notice(Level::Info, "Reply sent.");
                                            }
                                            Err(e) => {
                                                view.dismiss_ask();
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
                                        // No argument: open the picker overlay.
                                        let current_idx = theme::Theme::ALL
                                            .iter()
                                            .position(|t| *t == view.theme)
                                            .unwrap_or(0);
                                        view.theme_picker = Some(current_idx);
                                    } else if let Some(theme) = theme::Theme::parse(name) {
                                        view.theme = theme;
                                    } else {
                                        view.notice(
                                            Level::Warning,
                                            "Themes: system, dark, light, one-dark, monokai, solarized-dark, solarized-light, nord, dracula, catppuccin, tokyo-night, gruvbox. /theme NAME",
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
                                                limits.clone(),
                                            ));
                                        }
                                        view.editor.take();
                                        continue;
                                    }
                                    "/status" => {
                                        view.stats = !view.stats;
                                        view.editor.take();
                                        continue;
                                    }

                                    s if s == "/models" || s.starts_with("/models ") => {
                                        view.editor.take();
                                        let arg = s.strip_prefix("/models").unwrap().trim();
                                        if arg.is_empty() {
                                            // Open picker.
                                            view.model_picker = Some(0);
                                        } else {
                                            // Direct switch: /models provider/model [variant]
                                            let parts: Vec<&str> = arg.splitn(2, ' ').collect();
                                            let model_id = parts[0].to_string();
                                            let variant = parts.get(1).map(|s| s.to_string());
                                            match switch_model(
                                                &config, h, &mut view, &model_id, variant,
                                            )
                                            .await
                                            {
                                                Ok(label) => {
                                                    view.notice(
                                                        Level::Info,
                                                        format!("Model switched to {label}"),
                                                    );
                                                }
                                                Err(e) => {
                                                    view.notice(
                                                        Level::Error,
                                                        format!("Model switch failed: {e}"),
                                                    );
                                                }
                                            }
                                        }
                                        continue;
                                    }
                                    "/sessions" => {
                                        view.editor.take();
                                        // Guard: refuse while a turn is in flight.
                                        if view.active || !view.asks_empty() {
                                            view.notice(
                                                Level::Warning,
                                                "Wait for the current turn, or press Esc to cancel it first.",
                                            );
                                            continue;
                                        }
                                        // Load sessions off the main loop; populate the overlay.
                                        let metas = match h.sessions.list_sessions().await {
                                            Ok(m) => m,
                                            Err(e) => {
                                                view.notice(
                                                    Level::Error,
                                                    format!("Could not list sessions: {e}"),
                                                );
                                                continue;
                                            }
                                        };
                                        let current = h.host.context().id.clone();
                                        let rows =
                                            crate::sessions::rows_from(metas, Some(&current));
                                        view.session_picker = Some(state::SessionPickerState {
                                            rows,
                                            query: String::new(),
                                            selected: 0,
                                        });
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
                                            let (level, message) = match result {
                                                Ok(r) => (
                                                    Level::Info,
                                                    format!(
                                                        "Context {:?}: {} -> {} estimated tokens. {}",
                                                        r.action,
                                                        r.tokens_before,
                                                        r.tokens_after,
                                                        r.reason
                                                    ),
                                                ),
                                                Err(e) => {
                                                    (Level::Error, format!("Compact failed: {e}"))
                                                }
                                            };
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
                                                limits.clone(),
                                            ));
                                        }
                                        if let Some(title) = view.note_title(body) {
                                            let sessions = h.sessions.clone();
                                            let id = h.host.context().id;
                                            let notice = tx.clone();
                                            tokio::spawn(async move {
                                                let saved = async {
                                                    let mut meta =
                                                        sessions.load_session(&id).await?;
                                                    if meta.title.is_none() {
                                                        meta.title = Some(title);
                                                        sessions.save_session(&meta).await?;
                                                    }
                                                    Result::<(), YourAiError>::Ok(())
                                                }
                                                .await;
                                                if let Err(e) = saved {
                                                    let _ = notice.send(Out::Notice {
                                                        level: Level::Warning,
                                                        message: format!(
                                                            "Session title not saved: {e}"
                                                        ),
                                                    });
                                                }
                                            });
                                        }
                                    }
                                    Err(e) => view.notice(Level::Error, e.to_string()),
                                }
                            }
                            _ => {
                                if let Some(ask) = view.ask_mut() {
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
                    Ok(Ok(())) => "✓ Copied".into(),
                    Ok(Err(e)) => format!("Copy failed: {e}"),
                    Err(e) => format!("Copy failed: {e}"),
                };
                view.toast = Some((message, Instant::now()));
            }
            view.model_metrics = h.budget.snapshot();
            let mut context_tick = false;
            if context_refreshed.elapsed() >= Duration::from_secs(1) {
                let context_next = h.host.context_usage().ok();
                let usage_next = h.usage.session_usage(&h.host.context().id).await.ok();
                context_refreshed = Instant::now();
                // Redraw only when the sidebar data actually moved.
                let context_changed = match (&context_next, &view.context_usage) {
                    (None, None) => false,
                    (Some(a), Some(b)) => {
                        a.estimated_tokens != b.estimated_tokens
                            || a.context_window != b.context_window
                            || a.input_budget != b.input_budget
                            || a.output_reserve != b.output_reserve
                    }
                    _ => true,
                };
                let responses_changed = usage_next
                    .as_ref()
                    .is_some_and(|u| u.request_count != view.recorded_responses);
                if context_changed || responses_changed {
                    context_tick = true;
                }
                view.context_usage = context_next;
                if let Some(usage) = usage_next {
                    view.recorded_responses = usage.request_count;
                }
            }
            let status = h.host.status();
            let queued = h.host.queued();
            let compacting = compact.is_some();
            let snap = FrameSnap::capture(&view, &status, queued, compacting);
            if drained > 0 || inputs > 0 || context_tick || last_snap.as_ref() != Some(&snap) {
                screen
                    .0
                    .draw(|f| renderer.draw(f, &mut view, &meta, &status, queued, compacting))?;
                last_snap = Some(snap);
            }
        }
    };
    let result = ui_future.await;
    if let Some(task) = clipboard_task {
        task.abort();
    }
    cancel.cancel();
    if let Some(h) = harness_opt.take() {
        h.host.interrupt();
        if let Some(task) = compact {
            let _ = task.await;
        }
        if let Some(task) = driver {
            let _ = task.await;
        }
        let id = h.host.context().id.0.clone();
        let pending = h.close().await;
        result?;
        Ok((id, pending?))
    } else {
        result?;
        Ok((meta.session.clone(), vec![]))
    }
}

/// Validate a candidate without changing the live config. The harness publishes
/// model + context only at an idle boundary; errors preserve the old selection.
async fn switch_model(
    config: &Arc<std::sync::Mutex<Config>>,
    harness: &Harness,
    view: &mut View,
    model_id: &str,
    variant: Option<String>,
) -> Result<String, String> {
    let mut candidate = config.lock().map_err(|e| e.to_string())?.clone();
    candidate.model = model_id.to_string();
    let model = candidate
        .resolve(variant.as_deref())
        .map_err(|e| e.to_string())?;
    candidate.selected_variant = variant.clone();
    harness
        .switch_model(model, candidate.context.clone())
        .await
        .map_err(|e| e.to_string())?;
    let p = candidate.pricing();
    *config.lock().map_err(|e| e.to_string())? = candidate;
    view.pricing = match (p.input, p.output) {
        (Some(i), Some(o)) => Some((i, o)),
        _ => None,
    };
    let label = if let Some(v) = &variant {
        format!("{model_id} · {v}")
    } else {
        model_id.to_string()
    };
    view.model_label = label.clone();
    Ok(label)
}

/// Rebuild model-dependent settings from the structured runtime selection.
fn prepare_session(
    config: &Arc<std::sync::Mutex<Config>>,
    template: &HarnessConfig,
    id: SessionId,
) -> Result<(HarnessConfig, Arc<dyn ModelProvider>), Error> {
    let mut candidate = config
        .lock()
        .map_err(|e| Error::from(e.to_string()))?
        .clone();
    let variant = candidate.selected_variant.clone();
    let model = candidate.resolve(variant.as_deref())?;
    let mut hc = template.clone();
    hc.resume = Some(id);
    hc.context_policy = candidate.context.clone();
    hc.request_policy = candidate.request_policy()?;
    (hc.model_header_timeout, hc.model_chunk_timeout) = candidate.model_timeouts()?;
    Ok((hc, model))
}

#[cfg(test)]
mod switch_tests {
    use super::*;

    #[tokio::test]
    async fn failed_switch_is_atomic_and_session_rebuild_keeps_variant() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg: Config = serde_json::from_value(serde_json::json!({
            "model": "mock/large",
            "provider": { "mock": {
                "npm": "@ai-sdk/openai-compatible",
                "options": {"baseURL": "http://127.0.0.1:1/v1", "apiKey": "test"},
                "models": {
                    "large": {"limit": {"context": 64000, "output": 4096}},
                    "small": {"limit": {"context": 32000, "output": 4096},
                        "variants": {"short": {"maxOutputTokens": 2048},
                            "invalid": {"temperature": -1}}}
                }
            }}
        }))
        .unwrap();
        let model = cfg.resolve(None).unwrap();
        let mut hc = HarnessConfig::new(dir.path().join("sessions"), dir.path().into());
        hc.system_prompt = Some("test".into());
        hc.context_policy = cfg.context.clone();
        let h = Harness::open(hc.clone(), model).await.unwrap();
        let config = Arc::new(std::sync::Mutex::new(cfg));
        let mut view = View::default();
        view.model_label = "mock/large".into();
        for (id, variant) in [
            ("missing/model", None),
            ("mock/small", Some("invalid")),
            ("mock/small", Some("missing")),
        ] {
            assert!(
                switch_model(&config, &h, &mut view, id, variant.map(str::to_owned))
                    .await
                    .is_err()
            );
            let cfg = config.lock().unwrap();
            assert_eq!(cfg.model, "mock/large");
            assert_eq!(cfg.context.context_window, Some(64000));
            assert_eq!(cfg.selected_variant, None);
            assert_eq!(view.model_label, "mock/large");
            assert_eq!(h.host.context_usage().unwrap().context_window, Some(64000));
        }
        switch_model(&config, &h, &mut view, "mock/small", Some("short".into()))
            .await
            .unwrap();
        assert_eq!(h.host.context_usage().unwrap().output_reserve, 2048);
        let (resume, _) = prepare_session(&config, &hc, h.host.context().id).unwrap();
        assert_eq!(resume.context_policy.context_window, Some(32000));
        // This differs from the default model's 4096, proving variant resolution.
        assert_eq!(resume.context_policy.output_reserve, 2048);
        assert_eq!(
            config.lock().unwrap().selected_variant.as_deref(),
            Some("short")
        );
        assert_eq!(view.model_label, "mock/small · short");
        h.close().await.unwrap();
    }
}
