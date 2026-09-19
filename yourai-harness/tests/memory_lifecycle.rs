#[path = "../../yourai-loop/tests/support/mod.rs"]
mod support;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use support::{answer, Model};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use yourai_core::{context::DiscardSink, prelude::*};
use yourai_harness::*;

#[derive(Default)]
struct Provider {
    events: Mutex<Vec<String>>,
    turns: Mutex<Vec<CompletedMemoryTurn>>,
    compact: Mutex<Vec<ChatMessage>>,
    fail_sync: bool,
}
impl MemoryProvider for Provider {
    fn system_prompt_block<'a>(
        &'a self,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Option<String>, YourAiError>> {
        self.events.lock().unwrap().push("system".into());
        Box::pin(async { Ok(Some("STATIC PROVIDER GUIDANCE".into())) })
    }
    fn on_session_start<'a>(
        &'a self,
        _: &'a MemorySession,
        source: &'a str,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        self.events.lock().unwrap().push(format!("start:{source}"));
        Box::pin(async { Ok(()) })
    }
    fn on_session_end<'a>(
        &'a self,
        _: &'a MemorySession,
        reason: &'a str,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        self.events.lock().unwrap().push(format!("end:{reason}"));
        Box::pin(async { Ok(()) })
    }
    fn on_pre_compact<'a>(
        &'a self,
        _: &'a MemorySession,
        trigger: &'a str,
        messages: &'a [ChatMessage],
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        self.events
            .lock()
            .unwrap()
            .push(format!("compact:{trigger}"));
        *self.compact.lock().unwrap() = messages.to_vec();
        Box::pin(async { Ok(()) })
    }
    fn recall<'a>(
        &'a self,
        r: RecallRequest<'a>,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<RecalledMemory>, YourAiError>> {
        Box::pin(async move {
            Ok(vec![RecalledMemory {
                provider: "test".into(),
                id: "remembered".into(),
                content: format!("RECALLED:{}", r.query),
                updated_at: None,
            }])
        })
    }
    fn sync_turn<'a>(
        &'a self,
        turn: &'a CompletedMemoryTurn,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        self.events.lock().unwrap().push("sync".into());
        self.turns.lock().unwrap().push(turn.clone());
        Box::pin(async move {
            if self.fail_sync {
                Err(ErrorKind::Config("memory offline".into()).into())
            } else {
                Ok(())
            }
        })
    }
}
struct StopOnce(AtomicUsize);
impl HookHandler for StopOnce {
    fn execute<'a>(
        &'a self,
        _: &'a HookInvocation,
    ) -> BoxFuture<'a, Result<HookOutput, YourAiError>> {
        let first = self.0.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            Ok(HookOutput::Parsed(if first {
                serde_json::json!({"decision":"block", "reason":"continue once"})
            } else {
                serde_json::json!({})
            }))
        })
    }
}
async fn run(h: &Harness, text: &str) -> SessionTurn {
    h.host.submit(In::user_text(text)).unwrap();
    h.host
        .run_next(
            TurnLimits::default(),
            &DiscardSink,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap()
}
#[tokio::test]
async fn callbacks_are_auto_registered_scoped_and_sync_once_after_stop_settles() {
    let d = TempDir::new().unwrap();
    let provider = Arc::new(Provider::default());
    let mut config = HarnessConfig::new(d.path().join("sessions"), d.path().into());
    config.memory_provider = Some(provider.clone());
    config.memory_search_limit = 2;
    let h = Harness::open(
        config,
        Arc::new(Model::new(vec![
            answer("first"),
            answer("continued"),
            answer("second"),
        ])),
    )
    .await
    .unwrap();
    assert_eq!(
        *provider.events.lock().unwrap(),
        vec!["system", "start:startup"]
    );
    let stop = Arc::new(StopOnce(AtomicUsize::new(0)));
    HookRegistry::register(
        h.hooks.as_ref(),
        NativeHookRegistration {
            id: "continue-once".into(),
            event: HookEventKind::Stop,
            matcher: None,
            handler: stop.clone(),
            timeout: None,
            source: HookSource::Session,
            failure_policy: FailurePolicy::Open,
            once: false,
        },
    )
    .await
    .unwrap();
    let first = run(&h, "question-one").await;
    assert_eq!(first.result.unwrap().text, "continued");
    assert_eq!(stop.0.load(Ordering::SeqCst), 2);
    let second = run(&h, "question-two").await;
    second.result.unwrap();
    {
        let turns = provider.turns.lock().unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].turn_id, first.turn_id.to_string());
        assert_ne!(turns[0].turn_id, turns[1].turn_id);
        assert_eq!(turns[0].messages.len(), 3); // clean user and both model responses
        let first = serde_json::to_string(&turns[0].messages).unwrap();
        assert!(first.contains("question-one") && first.contains("continued"));
        assert!(!first.contains("RECALLED") && !first.contains("continue once"));
        let second = serde_json::to_string(&turns[1].messages).unwrap();
        assert!(second.contains("question-two") && !second.contains("question-one"));
    }
    h.hooks
        .dispatch(&HookInvocation::new(
            BaseInput::new("different-session", ""),
            HookEvent::SessionEnd {
                reason: "foreign".into(),
            },
        ))
        .await
        .unwrap();
    assert!(!provider
        .events
        .lock()
        .unwrap()
        .contains(&"end:foreign".into()));
    h.close().await.unwrap();
    h.close().await.unwrap();
    assert_eq!(
        provider
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.starts_with("end:"))
            .count(),
        1
    );
}
#[tokio::test]
async fn callback_errors_do_not_undo_a_completed_turn_and_failed_turns_do_not_sync() {
    let d = TempDir::new().unwrap();
    let provider = Arc::new(Provider {
        fail_sync: true,
        ..Default::default()
    });
    let mut config = HarnessConfig::new(d.path().join("sessions"), d.path().into());
    config.memory_provider = Some(provider.clone());
    let h = Harness::open(config, Arc::new(Model::new(vec![answer("success")])))
        .await
        .unwrap();
    assert_eq!(run(&h, "first").await.result.unwrap().text, "success");
    assert!(run(&h, "model now exhausted").await.result.is_err());
    assert_eq!(provider.turns.lock().unwrap().len(), 1);
    h.close().await.unwrap();
}

#[tokio::test]
async fn compact_callback_uses_clean_active_history_without_rebuilding_system() {
    let d = TempDir::new().unwrap();
    let root = d.path().join("sessions");
    let provider = Arc::new(Provider::default());
    let mut config = HarnessConfig::new(root.clone(), d.path().into());
    config.memory_provider = Some(provider.clone());
    config.context_policy.context_window = Some(32000);
    config.context_policy.keep_recent_tokens = 0;
    let h = Harness::open(config, Arc::new(Model::new(vec![])))
        .await
        .unwrap();
    let store = SessionCatalog::new(root).unwrap();
    let id = h.host.context().id;
    let mut user = StoredMessage::new(ChatMessage::user("CLEAN ".repeat(1000)));
    user.attach_recall(vec![RecalledMemory {
        provider: "test".into(),
        id: "recall".into(),
        content: "NOT NEW MEMORY".into(),
        updated_at: None,
    }]);
    store
        .append_messages(
            &id,
            vec![user, StoredMessage::new(ChatMessage::user("latest"))],
        )
        .await
        .unwrap();
    // Model has no summary completion implementation; the pre-compact callback
    // must still run at the boundary before the attempted summary call.
    assert!(h
        .host
        .compact(
            CompactionRequest::new(CompactionTrigger::Manual),
            &CancellationToken::new()
        )
        .await
        .is_err());
    assert!(provider
        .events
        .lock()
        .unwrap()
        .contains(&"compact:manual".into()));
    let messages = serde_json::to_string(&*provider.compact.lock().unwrap()).unwrap();
    assert!(messages.contains("CLEAN"));
    assert!(!messages.contains("NOT NEW MEMORY"));
    assert_eq!(
        provider
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|s| *s == "system")
            .count(),
        1
    );
    assert!(provider.turns.lock().unwrap().is_empty());
    h.close().await.unwrap();
}

#[derive(Default)]
struct WaitingProvider(Mutex<Option<CancellationToken>>);
impl MemoryProvider for WaitingProvider {
    fn on_session_start<'a>(
        &'a self,
        _: &'a MemorySession,
        source: &'a str,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async move {
            if source == "test-timeout" {
                *self.0.lock().unwrap() = Some(cancel.clone());
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    }
}
#[tokio::test]
async fn cancelling_hook_dispatch_cancels_provider_callback_token() {
    let d = TempDir::new().unwrap();
    let provider = Arc::new(WaitingProvider::default());
    let mut config = HarnessConfig::new(d.path().join("sessions"), d.path().into());
    config.memory_provider = Some(provider.clone());
    let h = Harness::open(config, Arc::new(Model::new(vec![])))
        .await
        .unwrap();
    let event = HookInvocation::new(
        BaseInput::new(h.host.context().id.0, ""),
        HookEvent::SessionStart {
            source: "test-timeout".into(),
            model: None,
        },
    );
    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(40),
        h.hooks.dispatch(&event)
    )
    .await
    .is_err());
    let cancel = provider.0.lock().unwrap().as_ref().unwrap().clone();
    // Aborting the dispatcher schedules its child handlers for cancellation.
    tokio::time::timeout(std::time::Duration::from_secs(1), cancel.cancelled())
        .await
        .expect("provider callback must be cancelled with its dispatcher");
    h.close().await.unwrap();
}

struct VetoCompleted;
impl HookHandler for VetoCompleted {
    fn execute<'a>(
        &'a self,
        _: &'a HookInvocation,
    ) -> BoxFuture<'a, Result<HookOutput, YourAiError>> {
        Box::pin(async {
            Ok(HookOutput::Parsed(
                serde_json::json!({"continue":false,"stopReason":"cannot undo completion"}),
            ))
        })
    }
}
#[tokio::test]
async fn completion_notification_cannot_veto_result_and_resume_gets_start_callback() {
    let d = TempDir::new().unwrap();
    let root = d.path().join("sessions");
    let provider = Arc::new(Provider::default());
    let mut config = HarnessConfig::new(root.clone(), d.path().into());
    config.memory_provider = Some(provider.clone());
    let h = Harness::open(config, Arc::new(Model::new(vec![answer("complete")])))
        .await
        .unwrap();
    HookRegistry::register(
        h.hooks.as_ref(),
        NativeHookRegistration {
            id: "try-veto-completed".into(),
            event: HookEventKind::TurnCompleted,
            handler: Arc::new(VetoCompleted),
            matcher: None,
            timeout: None,
            source: HookSource::Session,
            failure_policy: FailurePolicy::Closed,
            once: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(run(&h, "question").await.result.unwrap().text, "complete");
    let id = h.host.context().id;
    h.close().await.unwrap();
    drop(h);
    let mut config = HarnessConfig::new(root, d.path().into());
    config.memory_provider = Some(provider.clone());
    config.resume = Some(id);
    let h = Harness::open(config, Arc::new(Model::new(vec![])))
        .await
        .unwrap();
    assert!(provider
        .events
        .lock()
        .unwrap()
        .contains(&"start:resume".into()));
    assert_eq!(
        provider
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|s| *s == "system")
            .count(),
        1
    );
    h.close().await.unwrap();
}
