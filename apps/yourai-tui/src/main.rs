mod config;
mod launcher;
mod models;
mod picker;
mod sessions;
mod ui;
use config::{Config, Error};
use std::{io::IsTerminal, path::PathBuf, sync::Arc};
use yourai_core::prelude::*;
use yourai_harness::{Harness, HarnessConfig, SessionCatalog};

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("YourAI: {e}");
        std::process::exit(1);
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
                    resume = Some(SessionId(String::new()));
                } else {
                    // Either no more args (bare --resume at the end) or a real ID.
                    resume = match args.next() {
                        Some(v) => Some(SessionId(v)),
                        None => Some(SessionId(String::new())),
                    };
                }
            }
            "--check-config" => check = true,
            "--yolo" => yolo = true,
            "--import-json-sessions" => import_json = true,
            "--help" | "-h" => {
                println!("yourai-tui [--config PATH] [--resume [SESSION_ID]] [--check-config] [--import-json-sessions] [--yolo] [--model PROVIDER/MODEL] [--variant NAME]\n--resume with no ID opens a session picker; Esc starts a fresh session.\nEnter send/reply | Ctrl-J newline | Up/Down·Ctrl-P/N history | Ctrl-A/E/W/U/K line edit | Click tool/thinking to expand | F6 select | Ctrl-O toggle | Ctrl-T todo panel | Ctrl-B stats\nEsc cancel/overlay | Ctrl-Q quit | PgUp/PgDn scroll | Ctrl-End follow\n/queue TEXT | /compact | /continue | /clear | /theme NAME | /models | /sessions | /status | /help");
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
    let model = config.resolve(variant.as_deref())?;
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
    let request_policy = config.request_policy()?;
    let (model_header_timeout, model_chunk_timeout) = config.model_timeouts()?;
    let choices = crate::models::model_choices(&config);
    // Bare `--resume` opens the launcher; Esc there falls through to a fresh session.
    if resume.as_ref().is_some_and(|SessionId(id)| id.is_empty()) {
        let catalog = SessionCatalog::new(&config.session_dir)?;
        match launcher::pick(&catalog).await? {
            Some(picked) => resume = Some(picked),
            None => resume = None,
        }
    }
    let mut hc = HarnessConfig::new(config.session_dir.clone(), std::env::current_dir()?);
    hc.request_policy = request_policy.clone();
    hc.model_header_timeout = model_header_timeout;
    hc.model_chunk_timeout = model_chunk_timeout;
    hc.context_policy = config.context.clone();
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
    limits.max_model_calls = config.max_model_calls;
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
