mod config;
mod ui;
use config::{Config, Error};
use std::{io::IsTerminal, path::PathBuf};
use yourai_core::prelude::*;
use yourai_runtime::{Harness, HarnessConfig};

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("YourAI: {e}");
        std::process::exit(1);
    }
}
async fn run() -> Result<(), Error> {
    let mut path = PathBuf::from("yourai.toml");
    let mut resume = None;
    let mut check = false;
    let mut import_json = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => path = args.next().ok_or("--config requires a path")?.into(),
            "--resume" => {
                resume = Some(SessionId(
                    args.next().ok_or("--resume requires a session ID")?,
                ))
            }
            "--check-config" => check = true,
            "--import-json-sessions" => import_json = true,
            "--help" | "-h" => {
                println!("yourai-tui [--config yourai.toml] [--resume SESSION_ID] [--check-config] [--import-json-sessions]\nEnter send/reply | Esc cancel | Ctrl-Q quit | PgUp/PgDn scroll\n/queue TEXT | /compact | /quit | /help");
                return Ok(());
            }
            _ => return Err(format!("Unknown argument: {arg}").into()),
        }
    }
    let config = Config::load(&path)?;
    if import_json {
        let count = yourai_runtime::sqlite::import_json_sessions(&config.session_dir)?;
        println!(
            "Imported {count} sessions into {}. Original JSON files retained.",
            yourai_runtime::SqliteStore::path(&config.session_dir).display()
        );
        return Ok(());
    }
    let model = config.model()?;
    if config.context.input_budget().is_none() {
        eprintln!("Context window unknown: configure [context].context_window to enable automatic/manual summarization.");
    }
    if check {
        println!(
            "Config OK: {} / {} (no network request)",
            config.model.provider, config.model.name
        );
        return Ok(());
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("TUI requires an interactive terminal".into());
    }
    let mut hc = HarnessConfig::new(config.session_dir, std::env::current_dir()?);
    hc.context_policy = config.context;
    hc.resume = resume;
    hc.system_prompt = config.system_prompt;
    hc.extensions = config.extensions;
    let harness = Harness::open(hc, model).await?;
    let result = ui::run(&harness, &config.model.name).await;
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
