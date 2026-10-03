mod app;
mod clipboard;
mod commands;
mod draft;
mod editor;
mod frame_time;
mod markdown;
mod mention;
mod navigation;
mod overlay;
mod presentation;
mod render;
mod selection;
mod session;
mod state;
pub(crate) mod syntax;
pub(crate) mod theme;
use crate::config::{Config, Error};
use app::{App, Flow};
use crossterm::event::{self, Event};
use ratatui::Terminal;
use render::Metadata;
use session::Controller;
use state::View;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use yourai_core::prelude::*;
use yourai_harness::{Harness, HarnessConfig};

use crate::terminal::SyncWriter;

/// Coordinates handing stdin to an external editor. `pause` asks the reader
/// thread to park; `parked` acknowledges that no poll or read is in flight.
/// The acknowledgement is what makes the handoff race-free: an in-flight
/// `event::poll` would otherwise consume the editor's first keystrokes from
/// the fd before the child could read them.
#[derive(Clone, Default)]
struct ReaderGate {
    pause: Arc<AtomicBool>,
    parked: Arc<AtomicBool>,
}
impl ReaderGate {
    fn paused(&self) -> bool {
        self.pause.load(Ordering::Relaxed)
    }
    fn set_paused(&self, paused: bool) {
        self.pause.store(paused, Ordering::Relaxed);
    }
    fn is_parked(&self) -> bool {
        self.parked.load(Ordering::Relaxed)
    }
    fn mark_parked(&self) {
        self.parked.store(true, Ordering::Relaxed);
    }
    fn mark_unparked(&self) {
        self.parked.store(false, Ordering::Relaxed);
    }
    /// Wait until the reader parks, so stdin belongs to the child alone.
    /// Bounded: the reader's poll is short, and a dead reader (which can no
    /// longer steal keystrokes anyway) falls through on timeout.
    async fn wait_parked(&self) {
        let deadline = Instant::now() + Duration::from_millis(500);
        while !self.is_parked() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

struct Screen {
    terminal: Terminal<crate::terminal::SizedBackend<crate::terminal::ScrollBackend<SyncWriter>>>,
    /// Shared alternate-screen guard; dropped after the terminal so the
    /// terminal state is restored once rendering is fully finished.
    _guard: crate::terminal::Screen,
}
impl Screen {
    fn open(scroll_region: crate::terminal::SharedRegion) -> Result<Self, Error> {
        let guard = crate::terminal::Screen::open()?;
        let terminal = Terminal::new(crate::terminal::SizedBackend(
            crate::terminal::ScrollBackend::new(SyncWriter::new(), scroll_region),
        ))?;
        Ok(Self {
            _guard: guard,
            terminal,
        })
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
    view.model_choices = choices;
    let (pricing, effort) = {
        let cfg = config.lock().map_err(|e| Error::from(e.to_string()))?;
        let p = cfg.pricing();
        let pricing = match (p.input, p.output) {
            (Some(i), Some(o)) => Some((i, o)),
            _ => None,
        };
        let effort = cfg.effective_effort(&cfg.model, cfg.selected_variant.as_deref());
        (pricing, effort)
    };
    view.model = state::ModelInfo {
        label: model.into(),
        pricing,
        effort,
    };
    session::restore_history(&harness, &mut view).await?;
    let scroll_region: crate::terminal::SharedRegion = Arc::new(std::sync::Mutex::new(None));
    let mut screen = Screen::open(scroll_region.clone())?;
    let context = harness.host.context();
    let meta = Metadata {
        session: context.id.as_str().to_owned(),
        cwd: context.cwd.to_string_lossy().into(),
        trusted_shell,
        yolo,
    };
    let controller = Controller::new(harness, config, config_template, limits, yolo);
    let mut app = App::new(controller, view, meta);
    // Input is delivered by a dedicated reader thread; the UI loop wakes on
    // the channel instead of polling on a timer, so wheel and key events
    // paint immediately (SSH+tmux adds enough latency of its own). The
    // thread parks while an external editor owns stdin (see `ReaderGate`);
    // the short poll bounds the park handoff. The thread is detached: it
    // exits when the channel closes or the process does.
    let (input_tx, mut input_rx) = mpsc::unbounded_channel();
    let gate = ReaderGate::default();
    let reader = gate.clone();
    std::thread::spawn(move || {
        let mut parked = false;
        while !input_tx.is_closed() {
            if reader.paused() {
                if !parked {
                    parked = true;
                    reader.mark_parked();
                }
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            if parked {
                parked = false;
                reader.mark_unparked();
            }
            match event::poll(Duration::from_millis(100)) {
                Ok(true) => {
                    // A pause may have raced this poll: the event is already
                    // consumed from the fd, so the editor can never see it.
                    // Drop it rather than leak it into the draft later. The
                    // editor itself has not started yet — the UI is still
                    // waiting for the park acknowledgement — so nothing is
                    // stolen from the child either way.
                    let raced = reader.paused();
                    let received = event::read();
                    if !raced {
                        if let Ok(received) = received {
                            if input_tx.send(received).is_err() {
                                break;
                            }
                        }
                    }
                }
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });
    let ui_future = async {
        // Animation and periodic-work clock; unchanged frames still do not redraw.
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut presentation = presentation::Presentation::default();
        loop {
            // A termination signal asks for a graceful shutdown; the signal
            // task force-exits (terminal restored) after a grace period if
            // this loop cannot finish in time.
            if crate::TERMINATED.load(Ordering::Relaxed) != 0 {
                return Ok::<(), Error>(());
            }
            // Wake on input (immediate), harness output (streaming follow), or
            // the tick (animations, stats, timeouts). Whichever fires, the same
            // body below runs; bursts are coalesced into one frame.
            let mut input_queue: Vec<Event> = Vec::new();
            let mut pending_out: Option<Out> = None;
            tokio::select! {
                maybe = input_rx.recv() => {
                    match maybe {
                        Some(received) => {
                            input_queue.push(received);
                            // Everything already queued joins this frame.
                            while input_queue.len() < 512 {
                                match input_rx.try_recv() {
                                    Ok(received) => input_queue.push(received),
                                    Err(_) => break,
                                }
                            }
                            // Two or more at once means a burst (trackpad
                            // momentum, pasted text); its in-flight tail gets a
                            // short window. A lone keypress paints immediately.
                            if input_queue.len() >= 2 {
                                let deadline = tokio::time::Instant::now()
                                    + Duration::from_millis(2);
                                while input_queue.len() < 512 {
                                    match tokio::time::timeout_at(deadline, input_rx.recv()).await {
                                        Ok(Some(received)) => input_queue.push(received),
                                        Ok(None) => break,
                                        Err(_) => break,
                                    }
                                }
                            }
                        }
                        // The reader thread only dies with the process; treat
                        // a closed channel like a quit.
                        None => return Ok::<(), Error>(()),
                    }
                }
                _ = tick.tick() => {}
                _ = tokio::time::sleep_until(presentation.deadline()), if presentation.pending() => {}
                maybe = app.recv() => {
                    pending_out = maybe;
                }
                _ = crate::SHUTDOWN.notified() => {}
            }
            app.poll(pending_out.take()).await;
            if app.take_attention() {
                crate::terminal::bell();
            }
            for received in input_queue {
                match app.handle(received) {
                    Flow::Quit => return Ok::<(), Error>(()),
                    Flow::Editor => {
                        external_editor(
                            &mut app,
                            &gate,
                            &mut input_rx,
                            &mut screen.terminal,
                            &mut presentation,
                        )
                        .await
                    }
                    Flow::Continue => {}
                }
            }
            app.sync_todos();
            app.settle_clipboard().await;
            app.settle_draft().await;
            app.settle_git().await;
            let (queued, compacting) = app.pressure();
            presentation.request();
            let time = frame_time::FrameTime::now();
            presentation.flush(
                &mut screen.terminal,
                time.monotonic,
                &scroll_region,
                |area| app.paint(area, time, queued, compacting),
            )?;
        }
    };
    let result = ui_future.await;
    let closed = app.close().await;
    result?;
    closed
}

/// Hand the terminal to `$VISUAL`/`$EDITOR` with the current draft, then apply
/// the saved text back. The reader parks (and acknowledges) around the child
/// so no editor keystroke leaks into the UI, and the presentation is reset
/// afterwards so the re-entered alternate screen fully repaints.
async fn external_editor(
    app: &mut app::App,
    gate: &ReaderGate,
    input_rx: &mut mpsc::UnboundedReceiver<Event>,
    terminal: &mut Terminal<
        crate::terminal::SizedBackend<crate::terminal::ScrollBackend<crate::terminal::SyncWriter>>,
    >,
    presentation: &mut presentation::Presentation,
) {
    gate.set_paused(true);
    // No in-flight poll may remain: keystrokes typed as the editor starts
    // must reach the child, not be consumed by our reader. The reader parks
    // and acknowledges; whatever it delivered before parking is ours, not
    // the editor's, so drain it once the park is confirmed.
    gate.wait_parked().await;
    while input_rx.try_recv().is_ok() {}
    let text = app.draft_text().to_owned();
    crate::terminal::suspend();
    let outcome = run_editor(&text).await;
    let resumed = crate::terminal::resume();
    // Keystrokes raced at editor exit land here, never in the draft.
    while input_rx.try_recv().is_ok() {}
    gate.set_paused(false);
    if resumed.is_err() {
        return; // Fatal only for the screen; the guard's drop restores.
    }
    // The alternate screen round-trip left the physical screen blank; the
    // backend's cell model and the presentation's last canvas must forget it.
    let _ = terminal.clear();
    *presentation = presentation::Presentation::default();
    app.editor_finished(outcome);
}

/// Run `$VISUAL`/`$EDITOR` (fallback `vi`) on a temp file seeded with `text`.
/// A non-zero exit means cancel (vim's `:cq` convention): the draft is kept.
async fn run_editor(text: &str) -> Result<String, String> {
    let seed = text.to_owned();
    tokio::task::spawn_blocking(move || -> Result<String, String> {
        // Private (0600) and unpredictable: drafts can be sensitive and the
        // temp dir is world-writable, so a predictable `yourai-prompt-{pid}.md`
        // name was a symlink-hijack vector on shared machines. Drop removes
        // the file on every path, editor crashes included. The `.md` suffix
        // lets editors pick markdown mode like the old fixed name did.
        let mut file = tempfile::Builder::new()
            .prefix("yourai-prompt-")
            .suffix(".md")
            .tempfile()
            .map_err(|e| format!("Could not create editor file: {e}"))?;
        std::io::Write::write_all(&mut file, seed.as_bytes())
            .map_err(|e| format!("Could not write editor file: {e}"))?;
        let path = file.path().to_owned();
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".into());
        let mut parts = editor.split_whitespace();
        let program = parts.next().unwrap_or("vi");
        let status = std::process::Command::new(program)
            .args(parts)
            .arg(&path)
            .status()
            .map_err(|e| format!("Could not start editor {editor:?}: {e}"))?;
        if !status.success() {
            return Err(format!("Editor exited with {status}; draft unchanged."));
        }
        // Editors that replace the file (rename/backup) are covered too: the
        // content is read back through the path, and Drop removes whatever
        // currently sits there.
        let edited = std::fs::read_to_string(&path)
            .map_err(|e| format!("Could not read editor file: {e}"))?;
        Ok(edited)
        // `file` drops here and removes the temp file.
    })
    .await
    .map_err(|e| e.to_string())?
}
