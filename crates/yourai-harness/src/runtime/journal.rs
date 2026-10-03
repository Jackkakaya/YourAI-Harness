//! Durable host transitions. Writers serialize; readers never wait for filesystem I/O.
use super::*;
use std::ops::{Deref, DerefMut};

pub(super) struct Transaction<'a> {
    host: &'a SessionHost,
    _gate: std::sync::MutexGuard<'a, ()>,
    candidate: Journal,
}
impl Deref for Transaction<'_> {
    type Target = Journal;
    fn deref(&self) -> &Journal {
        &self.candidate
    }
}
impl DerefMut for Transaction<'_> {
    fn deref_mut(&mut self) -> &mut Journal {
        &mut self.candidate
    }
}
impl Transaction<'_> {
    /// A failed commit cannot discard inputs already owned by the host. Keep
    /// the candidate for close/recovery and quarantine uncertain execution.
    pub fn retain_for_recovery(&mut self, cause: &YourAiError) {
        self.candidate.last_error = Some(cause.to_string());
        self.host.closing.cancel();
        let mut live = self.host.live.lock().unwrap();
        live.journal = self.candidate.clone();
        live.status = SessionStatus::Closing;
        if let Some(cancel) = &live.cancel {
            cancel.cancel();
        }
    }

    pub fn restore_unstarted(&mut self, input: In) -> Result<(), YourAiError> {
        self.candidate.active.clear();
        self.candidate.queue.push_front(input);
        if let Err(error) = self.commit() {
            self.retain_for_recovery(&error);
            return Err(error);
        }
        Ok(())
    }
    pub fn commit(&self) -> Result<(), YourAiError> {
        // The transaction gate also covers the final unlock in finish_close.
        // An old host must never overwrite a journal owned by a reopened host.
        // Closing is still writable for durable cleanup and recovery.
        if self.host.status() == SessionStatus::Closed {
            return Err(error("host", "session closed"));
        }
        atomic_write(&self.host.dir.join("host.json"), &self.candidate)?;
        self.host.live.lock().unwrap().journal = self.candidate.clone();
        Ok(())
    }
}
impl SessionHost {
    pub(super) fn journal(&self) -> Transaction<'_> {
        let gate = self.journal_gate.lock().unwrap();
        let mut candidate = self.live.lock().unwrap().journal.clone();
        candidate.events = self.events.pending();
        Transaction {
            host: self,
            _gate: gate,
            candidate,
        }
    }
    /// Once started, an owned durable transition finishes even if its waiter leaves.
    pub(super) async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(Arc<Self>) -> T + Send + 'static,
    ) -> Result<T, YourAiError> {
        let host = self
            .self_ref
            .get()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| error("host", "host released"))?;
        tokio::task::spawn_blocking(move || work(host))
            .await
            .map_err(|e| error("host", e))
    }
    /// Durable acknowledgement without blocking an async executor. A cancelled
    /// waiter may have committed; never automatically resubmit that input.
    pub async fn submit_async(&self, input: In) -> Result<(), InputRejected> {
        let rejected = input.clone();
        self.blocking(move |host| host.submit(input))
            .await
            .map_err(|e| InputRejected {
                input: rejected,
                reason: e.to_string(),
            })?
    }
    pub async fn post_event_async(&self, event: RuntimeEvent) -> Result<bool, YourAiError> {
        self.blocking(move |host| host.post_event(event)).await?
    }
    pub async fn watch_path_async(&self, path: PathBuf) -> Result<(), YourAiError> {
        self.blocking(move |host| host.watch_path(path)).await?
    }
    pub(crate) async fn set_cwd_async(&self, cwd: PathBuf) -> Result<(), YourAiError> {
        self.blocking(move |host| host.set_cwd(cwd)).await?
    }
    pub(super) async fn persist(&self) -> Result<(), YourAiError> {
        self.blocking(|host| host.journal().commit()).await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Model;
    impl ModelProvider for Model {
        fn model_iden(&self) -> &str {
            "test"
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
            Box::pin(async { std::future::pending().await })
        }
    }
    async fn fixture() -> (tempfile::TempDir, crate::Harness) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::HarnessConfig::new(dir.path().join("sessions"), dir.path().into());
        config.system_prompt = Some("test".into());
        let harness = crate::Harness::open(config, Arc::new(Model)).await.unwrap();
        (dir, harness)
    }
    #[tokio::test]
    async fn closed_host_cannot_overwrite_reopened_session() {
        let (_dir, harness) = fixture().await;
        let old = harness.host.clone();
        harness.close().await.unwrap();
        let reopened = SessionHost::open(
            old.dir.clone(),
            old.context(),
            old.agent.clone(),
            HostConfig::default(),
            "resume",
        )
        .await
        .unwrap();
        reopened
            .submit_async(In::user_text("new owner input"))
            .await
            .unwrap();
        let path = old.dir.join("host.json");
        let saved = std::fs::read(&path).unwrap();

        assert!(old.watch_path_async(old.dir.join("stale")).await.is_err());
        assert!(old.set_cwd_async(old.dir.join("stale")).await.is_err());
        // Internal persistence must also reject a late write, independently
        // of the public mutation entry points' lifecycle checks.
        assert!(old.persist().await.is_err());
        assert!(old.try_operation().is_err());
        assert!(old
            .compact(
                CompactionRequest::new(CompactionTrigger::Manual),
                &CancellationToken::new(),
            )
            .await
            .is_err());
        assert_eq!(old.status(), SessionStatus::Closed);
        assert_eq!(std::fs::read(&path).unwrap(), saved);
        assert_eq!(reopened.close(None).await.unwrap().len(), 1);
    }
    async fn hold_writer(
        host: Arc<SessionHost>,
    ) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let (ready, wait) = oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let task = std::thread::spawn(move || {
            let _transaction = host.journal();
            ready.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        wait.await.unwrap();
        (release, task)
    }
    #[tokio::test(flavor = "current_thread")]
    async fn blocked_writer_keeps_control_responsive_and_dropped_submit_still_commits() {
        use futures_util::FutureExt;
        let (_dir, harness) = fixture().await;
        let host = harness.host.clone();
        let (release, task) = hold_writer(host.clone()).await;
        // Poll once to start the owned write, then abandon only its acknowledgement.
        assert!(host
            .submit_async(In::user_text("kept"))
            .now_or_never()
            .is_none());
        let start = std::time::Instant::now();
        assert_eq!(host.status(), SessionStatus::Idle);
        assert_eq!(host.queued(), 0);
        host.interrupt();
        assert!(start.elapsed() < Duration::from_millis(100));
        tokio::time::sleep(Duration::from_millis(5)).await;
        release.send(()).unwrap();
        task.join().unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while host.queued() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let saved: Journal = read_json(&host.dir.join("host.json")).unwrap();
        assert_eq!(saved.queue.len(), 1);
        assert_eq!(harness.close().await.unwrap().len(), 1);
    }
    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_start_is_supervised_and_close_recovers_unstarted_input() {
        let (_dir, harness) = fixture().await;
        let host = harness.host.clone();
        host.submit_async(In::user_text("kept")).await.unwrap();
        let (release, task) = hold_writer(host.clone()).await;
        let running = host.clone();
        let waiter = tokio::spawn(async move {
            running
                .run_next(
                    TurnLimits::default(),
                    &yourai_core::context::DiscardSink,
                    &CancellationToken::new(),
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        waiter.abort();
        let _ = waiter.await;
        release.send(()).unwrap();
        task.join().unwrap();
        let pending = tokio::time::timeout(Duration::from_secs(2), harness.close())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(host.status(), SessionStatus::Closed);
    }
    #[tokio::test(flavor = "current_thread")]
    async fn timed_out_close_retains_input_handoff_until_retry() {
        let (_dir, harness) = fixture().await;
        let host = harness.host.clone();
        host.submit_async(In::user_text("kept")).await.unwrap();
        let (release, writer) = hold_writer(host.clone()).await;
        assert!(host.close(Some(Duration::from_millis(20))).await.is_err());
        release.send(()).unwrap();
        writer.join().unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while host.status() != SessionStatus::Closed {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(harness.close().await.unwrap().len(), 1);
        assert!(harness.close().await.unwrap().is_empty());
    }
    #[tokio::test]
    async fn diagnostic_flush_failure_does_not_swallow_closed_inputs() {
        use futures_util::FutureExt;
        let (_dir, harness) = fixture().await;
        harness
            .host
            .submit_async(In::user_text("kept"))
            .await
            .unwrap();
        let connection =
            rusqlite::Connection::open(harness.host.context().transcript_path.unwrap()).unwrap();
        connection.execute("DROP TABLE model_requests", []).unwrap();
        let metered = crate::MeteredModel {
            inner: Arc::new(Model),
            budget: harness.budget.clone(),
        };
        assert!(metered
            .stream_events(ModelRequest::new(
                ChatRequest::from_user("test"),
                ChatOptions::default()
            ))
            .now_or_never()
            .is_none());
        assert_eq!(harness.close().await.unwrap().len(), 1);
        assert!(harness.budget.snapshot().requests.journal_errors > 0);
        assert!(harness
            .host
            .last_error()
            .unwrap()
            .contains("model_requests"));
        assert!(harness.close().await.unwrap().is_empty());
    }
    struct PendingLoop {
        entered: Notify,
        finish: Notify,
    }
    impl AgentLoop for PendingLoop {
        fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
            Box::pin(async move {
                let input = tc.inbox.recv().await.unwrap();
                self.entered.notify_one();
                self.finish.notified().await;
                let mut output = TurnOutput::new("");
                output.pending.push(input);
                Ok(output)
            })
        }
    }
    #[tokio::test]
    async fn failed_settlement_keeps_pending_for_close_recovery() {
        let (_dir, harness) = fixture().await;
        let host = harness.host.clone();
        let agent_loop = Arc::new(PendingLoop {
            entered: Notify::new(),
            finish: Notify::new(),
        });
        host.agent.ctx().set_agent_loop(agent_loop.clone());
        host.submit_async(In::user_text("retry me")).await.unwrap();
        let running = host.clone();
        let task = tokio::spawn(async move {
            running
                .run_next(
                    TurnLimits::default(),
                    &yourai_core::context::DiscardSink,
                    &CancellationToken::new(),
                )
                .await
        });
        agent_loop.entered.notified().await;
        let path = host.dir.join("host.json");
        let backup = path.with_extension("backup");
        std::fs::rename(&path, &backup).unwrap();
        std::fs::create_dir(&path).unwrap();
        agent_loop.finish.notify_one();
        let result = task.await.unwrap().unwrap().unwrap();
        assert!(result.result.is_err());
        assert_eq!(host.status(), SessionStatus::Closing);
        assert_eq!(host.queued(), 1);
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(backup, path).unwrap();
        assert_eq!(harness.close().await.unwrap().len(), 1);
    }
    #[tokio::test]
    async fn failed_start_rollback_keeps_unstarted_input_recoverable() {
        let (_dir, harness) = fixture().await;
        let host = harness.host.clone();
        host.submit_async(In::user_text("unstarted")).await.unwrap();
        let path = host.dir.join("host.json");
        let backup = path.with_extension("backup");
        {
            let mut transaction = host.journal();
            let first = transaction.queue.pop_front().unwrap();
            transaction.active.push(first.clone());
            transaction.commit().unwrap();
            std::fs::rename(&path, &backup).unwrap();
            std::fs::create_dir(&path).unwrap();
            assert!(transaction.restore_unstarted(first).is_err());
        }
        assert_eq!(host.status(), SessionStatus::Closing);
        assert_eq!(host.queued(), 1);
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(backup, path).unwrap();
        assert_eq!(harness.close().await.unwrap().len(), 1);
    }
}
