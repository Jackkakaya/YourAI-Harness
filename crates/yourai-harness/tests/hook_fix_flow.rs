#[path = "support/loop.rs"]
mod support;
use serde_json::json;
use std::{
    sync::{atomic::Ordering, Arc, Mutex},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use yourai_core::prelude::*;

struct TimedHooks {
    slow: HookEventKind,
    seen: Mutex<Vec<HookEventKind>>,
}
impl HookManager for TimedHooks {
    // This fixed-response fixture does not support dynamic registration.
    fn register<'a>(
        &'a self,
        _: yourai_core::hooks::NativeHookRegistration,
    ) -> yourai_core::BoxFuture<'a, Result<(), yourai_core::YourAiError>> {
        panic!("registration is not used by this test hook")
    }
    fn unregister<'a>(
        &'a self,
        _: &'a str,
    ) -> yourai_core::BoxFuture<'a, Result<bool, yourai_core::YourAiError>> {
        panic!("unregistration is not used by this test hook")
    }
    fn handler_ids<'a>(&'a self) -> yourai_core::BoxFuture<'a, Vec<String>> {
        panic!("registry inspection is not used by this test hook")
    }

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
            let mut turn = Turn::open(context).await?;
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
    fn policy(&self) -> ContextPolicy {
        self.history.policy()
    }
    fn estimate(
        &self,
        request: &ChatRequest,
        model: &dyn ModelProvider,
    ) -> Result<u64, YourAiError> {
        self.history.estimate(request, model)
    }
    fn save_summary<'a>(
        &'a self,
        _: &'a [StoredMessage],
        _sources: Vec<String>,
        _summary: StoredMessage,
        progress: std::sync::Arc<std::sync::atomic::AtomicU8>,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async move {
            progress.store(self.progress as u8, Ordering::Release);
            Err(ErrorKind::Provider {
                name: "replacement-context",
                message: "business error".into(),
            }
            .into())
        })
    }
}
#[tokio::test]
async fn replacement_commit_errors_report_the_confirmed_commit_stage() {
    for (progress, expected) in [
        (CommitStatus::Pending, "business error"),
        (CommitStatus::Started, "commit state unknown"),
        (CommitStatus::Committed, "summary committed"),
    ] {
        let context = Arc::new(FailingCompaction {
            history: Default::default(),
            progress,
        });
        context
            .append(vec![
                StoredMessage::new(ChatMessage::user("old history")),
                StoredMessage::new(ChatMessage::user("current request")),
            ])
            .await
            .unwrap();
        context.history.tokens.store(1000, Ordering::Release);
        let compact = Compactor::new(
            context.clone(),
            Arc::new(support::Model::new(vec![]).with_budget(12_000, 500)),
            None,
            None,
            BaseInput::new(context.session_id().as_str(), ""),
        );
        let result = compact
            .exec(
                CompactionRequest::new(CompactionTrigger::Manual),
                &CancellationToken::new(),
                None,
            )
            .await;
        if matches!(progress, CommitStatus::Committed) {
            let result = result.unwrap();
            assert!(result.compacted);
            let reason = result.stop_reason.unwrap();
            assert!(reason.contains(expected) && reason.contains("business error"));
            continue;
        }
        let error = result.unwrap_err();
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
