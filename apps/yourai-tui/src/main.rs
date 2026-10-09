mod branding;
mod config;
mod git;
mod launcher;
mod models;
mod picker;
mod sessions;
mod terminal;
mod text;
mod ui;
use config::{Config, Error};
use std::{io::IsTerminal, path::PathBuf, sync::Arc};
use yourai_core::prelude::*;
use yourai_harness::{Harness, HarnessConfig, SessionCatalog};

#[tokio::main]
async fn main() {
    // Both hooks must precede any terminal state change: a panic or an
    // external signal would otherwise leave the terminal in raw mode on the
    // alternate screen.
    install_panic_hook();
    watch_termination_signals();
    let result = run().await;
    let signal = TERMINATED.load(std::sync::atomic::Ordering::Relaxed);
    match (result, signal) {
        // A termination signal owns the exit status even when the shutdown
        // path hit terminal errors on the way out (a dropped SSH tty, say):
        // callers expect 128+signum, not a generic failure.
        (_, code) if code != 0 => std::process::exit(code),
        (Err(e), _) => {
            eprintln!("YourAI: {e}");
            std::process::exit(1);
        }
        (Ok(()), _) => {}
    }
}

/// How the process was asked to terminate: 0 = not at all, otherwise the
/// conventional 128+signum. The UI loop polls this for a graceful shutdown;
/// `main` turns it into the process exit status.
pub(crate) static TERMINATED: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Wakes the UI loop when a termination signal arrives. `notify_waiters`
/// only reaches currently-registered waiters, so the loop also re-checks
/// `TERMINATED` at the top of every iteration — its 100ms tick bounds the
/// window in which a notification could be missed.
pub(crate) static SHUTDOWN: std::sync::LazyLock<tokio::sync::Notify> =
    std::sync::LazyLock::new(tokio::sync::Notify::new);

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        terminal::restore();
        previous(info);
    }));
}

/// SIGTERM/SIGHUP/SIGQUIT (SSH drop, tmux kill-pane, logout) trigger a
/// graceful shutdown so the session close path (driver join, pending-input
/// flush, title backfill) runs like a normal quit. If the UI loop cannot
/// finish within the grace period — it has not started yet, or it is stuck
/// in the external editor — restore the terminal and exit with the signal's
/// conventional code.
fn watch_termination_signals() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let Ok(mut terminate) = signal(SignalKind::terminate()) else {
            return;
        };
        let Ok(mut hangup) = signal(SignalKind::hangup()) else {
            return;
        };
        let Ok(mut quit) = signal(SignalKind::quit()) else {
            return;
        };
        tokio::spawn(async move {
            let code = tokio::select! {
                _ = terminate.recv() => 128 + libc::SIGTERM,
                _ = hangup.recv() => 128 + libc::SIGHUP,
                _ = quit.recv() => 128 + libc::SIGQUIT,
            };
            TERMINATED.store(code, std::sync::atomic::Ordering::Relaxed);
            SHUTDOWN.notify_waiters();
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            terminal::restore();
            std::process::exit(code);
        });
    }
}
async fn run() -> Result<(), Error> {
    let mut config_path: Option<PathBuf> = None;
    let mut resume = None;
    let mut selected_model = None;
    let mut variant = None;
    let mut check = false;
    let mut yolo = false;
    let mut import_json = false;
    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config_path = Some(args.next().ok_or("--config requires a path")?.into()),
            "--model" => {
                selected_model = Some(args.next().ok_or("--model requires provider/model")?)
            }
            "--variant" => variant = Some(args.next().ok_or("--variant requires a name")?),
            "--resume" => {
                // Optional value: bare `--resume` (or followed by another `--flag`,
                // or no more args) launches the session picker; `--resume <ID>`
                // resumes directly. Peek without consuming; only swallow the next
                // token if it looks like an ID.
                let next_is_flag = args.peek().is_some_and(|v| v.starts_with("--"));
                if next_is_flag {
                    resume = Some(SessionId::from(String::new()));
                } else {
                    // Either no more args (bare --resume at the end) or a real ID.
                    resume = match args.next() {
                        Some(v) => Some(SessionId::from(v)),
                        None => Some(SessionId::from(String::new())),
                    };
                }
            }
            "--check-config" => check = true,
            "--yolo" => yolo = true,
            "--import-json-sessions" => import_json = true,
            "--help" | "-h" => {
                println!("yourai-tui [--config PATH] [--resume [SESSION_ID]] [--check-config] [--import-json-sessions] [--yolo] [--model PROVIDER/MODEL] [--variant NAME]\n--resume with no ID opens a session picker; Esc starts a fresh session.\nEnter send/reply | Ctrl-J newline | Up/Down·Ctrl-P/N history | Ctrl-A/E/W/U/K line edit | Click tool/thinking to expand | F6 select | Ctrl-O toggle | Ctrl-T todo panel | Ctrl-B stats\nEsc cancel/overlay | Ctrl-Q quit | PgUp/PgDn scroll | Ctrl-End follow | Ctrl-Home latest question | Ctrl-Up/Down browse questions | Ctrl-G YOLO | F2 cycle configured models | Ctrl-X edit draft in $VISUAL/$EDITOR\n/new | /clear (fresh context, saved history retained) | /yolo [on|off] | /queue TEXT | /compact | /continue | /editor | /theme NAME | /models | /sessions | /status | /help");
                return Ok(());
            }
            _ => return Err(format!("Unknown argument: {arg}").into()),
        }
    }
    let path = match config_path {
        Some(p) => p,
        None => config::default_config_path()?,
    };
    let mut config = Config::load(&path)?;
    // Resolve `theme: "system"` once at startup (v1: no runtime watch).
    if config.theme == crate::ui::theme::Theme::System {
        crate::ui::theme::resolve_system_from_os();
    }
    if let Some(model) = selected_model {
        config.model = model;
    }
    let yolo = yolo || config.yolo;
    if import_json {
        let count = yourai_harness::storage::sqlite::import_json_sessions(&config.session_dir)?;
        println!(
            "Imported {count} sessions into {}. Original JSON files retained.",
            yourai_harness::SqliteStore::path(&config.session_dir).display()
        );
        return Ok(());
    }
    let selection = config.resolve(variant.as_deref())?;
    let model = selection.model.clone();
    config.context = selection.context.clone();
    config.selected_variant = variant;
    if config.context.input_budget().is_none() {
        eprintln!("Context window unknown: configure provider.<id>.models.<id>.limit.context to enable automatic/manual summarization.");
    }
    if check {
        println!(
            "Config OK: {} / {} (no network request)",
            config.model,
            model.model_iden()
        );
        return Ok(());
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("TUI requires an interactive terminal".into());
    }
    let choices = crate::models::model_choices(&config);
    // Bare `--resume` opens the launcher; Esc there falls through to a fresh session.
    if resume.as_ref().is_some_and(|id| id.as_str().is_empty()) {
        let catalog = SessionCatalog::new(&config.session_dir)?;
        match launcher::pick(&catalog, config.theme).await? {
            launcher::Choice::Resume(picked) => resume = Some(picked),
            launcher::Choice::New => resume = None,
            launcher::Choice::Quit => return Ok(()),
        }
    }
    let mut hc = HarnessConfig::new(config.session_dir.clone(), std::env::current_dir()?);
    selection.apply_to(&mut hc);
    hc.resume = resume.take();
    hc.system_prompt = config.system_prompt.clone();
    hc.prompt = config.prompt.clone();
    hc.memory_search_limit = config.memory_search_limit;
    hc.extensions = config.extensions;
    hc.trusted_shell = config.trusted_shell;
    hc.yolo = yolo;
    let mut config_template = hc.clone();
    config_template.resume = None;
    let harness = Harness::open(hc, model).await?;
    let mut limits = TurnLimits::default();
    limits.steps = config.steps;
    let config_clone_model = match &config.selected_variant {
        Some(variant) => format!("{} · {variant}", config.model),
        None => config.model.clone(),
    };
    let config_clone_trusted = config.trusted_shell;
    let config_clone_theme = config.theme;
    let config = Arc::new(std::sync::Mutex::new(config));
    let (session_id, pending) = ui::run(
        harness,
        &config_clone_model,
        config_clone_trusted,
        yolo,
        config_clone_theme,
        limits,
        config.clone(),
        choices,
        config_template,
    )
    .await?;
    eprintln!("Session: {session_id}");
    if !pending.is_empty() {
        eprintln!(
            "Unprocessed inputs returned on close: {}",
            serde_json::to_string(&pending)?
        );
    }
    Ok(())
}
