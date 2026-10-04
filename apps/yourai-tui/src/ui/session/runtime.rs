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
    resume_after_settle: bool,
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
                    InputAction::Steer(id) => {
                        let result = host.steer_pending(id).await;
                        if !matches!(result, Ok(true)) {
                            let message = match result {
                                Ok(false) => "Message is no longer pending, or the turn has ended; nothing was resent.".into(),
                                Err(e) => format!("Could not steer queued message: {e}"),
                                _ => unreachable!(),
                            };
                            let _ = events.send(Out::Notice {
                                level: Level::Warning,
                                message,
                            });
                        }
                    }
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
            resume_after_settle: false,
            compact: None,
            stats: None,
            stats_at: Instant::now() - Duration::from_secs(2),
            limits,
        }
    }
    /// Queue in input order; durable rejection returns through the normal event reducer.
    pub fn submit(&mut self, input: In) -> bool {
        self.dispatch(InputAction::Submit(input))
    }
    pub fn steer_pending(&mut self, id: String) {
        self.dispatch(InputAction::Steer(id));
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
        if self.driver.as_ref().is_some_and(|t| t.is_finished())
            || (self.driver.is_some()
                && self.h.host.last_error().is_some()
                && matches!(self.h.host.status(), SessionStatus::Idle))
        {
            self.resume_after_settle = true;
        }
        if self.driver.is_none() {
            self.resume_after_settle = false;
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
            let _ = h
                .host
                .compact_with_events(
                    CompactionRequest::new(CompactionTrigger::Manual),
                    &cancel,
                    &tx,
                )
                .await;
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
        let context_revision = view.session.context_revision;
        if let Some(event) = first {
            view.event(event);
        }
        for _ in 0..256 {
            match self.rx.try_recv() {
                Ok(event) => view.event(event),
                Err(_) => break,
            }
        }
        if context_revision != view.session.context_revision {
            if let Some(stats) = self.stats.take() {
                stats.abort();
            }
            self.stats_at = Instant::now() - Duration::from_secs(2);
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
                        yourai_harness::model::failure::diagnostic(&e)
                    ),
                ),
                Err(e) => view.notice(Level::Error, format!("Driver failed: {e}")),
                _ => {}
            }
            view.settle();
            if self.resume_after_settle {
                self.resume();
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
        if view.session.compaction.is_none()
            && self.stats.is_none()
            && self.stats_at.elapsed() >= Duration::from_secs(1)
        {
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
    use super::Runtime;
    use crate::ui::{session, state::View};
    use std::time::{Duration, Instant};
    use yourai_core::prelude::*;

    struct FailingModel(std::sync::Mutex<Vec<ChatRequest>>);
    impl ModelProvider for FailingModel {
        fn model_iden(&self) -> &str {
            "failing"
        }
        fn complete<'a>(
            &'a self,
            _: ModelRequest,
        ) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
            Box::pin(async { unreachable!() })
        }
        fn stream_events<'a>(
            &'a self,
            request: ModelRequest,
        ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
            self.0.lock().unwrap().push(request.request);
            Box::pin(async { Err(ErrorKind::Config("test failure".into()).into()) })
        }
    }
    #[tokio::test]
    async fn new_submission_resumes_after_failure_even_before_driver_is_reaped() {
        let (_dir, mut controller, mut view) = session::fixture().await;
        let model = std::sync::Arc::new(FailingModel(std::sync::Mutex::new(vec![])));
        controller
            .runtime
            .h
            .switch_model(model.clone(), ContextPolicy::default())
            .await
            .unwrap();
        assert!(controller.submit(In::follow_up("first"), &mut view));
        tokio::time::timeout(Duration::from_secs(5), async {
            while controller.runtime.h.host.last_error().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Deliberately submit before polling/reaping the old driver.
        assert!(controller.submit(In::follow_up("continue with this instruction"), &mut view));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                controller.poll(&mut view, None).await;
                if model.0.lock().unwrap().len() == 2 && controller.runtime.driver.is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        {
            let requests = model.0.lock().unwrap();
            let text = requests[1]
                .messages
                .iter()
                .flat_map(|m| m.content.texts())
                .collect::<Vec<_>>();
            assert_eq!(text.iter().filter(|t| **t == "first").count(), 1);
            assert_eq!(
                text.iter()
                    .filter(|t| **t == "continue with this instruction")
                    .count(),
                1
            );
            assert!(!text
                .iter()
                .any(|t| t.starts_with("Continue the previous task")));
        }
        controller.close().await.unwrap();
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
    async fn compaction_completion_discards_a_ready_but_stale_stats_snapshot() {
        let (_dir, mut controller, mut view) = session::fixture().await;
        let old = yourai_harness::runtime::ContextUsage {
            estimated_tokens: 999_999,
            context_window: Some(128_000),
            input_budget: Some(120_000),
            output_reserve: 4096,
        };
        view.session.context_usage = Some(old.clone());
        controller.runtime.stats = Some(tokio::spawn(async move { (Some(old), None, Ok(None)) }));
        tokio::task::yield_now().await;
        let mut result = CompactionResult::new(CompactAction::Summarized, 999_999, 500, "saved");
        result.verified = true;
        result.input_budget = Some(120_000);
        controller
            .runtime
            .poll(
                &mut view,
                Some(Out::Compaction {
                    event: CompactionEvent::Finished {
                        trigger: CompactionTrigger::Manual,
                        result,
                    },
                }),
            )
            .await;
        assert_ne!(
            view.session
                .context_usage
                .as_ref()
                .map(|u| u.estimated_tokens),
            Some(999_999)
        );
        assert!(view.session.compaction.is_none());
        controller.close().await.unwrap();
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
