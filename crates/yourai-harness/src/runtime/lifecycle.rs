//! Session lifecycle wrappers. Business initialization and cleanup are private host actions.
use super::*;

impl SessionHost {
    pub(crate) async fn open_owned(
        lease: SessionLease,
        context: SessionContext,
        agent: Arc<Agent>,
        config: HostConfig,
        source: &str,
    ) -> Result<Arc<Self>, YourAiError> {
        let host = Self::run_open(lease, context, agent, config, source).await?;
        let result = host
            .dispatch(HookEvent::SessionStart {
                source: source.into(),
                model: host.agent.ctx().try_model().map(|m| m.model_iden().into()),
            })
            .await?;
        host.consume_hook_async(&result, false).await?;
        if let HookPointOutcome::SessionStart(o) = result.outcome {
            for path in o.watch_paths {
                host.watch_path_async(PathBuf::from(path)).await?;
            }
            if let Some(message) = o.initial_user_message {
                host.submit_async(In::user_text(message))
                    .await
                    .map_err(|e| error("host", e))?;
            }
        }
        if !host.watch_paths().is_empty() {
            host.workspace()?.start_watching(WATCH_INTERVAL)?;
        }
        Ok(host)
    }
    pub(crate) async fn dispatch(
        &self,
        event: HookEvent,
    ) -> Result<HookDispatchResult, YourAiError> {
        let snapshot = self.agent.ctx().snapshot()?;
        let Some(hooks) = &snapshot.hooks else {
            return Ok(HookDispatchResult::empty(event.kind()));
        };
        let c = self.context();
        let mut base = BaseInput::new(c.id.as_str(), c.cwd.to_string_lossy());
        base.transcript_path = c
            .transcript_path
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let invocation = HookInvocation::new(base, event)
            .with_execution(ExecutionBindings::from_snapshot(&snapshot));
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
        if through_seq > after_seq {
            match self
                .dispatch(HookEvent::TurnCompleted {
                    turn_id: turn_id.to_string(),
                    after_seq,
                    through_seq,
                })
                .await
            {
                Ok(r) => {
                    for message in r
                        .visible_messages()
                        .map(|m| m.content.clone())
                        .chain(r.common.system_messages.iter().cloned())
                        .chain(r.common.blocking_errors.iter().map(|e| e.message.clone()))
                    {
                        let _ = tx.send(Out::Notice {
                            level: Level::Warning,
                            message,
                        });
                    }
                }
                Err(e) => {
                    let _ = tx.send(Out::Notice {
                        level: Level::Warning,
                        message: format!("TurnCompleted hook failed: {e}"),
                    });
                }
            }
        }
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
            let hook_result = crate::time::timeout(
                timeout.map(|t| t / 4),
                self.dispatch(HookEvent::SessionEnd {
                    reason: "shutdown".into(),
                }),
            )
            .await
            .map_err(|_| error("hook", "SessionEnd cleanup deadline exceeded"))
            .and_then(|r| r);
            if let Some(hooks) = self.agent.ctx().try_hooks() {
                hooks.shutdown_session(self.context().id.as_str()).await?;
            }
            let hook_error = match hook_result {
                Err(e) => Some(e.to_string()),
                Ok(r) => {
                    let errors: Vec<_> = r
                        .common
                        .messages
                        .iter()
                        .filter(|m| matches!(m.kind, HookMessageKind::NonBlockingError))
                        .map(|m| m.content.clone())
                        .collect();
                    (!errors.is_empty()).then(|| errors.join("\n"))
                }
            };
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
