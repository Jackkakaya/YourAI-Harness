//! Offline demo: cargo run -p yourai-harness --example session_config
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use yourai_core::{context::DiscardSink, prelude::*};
use yourai_harness::{GenaiModel, Harness, SessionOptions};

// This program completes its turn without calling the model or any external service.
struct DemoLoop;
impl AgentLoop for DemoLoop {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            let mut turn = Turn::open(tc).await?;
            println!("Turn uses frozen steps: {:?}", turn.config().steps());
            let result = turn.complete("done").await.map(|_| ());
            turn.finish(result).await
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let mut options =
        SessionOptions::new(directory.path().join("sessions"), directory.path().into());
    options.config.set_steps(Some(20));
    options.agent_loop = Some(Arc::new(DemoLoop));
    let harness = Harness::open(
        options,
        Arc::new(GenaiModel::new(
            genai::Client::builder().build()?,
            "example-model",
        )),
    )
    .await?;
    let session = &harness.host;

    // Startup settings stay immutable. Per-turn overrides can tighten limits.
    println!("Session startup steps: {:?}", session.config().steps());
    let mut limits = TurnLimits::default();
    limits.steps = Some(10);

    session.submit(In::user_text("demonstrate configuration"))?;
    let report = session
        .run_next(limits, &DiscardSink, &CancellationToken::new())
        .await?
        .expect("queued input");
    report.result?;
    harness.close().await?;
    Ok(())
}
