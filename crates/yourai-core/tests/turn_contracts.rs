use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Notify;
use yourai_core::prelude::*;

struct ExitingLoop {
    entered: Arc<Notify>,
    resume: Arc<Notify>,
    fail: bool,
}

impl AgentLoop for ExitingLoop {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            let _ = tc.inbox.recv().await;
            self.entered.notify_one();
            self.resume.notified().await;
            let mut output = TurnOutput::new("partial response");
            output.usage = Some(Usage {
                input_tokens: 5,
                output_tokens: 3,
                total_tokens: 8,
            });
            // 已取走但未处理的输入归 Loop 负责；下一条仍留在通道供 Core 回收。
            output.pending.push(tc.inbox.recv().await.unwrap());
            if self.fail {
                Err(TurnFailure::new(AbortReason::Cancelled, output))
            } else {
                Ok(output)
            }
        })
    }
}

#[tokio::test]
async fn completion_preserves_stashed_and_unread_input_on_both_paths() {
    for fail in [false, true] {
        let entered = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        let agent = Agent::builder()
            .agent_loop(Arc::new(ExitingLoop {
                entered: entered.clone(),
                resume: resume.clone(),
                fail,
            }))
            .build();
        let mut handle = agent.start(In::user_text("first")).unwrap();
        entered.notified().await;
        let sender = handle.inbox.clone();
        sender.send(In::follow_up("stashed")).unwrap();
        sender.send(In::user_text("unread")).unwrap();
        resume.notify_one();

        assert!(handle.outbox.recv().await.is_none());
        let rejected = sender.send(In::user_text("late")).unwrap_err();
        assert!(matches!(rejected.0, In::UserText { text, .. } if text == "late"));
        let result = handle.join().await;
        let output = if fail {
            let failure = result.unwrap_err();
            assert!(matches!(
                *failure.error,
                YourAiError::Aborted(AbortReason::Cancelled)
            ));
            failure.output
        } else {
            result.unwrap()
        };
        assert_eq!(output.text, "partial response");
        assert_eq!(output.usage.unwrap().total_tokens, 8);
        assert_eq!(output.pending.len(), 2);
        assert!(
            matches!(&output.pending[0], In::UserText { text, mode: InputMode::FollowUp, .. } if text == "stashed")
        );
        assert!(
            matches!(&output.pending[1], In::UserText { text, mode: InputMode::Steer, .. } if text == "unread")
        );
    }
}

struct MetadataLoop;

impl AgentLoop for MetadataLoop {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            tc.check_control()?;
            let _ = tc.inbox.recv().await;
            let session = tc.info.options.session.as_ref().unwrap();
            assert_eq!(tc.info.options.limits.max_model_calls, Some(4));
            Ok(TurnOutput::new(format!("{}:{}", session.id, tc.info.id)))
        })
    }
}

#[tokio::test]
async fn turn_metadata_reaches_loop_and_deadline_failure_retains_input() {
    let session = Arc::new(SessionContext::new(SessionId::new(), "/workspace"));
    let mut options = TurnOptions::default();
    options.session = Some(session.clone());
    options.limits.max_model_calls = Some(4);
    let agent = Agent::builder().agent_loop(Arc::new(MetadataLoop)).build();
    let handle = agent
        .start_with(In::user_text("hello"), options.clone())
        .unwrap();
    let expected = format!("{}:{}", session.id, handle.info.id);
    assert_eq!(handle.join().await.unwrap().text, expected);

    options.limits.deadline = Some(Instant::now());
    let failure = agent
        .run_with(In::user_text("not processed"), options)
        .await
        .unwrap_err();
    assert!(matches!(
        *failure.error,
        YourAiError::Aborted(AbortReason::DeadlineExceeded)
    ));
    assert!(
        matches!(&failure.output.pending[..], [In::UserText { text, .. }] if text == "not processed")
    );
}

#[tokio::test]
async fn preflight_failure_in_noninteractive_run_returns_first_input() {
    let failure = Agent::builder()
        .build()
        .run(In::user_text("keep me"))
        .await
        .unwrap_err();
    assert!(matches!(
        *failure.error,
        YourAiError::Error(ErrorKind::Config(_))
    ));
    assert!(
        matches!(&failure.output.pending[..], [In::UserText { text, .. }] if text == "keep me")
    );
}

struct AskingLoop;

impl AgentLoop for AskingLoop {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            tc.outbox.send(Out::Ask {
                id: "request".into(),
                payload: serde_json::json!({}),
            });
            let mut output = TurnOutput::new("before asking");
            output.usage = Some(Usage {
                input_tokens: 1,
                output_tokens: 2,
                total_tokens: 3,
            });
            Err(TurnFailure::new(AbortReason::Cancelled, output))
        })
    }
}

#[tokio::test]
async fn noninteractive_ask_reclassification_preserves_partial_output() {
    let agent = Agent::builder().agent_loop(Arc::new(AskingLoop)).build();
    let failure = agent.run(In::user_text("unread")).await.unwrap_err();
    assert!(matches!(
        *failure.error,
        YourAiError::Error(ErrorKind::Config(_))
    ));
    assert_eq!(failure.output.text, "before asking");
    assert_eq!(failure.output.usage.unwrap().total_tokens, 3);
    assert_eq!(failure.output.pending.len(), 1);
}
