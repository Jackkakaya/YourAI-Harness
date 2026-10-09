//! All producers for one session share its lifetime and its own output channel.
use crate::ui::state::View;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use yourai_core::prelude::*;
use yourai_harness::Harness;

/// Keep the provider's diagnosis while removing transport JSON decoration.
fn summarize_error(text: &str) -> String {
    const MARKER: &str = "Response body:";
    let Some((head, body)) = text.split_once(MARKER) else {
        return text.to_owned();
    };
    let head = head.trim_end().trim_end_matches('.');
    let body = body.trim();
    let parsed = serde_json::from_str::<serde_json::Value>(body).ok();
    let message = parsed.as_ref().and_then(|value| {
        value
            .get("message")
            .and_then(serde_json::Value::as_str)
            .filter(|message| !message.trim().is_empty())
            .or_else(|| {
                value
                    .pointer("/error/message")
                    .and_then(serde_json::Value::as_str)
                    .filter(|message| !message.trim().is_empty())
            })
    });
    let detail = message
        .map(str::to_owned)
        .unwrap_or_else(|| body.split_whitespace().collect::<Vec<_>>().join(" "));
    if detail.is_empty() {
        head.to_owned()
    } else {
        format!("{head}: {}", crate::text::elide(&detail, 512))
    }
}

enum InputAction {
    Submit(In),
    Steer(String),
}

type Stats = (
    Option<yourai_harness::runtime::ContextUsage>,
    Option<u64>,
    Result<Option<String>, YourAiError>,
);
pub(super) struct Runtime {
    pub h: Arc<Harness>,
    tx: mpsc::UnboundedSender<Out>,
    rx: mpsc::UnboundedReceiver<Out>,
    cancel: CancellationToken,
    driver: Option<JoinHandle<Result<(), YourAiError>>>,
    restart_requested: bool,
    compact: Option<JoinHandle<Option<Usage>>>,
    stats: Option<JoinHandle<Stats>>,
    stats_at: Instant,
    limits: TurnLimits,
    submissions: Option<mpsc::UnboundedSender<InputAction>>,
    submitter: JoinHandle<()>,
    pending_submissions: Arc<std::sync::atomic::AtomicUsize>,
}
impl Runtime {
    pub fn new(h: Harness, limits: TurnLimits) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let h = Arc::new(h);
        let (submissions, mut inputs) = mpsc::unbounded_channel();
        let pending_submissions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pending = pending_submissions.clone();
        let host = h.host.clone();
        let events = tx.clone();
        let submitter = tokio::spawn(async move {
            while let Some(input) = inputs.recv().await {
                match input {
                    InputAction::Submit(input) => {
                        if let Err(rejection) = host.submit_async(input).await {
                            let _ = events.send(Out::InputRejected { rejection });
                        }
                    }
                    InputAction::Steer(id) => match host.steer_pending(id).await {
                        Ok(true) => {}
                        result => {
                            let message = match result {
                                    Ok(false) => "Message is no longer pending or the turn ended; nothing was resent.".into(),
                                    Err(e) => format!("Could not steer queued message: {e}"),
                                    Ok(true) => unreachable!(),
                                };
                            let _ = events.send(Out::Notice {
                                level: Level::Warning,
                                message,
                            });
                        }
                    },
                }
                pending.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        Self {
            h,
            submissions: Some(submissions),
            submitter,
            pending_submissions,
            tx,
            rx,
            cancel: CancellationToken::new(),
            driver: None,
            restart_requested: false,
            compact: None,
            stats: None,
            stats_at: Instant::now() - Duration::from_secs(2),
            limits,
        }
    }
    /// Queue in input order; durable rejection returns through the normal event reducer.
    pub fn submit(&mut self, input: In) -> bool {
        let accepted = self.dispatch(InputAction::Submit(input));
        self.restart_requested |= accepted;
        accepted
    }
    pub fn steer_pending(&mut self, id: String) {
        self.dispatch(InputAction::Steer(id));
    }
    pub fn interrupt(&mut self) {
        self.restart_requested = false;
        self.h.host.interrupt();
    }
    fn dispatch(&mut self, input: InputAction) -> bool {
        self.pending_submissions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self
            .submissions
            .as_ref()
            .is_some_and(|tx| tx.send(input).is_ok())
        {
            true
        } else {
            self.pending_submissions
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            false
        }
    }
    pub fn submitting(&self) -> bool {
        self.pending_submissions
            .load(std::sync::atomic::Ordering::SeqCst)
            != 0
    }
    pub fn resume(&mut self) {
        if self.driver.is_none() {
            self.restart_requested = false;
            let host = self.h.host.clone();
            let tx = self.tx.clone();
            let cancel = self.cancel.clone();
            let limits = self.limits.clone();
            self.driver = Some(tokio::spawn(async move {
                host.serve(limits, &tx, &cancel).await
            }));
        }
    }
    pub fn compacting(&self) -> bool {
        self.compact.is_some()
    }
    pub async fn recv(&mut self) -> Option<Out> {
        self.rx.recv().await
    }
    pub fn idle(&self) -> bool {
        !self.submitting()
            && matches!(self.h.host.status(), SessionStatus::Idle)
            && !self.compacting()
            && self.h.host.queued() == 0
    }
    pub fn compact(&mut self) {
        let h = self.h.clone();
        let tx = self.tx.clone();
        let cancel = self.cancel.clone();
        self.compact = Some(tokio::spawn(async move {
            let result = h
                .host
                .compact(CompactionRequest::new(CompactionTrigger::Manual), &cancel)
                .await;
            let (level, message) = match result {
                Ok(r) => {
                    for notice in &r.notices {
                        let _ = tx.send(Out::Notice {
                            level: Level::Warning,
                            message: notice.clone(),
                        });
                    }
                    (
                        Level::Info,
                        format!(
                            "Context {:?}: {} -> {} estimated tokens. {}",
                            r.action, r.tokens_before, r.tokens_after, r.reason
                        ),
                    )
                }
                Err(e) => (Level::Error, format!("Compact failed: {e}")),
            };
            let _ = tx.send(Out::Notice { level, message });
            h.usage
                .session_usage(&h.host.context().id)
                .await
                .ok()
                .map(|u| Usage {
                    input_tokens: u.total_input_tokens,
                    output_tokens: u.total_output_tokens,
                    total_tokens: u.total_tokens,
                })
        }));
    }
    /// Only finished tasks are awaited here; storage and model work never hold
    /// the UI loop. Drain output before settling stream identities.
    pub async fn poll(&mut self, view: &mut View, first: Option<Out>) {
        if let Some(event) = first {
            view.event(event);
        }
        for _ in 0..256 {
            match self.rx.try_recv() {
                Ok(event) => view.event(event),
                Err(_) => break,
            }
        }
        if self.driver.as_ref().is_some_and(|t| t.is_finished()) && self.rx.is_empty() {
            match self.driver.take().unwrap().await {
                Ok(Err(YourAiError::Aborted(reason))) => view.notice(
                    Level::Info,
                    format!("Stopped: {reason}. Queued inputs remain; send a message to resume."),
                ),
                Ok(Err(e)) => view.notice(
                    Level::Error,
                    format!(
                        "Execution failed: {}. Send a message to resume.",
                        summarize_error(&e.to_string())
                    ),
                ),
                Err(e) => view.notice(Level::Error, format!("Driver failed: {e}")),
                _ => {}
            }
            view.settle();
        }
        // A send requests continuation before submission/failed-turn cleanup finishes.
        // Do not decide this using last_error or the transient Running status.
        if self.driver.is_none()
            && self.restart_requested
            && !self.submitting()
            && matches!(self.h.host.status(), SessionStatus::Idle)
            && !self.compacting()
        {
            if self.h.host.queued() > 0 {
                self.resume();
            } else {
                self.restart_requested = false;
            }
        }
        view.session.pending_inputs = self.h.host.pending_inputs();
        view.session.can_steer = matches!(self.h.host.status(), SessionStatus::Running { .. });
        let active = self.submitting()
            || !matches!(
                self.h.host.status(),
                SessionStatus::Idle | SessionStatus::Closed
            );
        if active && !view.session.active {
            view.session.active = true;
            view.session.since = Some(Instant::now());
        }
        if !active && self.rx.is_empty() && (view.session.active || !view.asks_empty()) {
            view.idle();
        }
        if self.compact.as_ref().is_some_and(|t| t.is_finished()) {
            match self.compact.take().unwrap().await {
                Ok(Some(usage)) => view.replace_usage(usage),
                Err(e) => view.notice(Level::Error, format!("Compact failed: {e}")),
                _ => {}
            }
        }
        view.session.model_metrics = self.h.model_snapshot();
        if self.stats.is_none() && self.stats_at.elapsed() >= Duration::from_secs(1) {
            let host = self.h.host.clone();
            let usage = self.h.usage.clone();
            let id = host.context().id;
            let h = self.h.clone();
            let title = view.session.title.clone();
            self.stats = Some(tokio::spawn(async move {
                let context = tokio::task::spawn_blocking(move || host.context_usage().ok())
                    .await
                    .ok()
                    .flatten();
                let count = usage.session_usage(&id).await.ok().map(|u| u.request_count);
                let title = match title {
                    Some(title) => Ok(Some(title)),
                    None => super::setup::restore_title(&h).await,
                };
                (context, count, title)
            }));
            self.stats_at = Instant::now();
        }
        if self.stats.as_ref().is_some_and(|t| t.is_finished()) {
            let (context, count, title) = match self.stats.take().unwrap().await {
                Ok(stats) => stats,
                Err(e) => {
                    view.toast = Some((format!("Session refresh failed: {e}"), Instant::now()));
                    return;
                }
            };
            view.session.context_usage = context;
            match title {
                Ok(title) => view.session.title = title,
                Err(e) => {
                    view.toast = Some((format!("Session title unavailable: {e}"), Instant::now()))
                }
            }
            if let Some(count) = count {
                view.set_response_count(count);
            }
        }
    }
    pub async fn close(mut self) -> Result<Vec<In>, YourAiError> {
        self.cancel.cancel();
        self.h.host.interrupt();
        self.submissions.take();
        let _ = self.submitter.await;
        if let Some(task) = self.stats.take() {
            let _ = task.await;
        }
        if let Some(task) = self.compact.take() {
            let _ = task.await;
        }
        if let Some(task) = self.driver.take() {
            let _ = task.await;
        }
        // A fast reply followed by immediate quit/switch may precede the first
        // refresh. Finish title backfill from committed history before closing.
        // A title failure must not discard pending inputs returned by close;
        // the persisted messages allow the same backfill on the next open.
        let _ = super::setup::restore_title(&self.h).await;
        self.h.close().await
    }
}

#[cfg(test)]
mod tests {
    use super::{summarize_error, Runtime};
    use crate::ui::{session, state::View};
    use std::time::{Duration, Instant};
    use yourai_core::prelude::*;
    #[test]
    fn error_summary_preserves_nested_flat_and_unstructured_diagnoses() {
        for body in [
            r#"{"error":{"message":"maximum context length exceeded"}}"#,
            r#"{"message":"maximum context length exceeded"}"#,
            r#"{"message":null,"error":{"message":"maximum context length exceeded"}}"#,
            "maximum context length exceeded",
        ] {
            for separator in [" ", "\n"] {
                let raw = format!("HTTP 400. Response body:{separator}{body}");
                let summary = summarize_error(&raw);
                assert!(
                    summary.contains("maximum context length exceeded"),
                    "{summary}"
                );
                assert!(!summary.contains("Response body:"));
            }
        }
        assert!(
            summarize_error(r#"HTTP 400. Response body: {"detail":"invalid parameter"}"#)
                .contains("invalid parameter")
        );
        assert_eq!(summarize_error("plain failure"), "plain failure");
    }

    async fn refresh(runtime: &mut Runtime, view: &mut View) {
        runtime.stats_at = Instant::now() - Duration::from_secs(2);
        runtime.poll(view, None).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while runtime.stats.is_some() {
                tokio::task::yield_now().await;
                runtime.poll(view, None).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn title_write_failure_retries_from_committed_history_and_restores_consistently() {
        let (_dir, mut controller, mut view) = session::fixture().await;
        let h = controller.harness();
        let context = h.host.context();
        // A UI preview is not an admitted input and must never name a session.
        view.user("uncommitted preview");
        refresh(&mut controller.runtime, &mut view).await;
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
        refresh(&mut controller.runtime, &mut view).await;
        assert!(view.session.title.is_none());
        assert!(h
            .sessions
            .load_session(&context.id)
            .await
            .unwrap()
            .title
            .is_none());
        assert!(view
            .toast
            .as_ref()
            .unwrap()
            .0
            .contains("title write unavailable"));
        db.execute_batch("DROP TRIGGER fail_title;").unwrap();
        refresh(&mut controller.runtime, &mut view).await;
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
        let h = controller.harness();
        let id = h.host.context().id;
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
        let h = controller.harness();
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
        refresh(&mut controller.runtime, &mut view).await;
        assert!(view.session.title.is_none());
        assert!(h
            .sessions
            .load_session(&h.host.context().id)
            .await
            .unwrap()
            .title
            .is_none());
        controller.close().await.unwrap();
    }
}

#[cfg(test)]
mod failure_cleanup_tests {
    use super::*;
    use crate::ui::session;
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
            .runtime
            .h
            .switch_model(model.clone(), ContextPolicy::default())
            .await
            .unwrap();
        controller
            .runtime
            .h
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
            controller.runtime.h.host.status(),
            SessionStatus::Running { .. }
        ));
        assert!(controller.runtime.h.host.last_error().is_none());
        assert!(controller.submit(
            In::follow_up("new instruction during failure cleanup"),
            &mut view
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            while controller.runtime.submitting() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if cancel {
            controller.interrupt();
        }
        gate.release.notify_one();
        tokio::time::timeout(Duration::from_secs(5), async {
            while controller.runtime.driver.is_some() {
                controller.poll(&mut view, None).await;
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
