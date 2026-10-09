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
    /// Wait for the reader to acknowledge either side of the stdin handoff.
    /// Bounded: a dead reader can no longer consume keystrokes.
    async fn wait_parked(&self, parked: bool) {
        let deadline = Instant::now() + Duration::from_millis(500);
        while self.is_parked() != parked && Instant::now() < deadline {
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
                        .await?;
                        if crate::TERMINATED.load(Ordering::Relaxed) != 0 {
                            return Ok::<(), Error>(());
                        }
                        // The editor handoff consumes the rest of this input burst.
                        // Those events belong to the screen we just suspended.
                        break;
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
) -> Result<(), Error> {
    gate.set_paused(true);
    // No in-flight poll may remain: keystrokes typed as the editor starts
    // must reach the child, not be consumed by our reader. The reader parks
    // and acknowledges; whatever it delivered before parking is ours, not
    // the editor's, so drain it once the park is confirmed.
    gate.wait_parked(true).await;
    while input_rx.try_recv().is_ok() {}
    let text = app.draft_text().to_owned();
    crate::terminal::suspend();
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let outcome = run_editor(&text, &editor, async {
        let shutdown = crate::SHUTDOWN.notified();
        tokio::pin!(shutdown);
        // Register before checking: notify_waiters does not store a permit.
        shutdown.as_mut().enable();
        if crate::TERMINATED.load(Ordering::Relaxed) == 0 {
            shutdown.await;
        }
    })
    .await;
    let resumed = if crate::TERMINATED.load(Ordering::Relaxed) == 0 {
        crate::terminal::resume()
    } else {
        Ok(())
    };
    // Keystrokes raced at editor exit land here, never in the draft.
    while input_rx.try_recv().is_ok() {}
    gate.set_paused(false);
    // Complete the return handoff before another editor can reuse the gate.
    gate.wait_parked(false).await;
    resumed?;
    if crate::TERMINATED.load(Ordering::Relaxed) != 0 {
        return Ok(());
    }
    // The alternate screen round-trip left the physical screen blank; the
    // backend's cell model and the presentation's last canvas must forget it.
    terminal.clear()?;
    *presentation = presentation::Presentation::default();
    app.editor_finished(outcome);
    Ok(())
}

/// Run `$VISUAL`/`$EDITOR` (fallback `vi`) on a temp file seeded with `text`.
/// A non-zero exit means cancel (vim's `:cq` convention): the draft is kept.
async fn run_editor(
    text: &str,
    editor: &str,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<String, String> {
    // Own a private temp file for the entire child lifetime, including shutdown.
    let mut file = tempfile::Builder::new()
        .prefix("yourai-prompt-")
        .suffix(".md")
        .tempfile()
        .map_err(|e| format!("Could not create editor file: {e}"))?;
    std::io::Write::write_all(&mut file, text.as_bytes())
        .map_err(|e| format!("Could not write editor file: {e}"))?;
    // Parse quoting without a shell: paths and arguments may contain spaces,
    // but user configuration is not interpreted as a shell script.
    let parts = shlex::split(editor)
        .ok_or_else(|| "Invalid editor quoting; draft unchanged.".to_owned())?;
    let (program, args) = parts
        .split_first()
        .ok_or_else(|| "Empty editor command; draft unchanged.".to_owned())?;
    let mut command = tokio::process::Command::new(program);
    command.args(args).arg(file.path()).kill_on_drop(true);
    #[cfg(unix)]
    let foreground = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
    #[cfg(unix)]
    {
        // A separate group lets shutdown stop the editor and its children.
        // Give it the foreground terminal before exec, so interactive editors
        // can read stdin without being stopped by SIGTTIN.
        command.process_group(0);
        if foreground > 0 {
            unsafe {
                command.pre_exec(|| foreground_process(libc::getpid()));
            }
        }
    }
    let outcome = async {
        let mut child = command
            .spawn()
            .map_err(|e| format!("Could not start editor {editor:?}: {e}"))?;
        #[cfg(unix)]
        let pid = child.id();
        let status = tokio::select! {
            biased;
            _ = shutdown => {
                #[cfg(unix)]
                if let Some(pid) = pid {
                    unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL); }
                }
                // Also kill/reap the direct child on platforms without groups.
                let _ = child.start_kill();
                child.wait().await.map_err(|e| format!("Could not reap editor: {e}"))?;
                return Err("Editor interrupted; draft unchanged.".into());
            }
            result = child.wait() => result.map_err(|e| format!("Could not wait for editor: {e}"))?,
        };
        if !status.success() {
            return Err(format!("Editor exited with {status}; draft unchanged."));
        }
        // Read through the path: editors may save by replacing the file.
        std::fs::read_to_string(file.path()).map_err(|e| format!("Could not read editor file: {e}"))
    }
    .await;
    #[cfg(unix)]
    if foreground > 0 {
        foreground_process(foreground)
            .map_err(|e| format!("Could not restore terminal foreground: {e}"))?;
    }
    outcome
}

/// tcsetpgrp from a background group normally raises SIGTTOU. Block it only
/// for the handoff, then restore this thread's original signal mask.
#[cfg(unix)]
fn foreground_process(group: libc::pid_t) -> std::io::Result<()> {
    unsafe {
        let mut mask = std::mem::zeroed();
        let mut previous = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGTTOU);
        let code = libc::pthread_sigmask(libc::SIG_BLOCK, &mask, &mut previous);
        if code != 0 {
            return Err(std::io::Error::from_raw_os_error(code));
        }
        let result = libc::tcsetpgrp(libc::STDIN_FILENO, group);
        let error = std::io::Error::last_os_error();
        libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
        if result == 0 {
            Ok(())
        } else {
            Err(error)
        }
    }
}

#[cfg(all(test, unix))]
mod editor_tests {
    use super::run_editor;
    use std::{os::unix::fs::PermissionsExt, path::Path, time::Duration};

    fn script(dir: &Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("editor with spaces");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[tokio::test]
    async fn quoted_editor_and_arguments_save_and_remove_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("file path");
        let editor = script(dir.path(), "[ \"$1\" = 'argument with spaces' ] || exit 9\nprintf '%s' \"$3\" > \"$2\"\nprintf 'edited draft\\n' > \"$3\"");
        let command = format!("{editor:?} 'argument with spaces' {record:?}");
        let result = run_editor("original draft", &command, std::future::pending())
            .await
            .unwrap();
        assert_eq!(result, "edited draft\n");
        let temporary = std::fs::read_to_string(record).unwrap();
        assert!(!Path::new(&temporary).exists());
        // A cancelled edit does not admit the file's modified contents.
        let editor = script(dir.path(), "printf 'cancelled changes' > \"$1\"\nexit 1");
        assert!(run_editor(
            "original draft",
            &format!("{editor:?}"),
            std::future::pending()
        )
        .await
        .unwrap_err()
        .contains("draft unchanged"));
        assert!(
            run_editor("original draft", "'unterminated", std::future::pending())
                .await
                .unwrap_err()
                .contains("quoting")
        );
    }

    #[tokio::test]
    async fn shutdown_terminates_editor_and_descendants_and_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("pids");
        let editor = script(
            dir.path(),
            "sleep 60 &\nprintf '%s %s %s' \"$$\" \"$!\" \"$2\" > \"$1\"\nwait",
        );
        let command = format!("{editor:?} {record:?}");
        let shutdown = async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            while tokio::time::Instant::now() < deadline {
                if std::fs::read_to_string(&record).is_ok_and(|s| s.split_whitespace().count() >= 3)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        let result = run_editor("original draft", &command, shutdown).await;
        assert!(result.unwrap_err().contains("interrupted"));
        let recorded = std::fs::read_to_string(record).unwrap();
        let mut fields = recorded.splitn(3, ' ');
        let parent: i32 = fields.next().unwrap().parse().unwrap();
        let descendant: i32 = fields.next().unwrap().parse().unwrap();
        assert!(!Path::new(fields.next().unwrap()).exists());
        assert_eq!(
            unsafe { libc::kill(parent, 0) },
            -1,
            "direct child was reaped"
        );
        // An orphan may briefly be a zombie before init reaps it; either state
        // means the descendant has stopped executing.
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let output = tokio::process::Command::new("ps")
                    .args(["-o", "stat=", "-p", &descendant.to_string()])
                    .output()
                    .await
                    .unwrap();
                let state = String::from_utf8_lossy(&output.stdout);
                if state.trim().is_empty() || state.trim_start().starts_with('Z') {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("editor descendant remained alive after shutdown");
    }
}
