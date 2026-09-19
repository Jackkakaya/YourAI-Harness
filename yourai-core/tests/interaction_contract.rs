use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use yourai_core::context::DiscardSink;
use yourai_core::prelude::*;

struct RecordingInteraction(Mutex<Vec<InteractionRequest>>);

impl ToolInteraction for RecordingInteraction {
    fn request<'a>(
        &'a self,
        request: InteractionRequest,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(request);
            Ok(json!({"answer": "yes"}))
        })
    }
}

fn tool_context<'a>(
    cancel: &'a CancellationToken,
    interaction: Option<&'a dyn ToolInteraction>,
) -> ToolContext<'a> {
    ToolContext {
        call_id: "tool-1".into(),
        emit: &DiscardSink,
        cancel,
        security: None,
        sandbox: None,
        interaction,
    }
}

#[tokio::test]
async fn repeated_tool_questions_have_independent_ids_and_keep_call_identity() {
    let bridge = RecordingInteraction(Mutex::new(vec![]));
    let cancel = CancellationToken::new();
    let tc = tool_context(&cancel, Some(&bridge));
    for prompt in ["first", "second"] {
        let reply = tc
            .ask(InteractionKind::Question, json!({"prompt": prompt}))
            .await
            .unwrap();
        assert_eq!(reply, json!({"answer": "yes"}));
    }
    let requests = bridge.0.lock().unwrap();
    assert_ne!(requests[0].id, requests[1].id);
    assert!(requests.iter().all(|r| r.call_id == "tool-1"));
    assert_eq!(requests[1].payload, json!({"prompt": "second"}));
}

struct WaitingInteraction(Notify);

impl ToolInteraction for WaitingInteraction {
    fn request<'a>(
        &'a self,
        _request: InteractionRequest,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            self.0.notify_one();
            std::future::pending().await
        })
    }
}

#[tokio::test]
async fn tool_question_wait_is_cancellable_and_missing_bridge_fails_fast() {
    let cancel = CancellationToken::new();
    let missing = tool_context(&cancel, None);
    let err = missing
        .ask(InteractionKind::Question, json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, YourAiError::Error(ErrorKind::Config(_))));

    let bridge = WaitingInteraction(Notify::new());
    let tc = tool_context(&cancel, Some(&bridge));
    let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(tc.ask(InteractionKind::Question, json!({})), async {
            bridge.0.notified().await;
            cancel.cancel();
        })
    })
    .await
    .expect("cancellation must release the tool");
    assert!(matches!(
        result,
        Err(YourAiError::Aborted(AbortReason::Cancelled))
    ));
}
