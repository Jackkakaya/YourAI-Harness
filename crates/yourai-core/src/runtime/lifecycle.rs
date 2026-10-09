//! Session lifecycle wrappers. Business initialization and cleanup are private host actions.
use super::*;

impl SessionHost {
    pub async fn open_owned(
        lease: SessionLease,
        context: SessionContext,
        agent: Arc<Agent>,
        config: HostConfig,
        source: &str,
    ) -> Result<Arc<Self>, YourAiError> {
        let (host, interrupted) = Self::run_open(lease, context, agent, config)?;
        let source = source.to_owned();
        let cancel = CancellationToken::new();
        let _guard = cancel.clone().drop_guard();
        let (done, result) = oneshot::channel();
        // The initializer owns cleanup even when its caller abandons startup.
        tokio::spawn(async move {
            let initialized = tokio::select! {
                _ = cancel.cancelled() => Err(AbortReason::Cancelled.into()),
                result = host.run_initialize(&source, interrupted) => result,
            };
            if let Err(cause) = initialized {
                host.abort_open().await;
                let _ = done.send(Err(cause));
            } else {
                let (accepted, acknowledgement) = oneshot::channel();
                if done.send(Ok((host.clone(), accepted))).is_err()
                    || acknowledgement.await.is_err()
                {
                    host.abort_open().await;
                }
            }
        });
        let (host, accepted) = result.await.map_err(|e| error("host", e))??;
        let _ = accepted.send(());
        Ok(host)
    }
    async fn run_initialize(
        self: &Arc<Self>,
        source: &str,
        interrupted: bool,
    ) -> Result<(), YourAiError> {
        self.reconcile_history().await?;
        self.persist().await?;
        self.attach_background();
        let path = self.dir.join("config.json");
        if path.exists() {
            let mut value: serde_json::Value = read_json(&path)?;
            // Older releases persisted an unsupported field. Preserve resume while
            // rejecting the field in every new change_config request.
            if value
                .as_object_mut()
                .is_some_and(|v| v.remove("system_prompt").is_some())
            {
                atomic_write(&path, &value)?;
                self.post_event_async(RuntimeEvent {
                    id: uuid::Uuid::new_v4().to_string(), context: None,
                    notice: Some("Removed unsupported system_prompt from legacy runtime settings; the session prompt remains frozen.".into()), wake: false,
                }).await?;
            }
            serde_json::from_value::<crate::workspace::RuntimeConfig>(value)
                .map_err(|e| error("config", e))?
                .apply(self);
        }
        if self.config.workspace_enabled || !self.config.instruction_paths.is_empty() {
            let workspace = self.workspace()?;
            workspace
                .setup(if source == "startup" {
                    "init"
                } else {
                    "maintenance"
                })
                .await?;
            for path in &self.config.instruction_paths {
                workspace.load_instructions(path, source).await?;
            }
        }
        if interrupted {
            self.reconcile_history().await?;
            self.post_event_async(RuntimeEvent {
                id: format!("recovered-{}", uuid::Uuid::new_v4()), context: None,
                notice: Some("Previous execution was interrupted; tools were not replayed. Inspect interrupted_inputs() before continuing.".into()), wake: false,
            }).await?;
        }
        let result = self
            .dispatch(HookEvent::SessionStart {
                source: source.into(),
                model: self.agent.ctx().try_model().map(|m| m.model_iden().into()),
            })
            .await?;
        self.apply_hook(&result).await?;
        if let HookPointOutcome::SessionStart(outcome) = result.outcome {
            for path in outcome.watch_paths {
                self.watch_path_async(PathBuf::from(path)).await?;
            }
            if let Some(message) = outcome.initial_user_message {
                self.submit_async(In::user_text(message))
                    .await
                    .map_err(|e| error("host", e))?;
            }
        }
        if !self.watch_paths().is_empty() {
            self.workspace()?.start_watching(WATCH_INTERVAL)?;
        }
        Ok(())
    }
    async fn abort_open(&self) {
        self.closing.cancel();
        self.interrupt();
        self.wait_active().await;
        if let Ok(history) = self.history().await {
            if let Err(cause) = history.restore().await {
                self.close_warning(&cause);
            }
        }
        if let Err(cause) = self.run_shutdown(None).await {
            self.close_warning(&cause);
        }
        if let Some(hooks) = self.agent.ctx().try_hooks() {
            if let Err(cause) = hooks.shutdown_session(self.context().id.as_str()).await {
                self.close_warning(&cause);
            }
        }
        // Startup never takes ownership of the caller's pending input handoff.
        // Dropping the unpublished host releases its lease after resource cleanup.
    }
    pub(crate) async fn dispatch_active(
        &self,
        event: HookEvent,
    ) -> Result<HookDispatchResult, YourAiError> {
        tokio::select! {
            biased;
            _ = self.closing.cancelled() => Err(AbortReason::Cancelled.into()),
            result = self.dispatch(event) => result,
        }
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
    pub(crate) async fn apply_hook(&self, result: &HookDispatchResult) -> Result<(), YourAiError> {
        result.ensure_continuation()?;
        let contexts = result.additional_contexts();
        let notices: Vec<_> = result.notices().collect();
        if !contexts.is_empty() || !notices.is_empty() {
            self.post_event_async(RuntimeEvent {
                id: uuid::Uuid::new_v4().to_string(),
                context: (!contexts.is_empty()).then(|| contexts.join("\n")),
                notice: (!notices.is_empty()).then(|| notices.join("\n")),
                wake: false,
            })
            .await?;
        }
        Ok(())
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
        if through_seq <= after_seq {
            return;
        }
        let result = self
            .dispatch(HookEvent::TurnCompleted {
                turn_id: turn_id.to_string(),
                after_seq,
                through_seq,
            })
            .await;
        let messages = match result {
            Ok(result) => result
                .notices()
                .map(str::to_owned)
                .chain(result.blocking_messages())
                .collect::<Vec<_>>(),
            Err(e) => vec![format!("TurnCompleted hook failed: {e}")],
        };
        for message in messages {
            let _ = tx.send(Out::Notice {
                level: Level::Warning,
                message,
            });
        }
    }
    async fn end_session(&self, timeout: Option<Duration>) -> Option<String> {
        let previous = {
            let mut live = self.live.lock().unwrap();
            match &live.session_end {
                CloseNotification::Finished(result) => Some(result.clone()),
                CloseNotification::Started => Some(Some("Previous SessionEnd was interrupted; its effects are unknown and were not replayed.".into())),
                CloseNotification::Pending => { live.session_end = CloseNotification::Started; None }
            }
        };
        let mut warning = if let Some(previous) = previous {
            previous
        } else {
            let result = crate::time::timeout(
                timeout.map(|t| t / 4),
                self.dispatch(HookEvent::SessionEnd {
                    reason: "shutdown".into(),
                }),
            )
            .await
            .map_err(|_| error("hook", "SessionEnd cleanup deadline exceeded"))
            .and_then(|r| r);
            let warning = match result {
                Err(cause) => Some(cause.to_string()),
                Ok(result) => {
                    let errors: Vec<_> = result
                        .common
                        .messages
                        .iter()
                        .filter(|m| m.kind == HookMessageKind::NonBlockingError)
                        .map(|m| m.content.clone())
                        .chain(result.blocking_messages())
                        .collect();
                    (!errors.is_empty()).then(|| errors.join("\n"))
                }
            };
            self.live.lock().unwrap().session_end = CloseNotification::Finished(warning.clone());
            warning
        };
        if let Some(hooks) = self.agent.ctx().try_hooks() {
            if let Err(cause) = hooks.shutdown_session(self.context().id.as_str()).await {
                warning = Some(
                    [warning.unwrap_or_default(), cause.to_string()]
                        .into_iter()
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
        }
        warning
    }
    /// Finish durable cleanup without transferring ownership of pending inputs.
    pub async fn finish_close(&self, timeout: Option<Duration>) -> Result<(), YourAiError> {
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
            self.wait_active().await;
            let gate = self.operation.clone().lock_owned().await;
            if self.status() == SessionStatus::Closed {
                return Ok(());
            }
            // Restore is also the storage barrier for writes whose waiter was
            // cancelled after an owned SQLite commit started. Never release the
            // session lease while those writes can still modify its history.
            self.history().await?.restore().await?;
            self.run_shutdown(timeout).await?;
            // Closing hooks may report errors, but cannot prevent resource release.
            let hook_error = self.end_session(timeout).await;
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
