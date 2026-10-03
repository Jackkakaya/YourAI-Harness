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
    submissions: Option<mpsc::UnboundedSender<In>>,
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
                if let Err(rejection) = host.submit_async(input).await {
                    let _ = events.send(Out::InputRejected { rejection });
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
    /// Explicit continuation can race the failed driver's final cleanup.
    pub fn continue_execution(&mut self) {
        self.resume_after_settle = self.driver.is_some() && self.h.host.last_error().is_some();
        self.resume();
    }
    pub fn resume(&mut self) {
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
            let result = h
                .host
                .compact(CompactionRequest::new(CompactionTrigger::Manual), &cancel)
                .await;
            let (level, message) = match result {
                Ok(r) => (
                    Level::Info,
                    format!(
                        "Context {:?}: {} -> {} estimated tokens. {}",
                        r.action, r.tokens_before, r.tokens_after, r.reason
                    ),
                ),
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
                    format!("Stopped: {reason}. Queued inputs remain; /continue resumes them."),
                ),
                Ok(Err(e)) => view.notice(
                    Level::Error,
                    format!("Execution failed: {e}. /continue resumes queued inputs or continues from saved history."),
                ),
                Err(e) => view.notice(Level::Error, format!("Driver failed: {e}")),
                _ => {}
            }
            view.settle();
            if self.resume_after_settle {
                self.resume();
            }
        }
        if self.h.host.last_error().is_none() {
            self.resume_after_settle = false;
        }
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
    use super::Runtime;
    use crate::ui::{session, state::View};
    use std::time::{Duration, Instant};
    use yourai_core::prelude::*;

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
        view.user("uncommitted preview", false);
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
