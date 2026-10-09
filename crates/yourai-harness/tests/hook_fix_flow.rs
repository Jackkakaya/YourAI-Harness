#[path = "support/loop.rs"]
mod support;
use serde_json::json;
use std::{
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use yourai_core::{execution::ExecutionConfig, prelude::*};

struct TimedHooks {
    slow: HookEventKind,
    seen: Mutex<Vec<HookEventKind>>,
}
impl HookRuntime for TimedHooks {
    fn dispatch<'a>(
        &'a self,
        invocation: &'a HookInvocation,
    ) -> BoxFuture<'a, Result<HookDispatchResult, YourAiError>> {
        Box::pin(async move {
            let kind = invocation.event.kind();
            self.seen.lock().unwrap().push(kind);
            if kind == self.slow {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            let mut result = HookDispatchResult::empty(kind);
            if let HookPointOutcome::Elicitation(outcome) = &mut result.outcome {
                outcome.action = Some("accept".into());
                outcome.content = Some(json!({"answer":"valid"}));
            }
            Ok(result)
        })
    }
}
struct DirectElicitation {
    question: bool,
}
impl AgentLoop for DirectElicitation {
    fn run_turn<'a>(&'a self, context: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            let mut turn = Turn::open(context, ExecutionConfig::default()).await?;
            let kind = if self.question {
                InteractionKind::Question
            } else {
                InteractionKind::McpElicitation {
                    server_name: "test".into(),
                    elicitation_id: None,
                }
            };
            let mut request = InteractionRequest::new(
                "business",
                kind,
                json!({"message":"question", "requested_schema":{"type":"object", "required":["answer"]}}),
            );
            request.deadline = Some(Instant::now() + Duration::from_millis(20));
            let result = yourai_core::interaction::elicit(&mut turn, request)
                .await
                .map(|_| ());
            turn.finish(result).await
        })
    }
}
#[tokio::test]
async fn direct_elicitation_deadline_covers_both_hooks_and_the_reply() {
    for (question, slow) in [
        (false, HookEventKind::Elicitation),
        (false, HookEventKind::ElicitationResult),
        (true, HookEventKind::Notification),
    ] {
        let hooks = Arc::new(TimedHooks {
            slow,
            seen: Mutex::new(vec![]),
        });
        let agent = Agent::builder()
            .agent_loop(Arc::new(DirectElicitation { question }))
            .context_manager(Arc::new(support::History::default()))
            .hooks(hooks.clone())
            .build();
        let failure = tokio::time::timeout(
            Duration::from_secs(1),
            support::collect(agent.start(In::user_text("go")).unwrap()),
        )
        .await
        .expect("one absolute deadline must interrupt every interaction stage")
        .1
        .unwrap_err();
        assert!(
            failure.error.to_string().contains("timed out"),
            "{}",
            failure.error
        );
        let seen = hooks.seen.lock().unwrap();
        if !question {
            assert!(seen.contains(&slow));
            if slow == HookEventKind::Elicitation {
                assert!(!seen.contains(&HookEventKind::ElicitationResult));
            }
        }
    }
}

struct FailingJob(CommitStatus);
impl CompactionJob for FailingJob {
    fn run<'a>(
        self: Box<Self>,
        _: CompactionRequest,
        _: &'a dyn ModelProvider,
        _: Option<&'a dyn UsageTracker>,
        _: &'a CancellationToken,
        progress: Arc<AtomicU8>,
    ) -> BoxFuture<'a, Result<(CompactionResult, String), YourAiError>>
    where
        Self: 'a,
    {
        Box::pin(async move {
            progress.store(self.0 as u8, Ordering::Release);
            Err(ErrorKind::Provider {
                name: "replacement-context",
                message: "business error".into(),
            }
            .into())
        })
    }
}
struct FailingCompaction {
    history: support::History,
    progress: CommitStatus,
}
impl ContextManager for FailingCompaction {
    fn system_prompt(&self) -> String {
        self.history.system_prompt()
    }
    fn session_id(&self) -> &SessionId {
        self.history.session_id()
    }
    fn restore(&self) -> BoxFuture<'_, Result<(), YourAiError>> {
        self.history.restore()
    }
    fn append(&self, messages: Vec<StoredMessage>) -> BoxFuture<'_, Result<(), YourAiError>> {
        self.history.append(messages)
    }
    fn records(&self) -> Vec<StoredMessage> {
        self.history.records()
    }
    fn build_request(
        &self,
        tools: &[ToolDefinition],
        model: &dyn ModelProvider,
        suffix: &[ChatMessage],
    ) -> Result<ContextRequest, YourAiError> {
        self.history.build_request(tools, model, suffix)
    }
    fn prepare_compaction<'a>(
        &'a self,
        _: &'a CompactionRequest,
        _: &'a dyn ModelProvider,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<CompactionPlan<'a>, YourAiError>> {
        Box::pin(async move { Ok(CompactionPlan::Summary(Box::new(FailingJob(self.progress)))) })
    }
}
#[tokio::test]
async fn replacement_job_errors_report_the_confirmed_commit_stage() {
    for (progress, expected) in [
        (CommitStatus::Pending, "business error"),
        (CommitStatus::Started, "commit state unknown"),
        (CommitStatus::Committed, "summary committed"),
    ] {
        let context = Arc::new(FailingCompaction {
            history: Default::default(),
            progress,
        });
        let compact = Compactor::new(
            context.clone(),
            Arc::new(support::Model::new(vec![])),
            None,
            None,
            BaseInput::new(context.session_id().as_str(), ""),
        );
        let error = compact
            .exec(
                CompactionRequest::new(CompactionTrigger::Manual),
                &CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert!(
            error.to_string().contains("business error"),
            "keep the actual failure"
        );
        if matches!(progress, CommitStatus::Pending) {
            assert!(matches!(
                error,
                YourAiError::Error(ErrorKind::Provider {
                    name: "replacement-context",
                    ..
                })
            ));
        }
    }
}
