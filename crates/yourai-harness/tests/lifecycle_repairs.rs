#[path = "support/loop.rs"]
mod support;

use std::{sync::Arc, time::Duration};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use yourai_core::{context::DiscardSink, model::ModelEventStream, prelude::*};
use yourai_harness::{Harness, HarnessConfig, HostConfig, MemoryContext, SessionHost};

struct BrokenLoop {
    panic: bool,
}
impl AgentLoop for BrokenLoop {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            let _input = tc.inbox.recv().await;
            if self.panic {
                panic!("loop failed after consuming input");
            }
            tc.outbox.send(Out::Notice {
                level: Level::Info,
                message: "started".into(),
            });
            std::future::pending().await
        })
    }
}

async fn loop_host(dir: &std::path::Path, loop_: Arc<dyn AgentLoop>) -> Arc<SessionHost> {
    let id = SessionId::new();
    let agent = Agent::builder()
        .agent_loop(loop_)
        .context_manager(MemoryContext::memory(id.clone()))
        .build();
    SessionHost::open(
        dir,
        SessionContext::new(id, dir),
        agent,
        HostConfig::default(),
        "startup",
    )
    .await
    .unwrap()
}

struct SteerOnStart(Arc<SessionHost>);
impl OutSink for SteerOnStart {
    fn send(&self, event: Out) -> bool {
        if matches!(event, Out::Notice { ref message, .. } if message == "started") {
            self.0.submit(In::user_text("unread steer")).unwrap();
        }
        true
    }
}

#[tokio::test(start_paused = true)]
async fn watchdog_quarantines_and_retains_active_inputs_without_replaying_unread_steer() {
    let dir = TempDir::new().unwrap();
    let host = loop_host(dir.path(), Arc::new(BrokenLoop { panic: false })).await;
    host.submit(In::user_text("consumed first input")).unwrap();
    host.submit(In::follow_up("queued follow-up")).unwrap();
    let mut limits = TurnLimits::default();
    limits.deadline = Some(std::time::Instant::now());
    let report = host
        .run_next(
            limits,
            &SteerOnStart(host.clone()),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(report.result.is_err());
    assert_eq!(host.status(), SessionStatus::Closing);
    assert_eq!(host.interrupted_inputs().len(), 2);
    assert_eq!(host.queued(), 1);
    assert!(host.submit(In::user_text("must not execute")).is_err());
    assert!(host
        .run_next(
            TurnLimits::default(),
            &DiscardSink,
            &CancellationToken::new()
        )
        .await
        .is_err());
    assert_eq!(host.close(None).await.unwrap().len(), 1);
    let journal: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("host.json")).unwrap()).unwrap();
    assert_eq!(journal["interrupted"].as_array().unwrap().len(), 2);
    assert!(journal["active"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn panicking_loop_quarantines_and_preserves_consumed_input() {
    let dir = TempDir::new().unwrap();
    let host = loop_host(dir.path(), Arc::new(BrokenLoop { panic: true })).await;
    host.submit(In::user_text("consumed before panic")).unwrap();
    let report = host
        .run_next(
            TurnLimits::default(),
            &DiscardSink,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(report.result.unwrap_err().to_string().contains("panicked"));
    assert_eq!(host.status(), SessionStatus::Closing);
    assert_eq!(host.interrupted_inputs().len(), 1);
    host.close(None).await.unwrap();
}

struct ConstructorPanic;
impl AgentLoop for ConstructorPanic {
    fn run_turn<'a>(&'a self, _: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        panic!("panicked before returning the loop future");
    }
}

#[tokio::test]
async fn panic_during_loop_future_construction_also_quarantines() {
    let dir = TempDir::new().unwrap();
    let host = loop_host(dir.path(), Arc::new(ConstructorPanic)).await;
    host.submit(In::user_text("not yet consumed")).unwrap();
    let report = host
        .run_next(
            TurnLimits::default(),
            &DiscardSink,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        *report.result.unwrap_err().error,
        YourAiError::Error(ErrorKind::LoopTerminated(_))
    ));
    assert_eq!(host.status(), SessionStatus::Closing);
    assert_eq!(host.interrupted_inputs().len(), 1);
    assert_eq!(host.queued(), 0);
    host.close(None).await.unwrap();
}

struct NamedModel {
    name: &'static str,
    inner: support::Model,
}
impl NamedModel {
    fn new(name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            name,
            inner: support::Model::new(vec![support::answer("still usable")]),
        })
    }
}
impl ModelProvider for NamedModel {
    fn model_iden(&self) -> &str {
        self.name
    }
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
        self.inner.complete(request)
    }
    fn stream_events<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
        self.inner.stream_events(request)
    }
}

#[tokio::test]
async fn rejected_resume_and_restore_do_not_change_active_session_metadata() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("sessions");
    let harness = Harness::open(
        HarnessConfig::new(root.clone(), dir.path().into()),
        NamedModel::new("first"),
    )
    .await
    .unwrap();
    let id = harness.host.context().id;
    let mut config = HarnessConfig::new(root.clone(), dir.path().into());
    config.resume = Some(id.clone());
    assert!(Harness::open(config, NamedModel::new("second"))
        .await
        .is_err());
    assert_eq!(
        harness
            .sessions
            .load_session(&id)
            .await
            .unwrap()
            .model
            .as_deref(),
        Some("first")
    );
    assert!(SessionHost::restore(
        &root,
        id.clone(),
        dir.path().into(),
        NamedModel::new("third"),
        None,
        None,
        ContextPolicy::default(),
        "resume",
    )
    .await
    .is_err());
    assert_eq!(
        harness
            .sessions
            .load_session(&id)
            .await
            .unwrap()
            .model
            .as_deref(),
        Some("first")
    );
    harness
        .host
        .submit_async(In::user_text("go"))
        .await
        .unwrap();
    let report = harness
        .host
        .run_next(
            TurnLimits::default(),
            &DiscardSink,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.result.unwrap().text, "still usable");
    harness.close().await.unwrap();
}

#[tokio::test]
async fn oversized_hook_first_line_and_combined_stdout_are_rejected() {
    use yourai_core::hooks::{BaseInput, HookEvent, HookInvocation, HookRunStatus, HookSource};
    use yourai_harness::hooks::{ConcreteHookRuntime, HooksConfig};
    for (command, failure_policy, stopped) in [
        ("head -c 2097152 /dev/zero | tr '\\0' x", "open", false),
        (
            "printf 'prefix\\n'; head -c 1048576 /dev/zero | tr '\\0' x",
            "closed",
            true,
        ),
    ] {
        let runtime = ConcreteHookRuntime::new();
        let config: HooksConfig = serde_json::from_value(serde_json::json!({
            "hooks": {"UserPromptSubmit": [{"hooks": [{"type":"command", "command":command, "failurePolicy":failure_policy}]}]}
        }))
        .unwrap();
        runtime
            .register_config(&config, HookSource::Project)
            .await
            .unwrap();
        let invocation = HookInvocation::new(
            BaseInput::new("probe", "/tmp"),
            HookEvent::UserPromptSubmit {
                prompt: "hi".into(),
            },
        );
        let result = tokio::time::timeout(Duration::from_secs(5), runtime.dispatch(&invocation))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.runs[0].status, HookRunStatus::Failed);
        assert_eq!(result.common.prevent_continuation, stopped);
        assert!(result.runs[0]
            .stderr
            .as_deref()
            .unwrap()
            .contains("output exceeds"));
    }
}

#[tokio::test]
async fn hook_stdout_exactly_at_shared_limit_is_preserved() {
    use yourai_core::hooks::{BaseInput, HookEvent, HookInvocation, HookRunStatus, HookSource};
    use yourai_harness::hooks::{ConcreteHookRuntime, HooksConfig};
    let runtime = ConcreteHookRuntime::new();
    let config: HooksConfig = serde_json::from_value(serde_json::json!({
        "hooks": {"UserPromptSubmit": [{"hooks": [{
            "type":"command", "command":"printf 'hi\\n'; head -c 1048573 /dev/zero | tr '\\0' x"
        }]}]}
    }))
    .unwrap();
    runtime
        .register_config(&config, HookSource::Project)
        .await
        .unwrap();
    let invocation = HookInvocation::new(
        BaseInput::new("probe", "/tmp"),
        HookEvent::UserPromptSubmit {
            prompt: "hi".into(),
        },
    );
    let result = runtime.dispatch(&invocation).await.unwrap();
    assert_eq!(result.runs[0].status, HookRunStatus::Completed);
    let stdout = result.runs[0].stdout.as_deref().unwrap();
    assert_eq!(stdout.len(), 1024 * 1024);
    assert!(stdout.starts_with("hi\n"));
    assert!(stdout.ends_with('x'));
}

#[tokio::test]
async fn failed_assembly_releases_execution_lease_for_the_next_open() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("sessions");
    let harness = Harness::open(
        HarnessConfig::new(root.clone(), dir.path().into()),
        NamedModel::new("first"),
    )
    .await
    .unwrap();
    let id = harness.host.context().id;
    harness.close().await.unwrap();
    let mut config = HarnessConfig::new(root.clone(), dir.path().into());
    config.resume = Some(id.clone());
    config.hooks = serde_json::from_value(serde_json::json!({
        "hooks": {"UnknownEvent": [{"hooks": [{"type":"command", "command":"true"}]}]}
    }))
    .unwrap();
    assert!(Harness::open(config, NamedModel::new("failed"))
        .await
        .is_err());
    let host = SessionHost::restore(
        &root,
        id,
        dir.path().into(),
        NamedModel::new("restored"),
        None,
        None,
        ContextPolicy::default(),
        "resume",
    )
    .await
    .unwrap();
    host.close(None).await.unwrap();
}
