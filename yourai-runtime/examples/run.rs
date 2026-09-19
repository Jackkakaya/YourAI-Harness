//! cargo run -p yourai-runtime --example run -- <model> <prompt>
//! Credentials are read by genai from the environment. This example denies interactive approvals.
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use yourai_core::prelude::*;
use yourai_runtime::{GenaiModel, Harness, HarnessConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let model = args.next().ok_or("expected model and prompt")?;
    let prompt = args.collect::<Vec<_>>().join(" ");
    if prompt.is_empty() {
        return Err("expected prompt".into());
    }
    let cwd = std::env::current_dir()?;
    let harness = Harness::open(
        HarnessConfig::new(cwd.join(".yourai/sessions"), cwd),
        Arc::new(GenaiModel::new(genai::Client::default(), model)),
    )
    .await?;
    harness.host.submit(In::user_text(prompt))?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let host = harness.host.clone();
    let output = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if let Out::Ask { id, .. } = &event {
                let _ = host.submit(In::Reply {
                    id: id.clone(),
                    payload: serde_json::json!({"behavior":"deny"}),
                });
            }
            println!("{}", serde_json::to_string(&event).unwrap());
        }
    });
    let result = harness
        .host
        .run_until_idle(TurnLimits::default(), &tx, &CancellationToken::new())
        .await;
    drop(tx);
    output.await?;
    let pending = harness.close().await?;
    eprintln!(
        "Session: {}; pending inputs: {}",
        harness.host.context().id.0,
        pending.len()
    );
    for report in result? {
        report.result?;
    }
    Ok(())
}
