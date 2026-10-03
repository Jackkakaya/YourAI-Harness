//! Session lifecycle wrappers. Business initialization and cleanup are private host actions.
use super::*;

/// 宿主回调实现：core 公共操作模板（任务/工作区等）经由这里派发与消费 hook。
impl yourai_core::hooks::HookHost for SessionHost {
    fn dispatch_hook(
        &self,
        event: HookEvent,
    ) -> BoxFuture<'_, Result<HookDispatchResult, YourAiError>> {
        Box::pin(SessionHost::dispatch(self, event))
    }
    fn consume_hook_result<'a>(
        &'a self,
        result: &'a HookDispatchResult,
        deny_block: bool,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(SessionHost::consume_hook_async(self, result, deny_block))
    }
}

impl SessionHost {
    pub(crate) async fn open_owned(
        lease: SessionLease,
        context: SessionContext,
        agent: Arc<Agent>,
        config: HostConfig,
        source: &str,
    ) -> Result<Arc<Self>, YourAiError> {
        let host = Self::run_open(lease, context, agent, config, source).await?;
        let sink = HostStartSink(&host);
        yourai_core::session_ops::session_start(
            host.as_ref(),
            &sink,
            source,
            host.agent.ctx().try_model().map(|m| m.model_iden().into()),
        )
        .await?;
        if !host.watch_paths().is_empty() {
            host.workspace()?.start_watching(WATCH_INTERVAL)?;
        }
        Ok(host)
    }
    pub(crate) async fn dispatch(
        &self,
        event: HookEvent,
    ) -> Result<HookDispatchResult, YourAiError> {
        let Some(hooks) = self.agent.ctx().try_hooks() else {
            return Ok(HookDispatchResult::empty(event.kind()));
        };
        let c = self.context();
        let mut base = BaseInput::new(c.id.as_str(), c.cwd.to_string_lossy());
        base.transcript_path = c
            .transcript_path
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let invocation = HookInvocation::new(base, event);
        let result = crate::time::timeout(self.config.hook_timeout, hooks.dispatch(&invocation))
            .await
            .map_err(|_| error("hook", "host hook timed out"))??;
        result.validate_for(invocation.event.kind())?;
        Ok(result)
    }
    pub(crate) fn consume_hook(
        &self,
        r: &HookDispatchResult,
        deny_block: bool,
    ) -> Result<(), YourAiError> {
        if r.common.prevent_continuation || (deny_block && !r.common.blocking_errors.is_empty()) {
            return Err(
                AbortReason::HookStopped(r.common.stop_reason.clone().unwrap_or_else(|| {
                    r.common
                        .blocking_errors
                        .iter()
                        .map(|e| e.message.clone())
                        .collect::<Vec<_>>()
                        .join("\n")
                }))
                .into(),
            );
        }
        let contexts = match &r.outcome {
            HookPointOutcome::SessionStart(o) => o.additional_contexts.clone(),
            HookPointOutcome::Generic(o) => o.additional_contexts.clone(),
            _ => vec![],
        };
        let notices: Vec<_> = r.notices().collect();
        if !contexts.is_empty() || !notices.is_empty() {
            self.post_event(RuntimeEvent {
                id: uuid::Uuid::new_v4().to_string(),
                context: (!contexts.is_empty()).then(|| contexts.join("\n")),
                notice: (!notices.is_empty()).then(|| notices.join("\n")),
                wake: false,
            })?;
        }
        Ok(())
    }
    pub(super) fn attach_background(self: &Arc<Self>) {
        let Some(mut rx) = self
            .agent
            .ctx()
            .try_hooks()
            .and_then(|h| h.subscribe_background())
        else {
            return;
        };
        let weak = Arc::downgrade(self);
        let cancel = self.closing.clone();
        let task = tokio::spawn(async move {
            loop {
                let event = tokio::select! {_=cancel.cancelled()=>break,e=rx.recv()=>e};
                let Some(host) = weak.upgrade() else { break };
                match event {
                    Ok(e) if e.session_id == host.context().id.as_str() => {
                        let wake = e.rewake && e.exit_code == 2;
                        let text = if e.stderr.is_empty() {
                            e.stdout
                        } else {
                            format!("{}\n{}", e.stdout, e.stderr)
                        };
                        if let Err(err) = host
                            .post_event_async(RuntimeEvent {
                                id: e.task_id,
                                context: wake.then(|| text.clone()),
                                notice: Some(text),
                                wake,
                            })
                            .await
                        {
                            host.live.lock().unwrap().journal.last_error = Some(err.to_string());
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        let _ = host
                            .post_event_async(RuntimeEvent {
                                id: uuid::Uuid::new_v4().to_string(),
                                context: None,
                                notice: Some(format!(
                                    "Lost {n} background events; inspect hook logs"
                                )),
                                wake: false,
                            })
                            .await;
                    }
                    Err(_) => break,
                }
            }
        });
        self.background.lock().unwrap().push(task);
    }
    pub(crate) async fn consume_hook_async(
        &self,
        result: &HookDispatchResult,
        deny: bool,
    ) -> Result<(), YourAiError> {
        let result = result.clone();
        self.blocking(move |host| host.consume_hook(&result, deny))
            .await?
    }
    pub(super) async fn notify_turn_completed(
        &self,
        turn_id: &TurnId,
        after_seq: i64,
        tx: &mpsc::UnboundedSender<Out>,
    ) {
        let through_seq = self
            .agent
            .ctx()
            .try_context_manager()
            .map(|h| h.last_sequence())
            .unwrap_or(after_seq);
        let sender = tx.clone();
        let notify = move |message: &str| {
            let _ = sender.send(Out::Notice {
                level: Level::Warning,
                message: message.to_owned(),
            });
        };
        yourai_core::session_ops::turn_completed(self, turn_id, after_seq, through_seq, &notify)
            .await;
    }
    /// Finish durable cleanup without transferring ownership of pending inputs.
    pub(crate) async fn finish_close(&self, timeout: Option<Duration>) -> Result<(), YourAiError> {
        if self.status() == SessionStatus::Closed {
            return Ok(());
        }
        self.closing.cancel();
        self.interrupt();
        {
            let mut live = self.live.lock().unwrap();
            if live.status == SessionStatus::Closed {
                return Ok(());
            }
            live.status = SessionStatus::Closing;
        }
        crate::time::timeout(timeout, async {
            let gate = self.operation.clone().lock_owned().await;
            if self.status() == SessionStatus::Closed {
                return Ok(());
            }
            self.run_shutdown(timeout).await?;
            // Closing hooks may report errors, but cannot prevent resource release.
            let hook_error = yourai_core::session_ops::session_end(
                self,
                self.agent.ctx().try_hooks(),
                self.context().id.as_str(),
                timeout,
            )
            .await?;
            self.run_commit_close(gate, hook_error).await
        })
        .await
        .map_err(|_| {
            error(
                "host",
                "close timed out; retry close to finish cleanup and collect pending inputs",
            )
        })?
    }
}

/// SessionStart 结果的宿主应用回调（监视路径与初始用户输入）。
struct HostStartSink<'a>(&'a SessionHost);
impl yourai_core::session_ops::SessionStartSink for HostStartSink<'_> {
    fn watch_path(&self, path: PathBuf) -> BoxFuture<'_, Result<(), YourAiError>> {
        Box::pin(self.0.watch_path_async(path))
    }
    fn submit_user_text(&self, message: String) -> BoxFuture<'_, Result<(), YourAiError>> {
        Box::pin(async move {
            self.0
                .submit_async(In::user_text(message))
                .await
                .map_err(|e| error("host", e))
        })
    }
}
