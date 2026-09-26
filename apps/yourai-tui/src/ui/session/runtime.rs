//! All producers for one session share its lifetime and its own output channel.
use crate::ui::state::View;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::mpsc,
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;
use yourai_core::prelude::*;
use yourai_harness::Harness;

type Stats = (Option<yourai_harness::runtime::ContextUsage>, Option<u64>);
pub(super) struct Runtime {
    pub h: Arc<Harness>,
    tx: mpsc::UnboundedSender<Out>,
    rx: mpsc::UnboundedReceiver<Out>,
    cancel: CancellationToken,
    driver: Option<JoinHandle<Result<(), YourAiError>>>,
    compact: Option<JoinHandle<Option<Usage>>>,
    stats: Option<JoinHandle<Stats>>,
    stats_at: Instant,
    titles: JoinSet<()>,
    limits: TurnLimits,
}
impl Runtime {
    pub fn new(h: Harness, limits: TurnLimits) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            h: Arc::new(h),
            tx,
            rx,
            cancel: CancellationToken::new(),
            driver: None,
            compact: None,
            stats: None,
            stats_at: Instant::now() - Duration::from_secs(2),
            titles: JoinSet::new(),
            limits,
        }
    }
    pub fn resume(&mut self) {
        if self.driver.is_none() {
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
        matches!(self.h.host.status(), SessionStatus::Idle)
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
    pub fn save_title(&mut self, title: String) {
        let sessions = self.h.sessions.clone();
        let id = self.h.host.context().id;
        let tx = self.tx.clone();
        self.titles.spawn(async move {
            let saved = async {
                let mut meta = sessions.load_session(&id).await?;
                if meta.title.is_none() {
                    meta.title = Some(title);
                    sessions.save_session(&meta).await?;
                }
                Result::<(), YourAiError>::Ok(())
            }
            .await;
            if let Err(e) = saved {
                let _ = tx.send(Out::Notice {
                    level: Level::Warning,
                    message: format!("Session title not saved: {e}"),
                });
            }
        });
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
                    format!("Execution failed: {e}. /continue retries pending inputs."),
                ),
                Err(e) => view.notice(Level::Error, format!("Driver failed: {e}")),
                _ => {}
            }
            view.settle();
        }
        let active = !matches!(
            self.h.host.status(),
            SessionStatus::Idle | SessionStatus::Closed
        );
        if active && !view.active {
            view.active = true;
            view.since = Some(Instant::now());
        }
        if !active && self.rx.is_empty() && (view.active || !view.asks_empty()) {
            view.idle();
        }
        if self.compact.as_ref().is_some_and(|t| t.is_finished()) {
            match self.compact.take().unwrap().await {
                Ok(Some(usage)) => view.replace_usage(usage),
                Err(e) => view.notice(Level::Error, format!("Compact failed: {e}")),
                _ => {}
            }
        }
        while self.titles.try_join_next().is_some() {}
        view.model_metrics = self.h.model_snapshot();
        if self.stats.is_none() && self.stats_at.elapsed() >= Duration::from_secs(1) {
            let host = self.h.host.clone();
            let usage = self.h.usage.clone();
            let id = host.context().id;
            self.stats = Some(tokio::spawn(async move {
                let context = tokio::task::spawn_blocking(move || host.context_usage().ok())
                    .await
                    .ok()
                    .flatten();
                let count = usage.session_usage(&id).await.ok().map(|u| u.request_count);
                (context, count)
            }));
            self.stats_at = Instant::now();
        }
        if self.stats.as_ref().is_some_and(|t| t.is_finished()) {
            let (context, count) = self.stats.take().unwrap().await.unwrap_or_default();
            view.context_usage = context;
            if let Some(count) = count {
                view.set_response_count(count);
            }
        }
    }
    pub async fn close(mut self) -> Result<Vec<In>, YourAiError> {
        self.cancel.cancel();
        self.h.host.interrupt();
        if let Some(task) = self.stats.take() {
            task.abort();
        }
        if let Some(task) = self.compact.take() {
            let _ = task.await;
        }
        if let Some(task) = self.driver.take() {
            let _ = task.await;
        }
        while self.titles.join_next().await.is_some() {}
        self.h.close().await
    }
}
