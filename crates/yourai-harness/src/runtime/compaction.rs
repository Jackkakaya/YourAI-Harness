//! Manual maintenance uses the same lifecycle as automatic maintenance.
use super::*;

impl SessionHost {
    pub fn compact_with_events<'a>(
        &'a self,
        mut request: CompactionRequest,
        cancel: &'a CancellationToken,
        events: &'a dyn OutSink,
    ) -> BoxFuture<'a, Result<CompactionResult, YourAiError>> {
        Box::pin(async move {
            let started = std::sync::atomic::AtomicBool::new(false);
            let result = async {
                let _gate = self.try_operation()?;
                let token = cancel.child_token();
                {
                    let mut live = self.live.lock().unwrap();
                    // Publish under the same lock as close so a racing close can
                    // never have its Closing state overwritten by Compacting.
                    self.ensure_open()?;
                    live.status = SessionStatus::Compacting;
                    live.cancel = Some(token.clone());
                }
                let _status = StatusGuard(self);
                let deadline = request.deadline;
                let _cancel_guard = token.clone().drop_guard();
                let task = async {
                    request.trigger = CompactionTrigger::Manual;
                    let snapshot = self.agent.ctx().snapshot()?;
                    let context = self.context();
                    let history = snapshot
                        .context_manager
                        .clone()
                        .ok_or_else(|| error("compact", "context not configured"))?;
                    tokio::select! {
                        biased;
                        _ = token.cancelled() => return Err(AbortReason::Cancelled.into()),
                        _ = crate::time::sleep_until(deadline) => return Err(AbortReason::DeadlineExceeded.into()),
                        result = history.restore() => result?,
                    }
                    request.tools = snapshot
                        .tools
                        .as_ref()
                        .map(|r| r.definitions())
                        .unwrap_or_default();
                    let execution = ContextExecution::from_snapshot(
                        &snapshot,
                        history.session_id(),
                        Some(&context),
                    )?;
                    started.store(true, std::sync::atomic::Ordering::Release);
                    crate::context::compact_with_events(
                        history.as_ref(),
                        request,
                        &execution,
                        &token,
                        self.config.hook_timeout,
                        events,
                    )
                    .await
                };
                tokio::pin!(task);
                tokio::select! {
                    result = &mut task => result,
                    _ = self.closing.cancelled() => {
                        token.cancel();
                        task.await
                    }
                }
            }
            .await;
            if let Err(e) = &result {
                if !started.load(std::sync::atomic::Ordering::Acquire) {
                    events.send(Out::Compaction {
                        event: CompactionEvent::Failed {
                            trigger: CompactionTrigger::Manual,
                            message: e.to_string(),
                            committed: false,
                        },
                    });
                }
            }
            result
        })
    }
}
