mod config;
mod ui;
use config::{Config, Error};
use std::{io::IsTerminal, path::PathBuf};
use yourai_core::prelude::*;
use yourai_harness::{Harness, HarnessConfig};

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("YourAI: {e}");
        std::process::exit(1);
    }
}
async fn run() -> Result<(), Error> {
    let mut path = PathBuf::from("yourai.json");
    let mut resume = None;
    let mut selected_model = None;
    let mut variant = None;
    let mut check = false;
    let mut yolo = false;
    let mut import_json = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => path = args.next().ok_or("--config requires a path")?.into(),
            "--model" => {
                selected_model = Some(args.next().ok_or("--model requires provider/model")?)
            }
            "--variant" => variant = Some(args.next().ok_or("--variant requires a name")?),
            "--resume" => {
                resume = Some(SessionId(
                    args.next().ok_or("--resume requires a session ID")?,
                ))
            }
            "--check-config" => check = true,
            "--yolo" => yolo = true,
            "--import-json-sessions" => import_json = true,
            "--help" | "-h" => {
                println!("yourai-tui [--config yourai.json] [--resume SESSION_ID] [--check-config] [--import-json-sessions] [--yolo] [--model PROVIDER/MODEL] [--variant NAME]\nEnter send/reply | Ctrl-J newline | Ctrl-P/N history | Click tool/thinking to expand | F6 select | Ctrl-O toggle | Ctrl-T TODO\nEsc cancel | Ctrl-Q quit | PgUp/PgDn scroll | Ctrl-End follow\n/queue TEXT | /compact | /continue | /clear | /quit | /help");
                return Ok(());
            }
            _ => return Err(format!("Unknown argument: {arg}").into()),
        }
    }
    let mut config = Config::load(&path)?;
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
    let mut hc = HarnessConfig::new(config.session_dir, std::env::current_dir()?);
    hc.request_policy = request_policy;
    hc.context_policy = config.context;
    hc.resume = resume;
    hc.system_prompt = config.system_prompt;
    hc.prompt = config.prompt;
    hc.memory_search_limit = config.memory_search_limit;
    hc.extensions = config.extensions;
    hc.trusted_shell = config.trusted_shell;
    hc.yolo = yolo;
    let harness = Harness::open(hc, model).await?;
    let result = ui::run(
        &harness,
        &config.model,
        config.trusted_shell,
        yolo,
        config.theme,
    )
    .await;
    let pending = harness.close().await?;
    eprintln!("Session: {}", harness.host.context().id.0);
    if !pending.is_empty() {
        eprintln!(
            "Unprocessed inputs returned on close: {}",
            serde_json::to_string(&pending)?
        );
    }
    result
}
