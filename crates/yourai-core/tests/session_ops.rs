//! session_ops 模板的编排语义：消费顺序、结果应用顺序、错误聚合与门控。

use std::sync::Mutex;

use yourai_core::error::{ErrorKind, YourAiError};
use yourai_core::future::BoxFuture;
use yourai_core::hooks::{
    HookDispatchResult, HookEvent, HookEventKind, HookHost, HookMessage, HookMessageKind,
    HookPointOutcome, SessionStartOutcome,
};
use yourai_core::session_ops::{session_end, session_start, turn_completed, SessionStartSink};
use yourai_core::turn::TurnId;

struct MockHost {
    result: Mutex<Option<HookDispatchResult>>,
    dispatched: Mutex<Vec<HookEventKind>>,
    consumed_deny: Mutex<Vec<bool>>,
    fail: bool,
}

impl MockHost {
    fn new(result: Option<HookDispatchResult>) -> Self {
        Self {
            result: Mutex::new(result),
            dispatched: Mutex::new(vec![]),
            consumed_deny: Mutex::new(vec![]),
            fail: false,
        }
    }
    fn failing() -> Self {
        let mut host = Self::new(None);
        host.fail = true;
        host
    }
}

impl HookHost for MockHost {
    fn dispatch_hook(
        &self,
        event: HookEvent,
    ) -> BoxFuture<'_, Result<HookDispatchResult, YourAiError>> {
        let kind = event.kind();
        self.dispatched.lock().unwrap().push(kind);
        if self.fail {
            return Box::pin(async {
                Err(ErrorKind::Provider {
                    name: "hook",
                    message: "boom".into(),
                }
                .into())
            });
        }
        let result = self
            .result
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| HookDispatchResult::empty(kind));
        Box::pin(async move { Ok(result) })
    }
    fn consume_hook_result<'a>(
        &'a self,
        _result: &'a HookDispatchResult,
        deny_block: bool,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        self.consumed_deny.lock().unwrap().push(deny_block);
        Box::pin(async { Ok(()) })
    }
}

struct RecordingSink {
    calls: Mutex<Vec<String>>,
}

impl SessionStartSink for RecordingSink {
    fn watch_path(&self, path: std::path::PathBuf) -> BoxFuture<'_, Result<(), YourAiError>> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("watch:{}", path.display()));
        Box::pin(async { Ok(()) })
    }
    fn submit_user_text(&self, message: String) -> BoxFuture<'_, Result<(), YourAiError>> {
        self.calls.lock().unwrap().push(format!("submit:{message}"));
        Box::pin(async { Ok(()) })
    }
}

fn start_result(paths: Vec<&str>, initial: Option<&str>) -> HookDispatchResult {
    let mut result = HookDispatchResult::empty(HookEventKind::SessionStart);
    result.outcome = HookPointOutcome::SessionStart(SessionStartOutcome {
        initial_user_message: initial.map(String::from),
        watch_paths: paths.into_iter().map(String::from).collect(),
        ..Default::default()
    });
    result
}

#[tokio::test]
async fn session_start_consumes_then_applies_outcome_in_order() {
    let host = MockHost::new(Some(start_result(vec!["/tmp/a", "/tmp/b"], Some("hello"))));
    let sink = RecordingSink {
        calls: Mutex::new(vec![]),
    };
    session_start(&host, &sink, "startup", Some("model-x".into()))
        .await
        .unwrap();
    assert_eq!(
        host.dispatched.lock().unwrap().as_slice(),
        &[HookEventKind::SessionStart]
    );
    assert_eq!(host.consumed_deny.lock().unwrap().as_slice(), &[false]);
    assert_eq!(
        sink.calls.lock().unwrap().as_slice(),
        &[
            "watch:/tmp/a".to_string(),
            "watch:/tmp/b".to_string(),
            "submit:hello".to_string(),
        ]
    );
}

fn end_result(messages: Vec<HookMessage>) -> HookDispatchResult {
    let mut result = HookDispatchResult::empty(HookEventKind::SessionEnd);
    result.common.messages = messages;
    result
}

fn message(kind: HookMessageKind, content: &str) -> HookMessage {
    HookMessage {
        hook_id: "h1".into(),
        kind,
        content: content.into(),
    }
}

#[tokio::test]
async fn session_end_aggregates_non_blocking_errors() {
    let host = MockHost::new(Some(end_result(vec![
        message(HookMessageKind::Success, "all good"),
        message(HookMessageKind::NonBlockingError, "err-1"),
        message(HookMessageKind::NonBlockingError, "err-2"),
    ])));
    let hook_error = session_end(&host, None, "session-1", None).await.unwrap();
    assert_eq!(hook_error.as_deref(), Some("err-1\nerr-2"));
}

#[tokio::test]
async fn session_end_without_errors_reports_none() {
    let host = MockHost::new(Some(end_result(vec![message(
        HookMessageKind::Success,
        "fine",
    )])));
    let hook_error = session_end(&host, None, "session-1", None).await.unwrap();
    assert_eq!(hook_error, None);
}

#[tokio::test]
async fn session_end_captures_dispatch_failure_instead_of_propagating() {
    let host = MockHost::failing();
    let hook_error = session_end(&host, None, "session-1", None).await.unwrap();
    assert!(hook_error.is_some());
    assert!(hook_error.unwrap().contains("boom"));
}

#[tokio::test]
async fn turn_completed_is_gated_on_new_history_rows() {
    let host = MockHost::new(None);
    let notices = Mutex::new(vec![]);
    let recorder = |message: &str| notices.lock().unwrap().push(message.to_owned());
    turn_completed(&host, &TurnId::new(), 7, 7, &recorder).await;
    assert!(host.dispatched.lock().unwrap().is_empty());
    assert!(notices.lock().unwrap().is_empty());
}

#[tokio::test]
async fn turn_completed_forwards_all_visible_output_as_notices() {
    let mut result = HookDispatchResult::empty(HookEventKind::TurnCompleted);
    result.common.system_messages = vec!["sys".into()];
    result.common.blocking_errors = vec![yourai_core::hooks::HookBlockingError {
        hook_id: "h1".into(),
        message: "blocked".into(),
    }];
    result.common.messages = vec![message(HookMessageKind::Success, "visible")];
    let host = MockHost::new(Some(result));
    let notices = Mutex::new(vec![]);
    let recorder = |message: &str| notices.lock().unwrap().push(message.to_owned());
    turn_completed(&host, &TurnId::new(), 3, 9, &recorder).await;
    assert_eq!(
        notices.lock().unwrap().as_slice(),
        &[
            "visible".to_string(),
            "sys".to_string(),
            "blocked".to_string()
        ]
    );
}

#[tokio::test]
async fn turn_completed_downgrades_dispatch_failure_to_notice() {
    let host = MockHost::failing();
    let notices = Mutex::new(vec![]);
    let recorder = |message: &str| notices.lock().unwrap().push(message.to_owned());
    turn_completed(&host, &TurnId::new(), 3, 9, &recorder).await;
    let messages = notices.lock().unwrap();
    assert_eq!(messages.len(), 1);
    assert!(messages[0].contains("TurnCompleted hook failed"));
}
