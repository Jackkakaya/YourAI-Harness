mod app;
mod clipboard;
mod commands;
mod editor;
mod frame_time;
mod markdown;
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
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;
use yourai_core::prelude::*;
use yourai_harness::{Harness, HarnessConfig};

use crate::terminal::SyncWriter;

struct Screen {
    /// Shared alternate-screen guard; dropped after the terminal so the
    /// terminal state is restored first.
    _guard: crate::terminal::Screen,
    terminal: Terminal<crate::terminal::SizedBackend<crate::terminal::ScrollBackend<SyncWriter>>>,
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
    view.model_choices = choices.iter().map(|c| c.label.clone()).collect();
    view.model = state::ModelInfo {
        label: model.into(),
        pricing: {
            let cfg = config.lock().map_err(|e| Error::from(e.to_string()))?;
            let p = cfg.pricing();
            match (p.input, p.output) {
                (Some(i), Some(o)) => Some((i, o)),
                _ => None,
            }
        },
    };
    session::restore_history(&harness, &mut view).await?;
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        crate::terminal::restore();
        previous(info);
    }));
    let scroll_region: crate::terminal::SharedRegion = Arc::new(std::sync::Mutex::new(None));
    let mut screen = Screen::open(scroll_region.clone())?;
    let context = harness.host.context();
    let meta = Metadata {
        session: context.id.0,
        cwd: context.cwd.to_string_lossy().into(),
        trusted_shell,
        yolo,
    };
    let controller = Controller::new(harness, config, config_template, limits, yolo);
    let mut app = App::new(controller, view, meta, choices);
    // Input is delivered by a dedicated reader thread; the UI loop wakes on
    // the channel instead of polling on a timer, so wheel and key events
    // paint immediately (SSH+tmux adds enough latency of its own). The thread
    // is detached: it exits when the channel closes or the process does.
    let (input_tx, mut input_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(received) = event::read() {
            if input_tx.send(received).is_err() {
                break;
            }
        }
    });
    let ui_future = async {
        // Animation and periodic-work clock; unchanged frames still do not redraw.
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut presentation = presentation::Presentation::default();
        loop {
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
            }
            app.poll(pending_out.take()).await;
            for received in input_queue {
                if app.handle(received) == Flow::Quit {
                    return Ok::<(), Error>(());
                }
            }
            app.sync_todos();
            app.settle_clipboard().await;
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
