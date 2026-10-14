// Frontend projection regression tests; execution resides in the backend.
#[cfg(test)]
mod tests {
    use crate::ui::{session, state::View};
    use std::time::Duration;
    use yourai_core::prelude::*;
    async fn refresh(controller: &mut session::Controller, view: &mut View) {
        let revision = controller.backend.client().snapshot().revision;
        controller.backend.client().refresh();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                controller.poll(view, None).await;
                if controller.backend.client().snapshot().revision != revision {
                    controller.poll(view, None).await;
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn initial_snapshot_does_not_erase_restored_metadata_before_backend_refresh() {
        let (_dir, mut controller, mut view) = session::fixture().await;
        view.session.title = Some("restored title".into());
        view.set_response_count(12);
        controller.poll(&mut view, None).await;
        assert_eq!(view.session.title.as_deref(), Some("restored title"));
        assert_eq!(view.recorded_responses(), 12);
        controller.close().await.unwrap();
    }

    #[tokio::test]
    async fn title_refresh_reads_service_metadata_without_writing() {
        let (_dir, mut controller, mut view) = session::fixture().await;
        let h = controller.backend.harness();
        let context = h.host.environment();
        // A UI preview is not an admitted input and must never name a session.
        view.user("uncommitted preview");
        refresh(&mut controller, &mut view).await;
        assert!(view.session.title.is_none());
        h.sessions
            .append_messages(
                &context.id,
                vec![
                    StoredMessage::runtime_context("hook context"),
                    StoredMessage::new(ChatMessage::user("  Actual   request\nsecond line")),
                    StoredMessage::new(ChatMessage::user("later request")),
                ],
            )
            .await
            .unwrap();
        let db = rusqlite::Connection::open(context.transcript_path.unwrap()).unwrap();
        db.execute_batch("CREATE TRIGGER fail_title BEFORE UPDATE OF title ON sessions BEGIN SELECT RAISE(FAIL, 'title write unavailable'); END;").unwrap();
        refresh(&mut controller, &mut view).await;
        assert!(view.session.title.is_none());
        assert!(h
            .sessions
            .load_session(&context.id)
            .await
            .unwrap()
            .title
            .is_none());
        assert!(view.toast.is_none());
        db.execute_batch("DROP TRIGGER fail_title;").unwrap();
        // The session side publishes the title; frontend refresh only reads it.
        let mut meta = h.sessions.load_session(&context.id).await.unwrap();
        meta.title = Some("Actual request".into());
        h.sessions.save_session(&meta).await.unwrap();
        refresh(&mut controller, &mut view).await;
        assert_eq!(view.session.title.as_deref(), Some("Actual request"));
        assert_eq!(
            h.sessions.load_session(&context.id).await.unwrap().title,
            view.session.title
        );
        let mut restored = View::default();
        session::restore_history(&h, &mut restored).await.unwrap();
        assert_eq!(restored.session.title, view.session.title);
        controller.close().await.unwrap();
    }

    #[tokio::test]
    async fn closing_before_first_refresh_saves_committed_title() {
        let (_dir, controller, view) = session::fixture().await;
        let h = controller.backend.harness();
        let id = h.host.environment().id;
        h.sessions
            .append_messages(
                &id,
                vec![StoredMessage::new(ChatMessage::user("quick reply"))],
            )
            .await
            .unwrap();
        assert!(view.session.title.is_none());
        controller.close().await.unwrap();
        assert_eq!(
            h.sessions.load_session(&id).await.unwrap().title.as_deref(),
            Some("quick reply")
        );
    }

    #[tokio::test]
    async fn rejected_input_does_not_name_session() {
        let (_dir, mut controller, mut view) = session::fixture().await;
        let h = controller.backend.harness();
        let input = In::user_text_with_attachments(
            "rejected request",
            vec![UserAttachment::file("missing-file.png", None)],
        );
        assert!(controller.submit(input, &mut view));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                controller.poll(&mut view, None).await;
                if h.host.queued() == 0 && matches!(h.host.status(), SessionStatus::Idle) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        refresh(&mut controller, &mut view).await;
        assert!(view.session.title.is_none());
        assert!(h
            .sessions
            .load_session(&h.host.environment().id)
            .await
            .unwrap()
            .title
            .is_none());
        controller.close().await.unwrap();
    }
}

#[cfg(test)]
mod failure_cleanup_tests {
    use crate::ui::session;
    use std::{sync::Arc, time::Duration};
    use yourai_core::prelude::*;
    struct FailureCleanupGate {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    impl HookHandler for FailureCleanupGate {
        fn execute<'a>(
            &'a self,
            _: &'a HookInvocation,
        ) -> BoxFuture<'a, Result<HookOutput, YourAiError>> {
            Box::pin(async move {
                self.entered.notify_one();
                self.release.notified().await;
                Ok(HookOutput::Parsed(serde_json::json!({})))
            })
        }
    }
    struct ErrorModel(std::sync::atomic::AtomicUsize);
    impl ModelProvider for ErrorModel {
        fn model_iden(&self) -> &str {
            "review-error"
        }
        fn complete<'a>(
            &'a self,
            _: ModelRequest,
        ) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
            Box::pin(async { unreachable!() })
        }
        fn stream_events<'a>(
            &'a self,
            _: ModelRequest,
        ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async {
                Err(ErrorKind::Provider {
                    name: "model",
                    message: "failed".into(),
                }
                .into())
            })
        }
    }
    async fn cleanup_with_queued_input(cancel: bool) {
        let (_dir, mut controller, mut view) = session::fixture().await;
        let model = Arc::new(ErrorModel(std::sync::atomic::AtomicUsize::new(0)));
        let gate = Arc::new(FailureCleanupGate {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        controller
            .backend
            .harness()
            .switch_model(model.clone(), ContextPolicy::default())
            .await
            .unwrap();
        controller
            .backend
            .harness()
            .hooks
            .register(NativeHookRegistration {
                id: "review-cleanup-gate".into(),
                event: HookEventKind::StopFailure,
                matcher: None,
                handler: gate.clone(),
                timeout: Some(Duration::from_secs(10)),
                source: HookSource::Session,
                failure_policy: FailurePolicy::Closed,
                once: true,
            })
            .await
            .unwrap();
        assert!(controller.submit(In::follow_up("first"), &mut view));
        tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
            .await
            .unwrap();
        assert!(matches!(
            controller.backend.harness().host.status(),
            SessionStatus::Running { .. }
        ));
        assert!(controller.backend.harness().host.last_error().is_none());
        assert!(controller.submit(
            In::follow_up("new instruction during failure cleanup"),
            &mut view
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            while controller.backend.client().submitting() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if cancel {
            controller.backend.client().interrupt();
        }
        gate.release.notify_one();
        tokio::time::timeout(Duration::from_secs(5), async {
            let expected = if cancel { 1 } else { 2 };
            while model.0.load(std::sync::atomic::Ordering::SeqCst) < expected
                || matches!(
                    controller.backend.harness().host.status(),
                    SessionStatus::Running { .. }
                )
            {
                // Backend progress must not depend on frontend polling.
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let calls = model.0.load(std::sync::atomic::Ordering::SeqCst);
        controller.close().await.unwrap();
        assert_eq!(
            calls,
            if cancel { 1 } else { 2 },
            "a new message resumes after failure unless explicitly cancelled"
        );
    }
    #[tokio::test]
    async fn send_during_failure_cleanup_resumes_without_replaying_input() {
        cleanup_with_queued_input(false).await;
    }
    #[tokio::test]
    async fn cancel_during_failure_cleanup_leaves_queued_input_paused() {
        cleanup_with_queued_input(true).await;
    }
}
