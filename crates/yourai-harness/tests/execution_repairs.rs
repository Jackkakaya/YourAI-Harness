#[cfg(test)]
#[path = "support/loop.rs"]
mod support;

#[cfg(test)]
mod tests {
    use super::support::{Model, *};
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;
    use yourai_core::execution::ExecutionConfig;
    use yourai_core::prelude::*;
    use yourai_core::runtime::{HostConfig, SessionHost};
    use yourai_harness::DefaultContext;

    struct LostAppendReply {
        inner: Arc<DefaultContext>,
        fail_user: AtomicBool,
        fail_assistant: AtomicBool,
        fail_result: AtomicBool,
    }
    impl LostAppendReply {
        fn new() -> Self {
            Self {
                inner: DefaultContext::memory(SessionId::new()),
                fail_user: AtomicBool::new(false),
                fail_assistant: AtomicBool::new(false),
                fail_result: AtomicBool::new(false),
            }
        }
    }
    impl ContextManager for LostAppendReply {
        fn system_prompt(&self) -> String {
            self.inner.system_prompt()
        }
        fn session_id(&self) -> &SessionId {
            self.inner.session_id()
        }
        fn restore(&self) -> BoxFuture<'_, Result<(), YourAiError>> {
            self.inner.restore()
        }
        fn append(&self, m: Vec<StoredMessage>) -> BoxFuture<'_, Result<(), YourAiError>> {
            Box::pin(async move {
                let is_user = m
                    .iter()
                    .any(|m| m.message.role == ChatRole::User && !m.runtime_context);
                let is_assistant = m.iter().any(|m| {
                    m.message.role == ChatRole::Assistant
                        && !m.message.content.tool_calls().is_empty()
                });
                let is_result = m
                    .iter()
                    .any(|m| !m.message.content.tool_responses().is_empty());
                self.inner.append(m).await?;
                if (is_user && self.fail_user.swap(false, Ordering::SeqCst))
                    || (is_assistant && self.fail_assistant.swap(false, Ordering::SeqCst))
                    || (is_result && self.fail_result.swap(false, Ordering::SeqCst))
                {
                    return Err(ErrorKind::Provider {
                        name: "storage",
                        message: "write committed but completion lost".into(),
                    }
                    .into());
                }
                Ok(())
            })
        }
        fn records(&self) -> Vec<StoredMessage> {
            self.inner.records()
        }
        fn build_request(
            &self,
            t: &[ToolDefinition],
            m: &dyn ModelProvider,
        ) -> Result<ContextRequest, YourAiError> {
            self.inner.build_request(t, m)
        }
        fn prepare_compaction<'a>(
            &'a self,
            r: &'a CompactionRequest,
            m: &'a dyn ModelProvider,
            c: &'a CancellationToken,
        ) -> BoxFuture<'a, Result<CompactionPlan<'a>, YourAiError>> {
            self.inner.prepare_compaction(r, m, c)
        }
    }
    struct RejectThenExec(Arc<LostAppendReply>);
    impl AgentLoop for RejectThenExec {
        fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
            Box::pin(async move {
                let mut t = Turn::open(tc, ExecutionConfig::default()).await?;
                let c = t.enqueue("tool", json!({"value":1})).await?;
                self.0.fail_result.store(true, Ordering::SeqCst);
                assert!(t.reject_pending_tools("disabled").await.is_err());
                let observed = t.tool(&c.call_id)?.exec(&mut t, &c.call_id).await;
                assert!(observed.is_err(), "a rejected call must never run");
                t.finish(Err(ErrorKind::Loop("finish rejected batch".into()).into()))
                    .await
            })
        }
    }
    #[tokio::test]
    async fn rejected_call_cannot_execute_after_lost_append_reply() {
        let history = Arc::new(LostAppendReply::new());
        let registry = Arc::new(Registry::default());
        let handler = Arc::new(Handler::new("tool", Mode::Return));
        registry.register(handler.clone());
        let a = Agent::builder()
            .agent_loop(Arc::new(RejectThenExec(history.clone())))
            .context_manager(history.clone())
            .tools(registry)
            .build();
        a.run(In::user_text("go")).await.unwrap_err();
        assert!(handler.inputs.lock().unwrap().is_empty());
        let results: Vec<_> = history
            .records()
            .into_iter()
            .flat_map(|r| {
                r.message
                    .content
                    .tool_responses()
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("disabled"));
    }
    struct RecoverModel(Arc<LostAppendReply>);
    impl AgentLoop for RecoverModel {
        fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
            Box::pin(async move {
                let mut t = Turn::open(tc, ExecutionConfig::default()).await?;
                self.0.fail_assistant.store(true, Ordering::SeqCst);
                assert!(t
                    .model()?
                    .exec(&mut t, ModelOptions::default())
                    .await
                    .is_err());
                let calls = t.pending_tools();
                assert_eq!(calls.len(), 1);
                assert!(t
                    .model()?
                    .exec(&mut t, ModelOptions::default())
                    .await
                    .is_err());
                assert!(t
                    .tool(&calls[0].call_id)?
                    .exec(&mut t, &calls[0].call_id)
                    .await
                    .is_err());
                t.finish(Err(
                    ErrorKind::Loop("finish uncertain response".into()).into()
                ))
                .await
            })
        }
    }
    #[tokio::test]
    async fn lost_model_append_reply_blocks_execution_and_closes_calls() {
        let history = Arc::new(LostAppendReply::new());
        let registry = Arc::new(Registry::default());
        registry.register(Arc::new(Handler::new("tool", Mode::Return)));
        let model = Arc::new(Model::new(vec![calls(&["tool"]), answer("done")]));
        let a = Agent::builder()
            .agent_loop(Arc::new(RecoverModel(history.clone())))
            .context_manager(history.clone())
            .tools(registry)
            .model(model.clone())
            .build();
        a.run(In::user_text("go")).await.unwrap_err();
        assert_eq!(model.requests.lock().unwrap().len(), 1);
        let messages = history.messages();
        assert_eq!(
            messages.iter().flat_map(|m| m.content.tool_calls()).count(),
            1
        );
        assert_eq!(
            messages
                .iter()
                .flat_map(|m| m.content.tool_responses())
                .count(),
            1
        );
    }
    #[tokio::test]
    async fn user_committed_with_lost_reply_is_reconciled_once() {
        let dir = tempfile::TempDir::new().unwrap();
        let history = Arc::new(LostAppendReply::new());
        let a = Agent::builder()
            .agent_loop(Arc::new(
                yourai_harness::default_loop::DefaultLoop::default(),
            ))
            .context_manager(history.clone())
            .model(Arc::new(Model::new(vec![answer("done")])))
            .build();
        let h = SessionHost::open(
            dir.path(),
            SessionContext::new(history.session_id().clone(), dir.path()),
            a,
            HostConfig::default(),
            "startup",
        )
        .await
        .unwrap();
        history.fail_user.store(true, Ordering::SeqCst);
        h.submit(In::user_text("unique user input")).unwrap();
        let first = h
            .run_next(
                TurnLimits::default(),
                &yourai_core::context::DiscardSink,
                &CancellationToken::new(),
            )
            .await
            .unwrap()
            .unwrap();
        assert!(first.result.is_err());
        assert_eq!(h.queued(), 1);
        assert_eq!(
            history
                .records()
                .iter()
                .filter(|m| m.message.role == ChatRole::User)
                .count(),
            1
        );
        let second = h
            .run_next(
                TurnLimits::default(),
                &yourai_core::context::DiscardSink,
                &CancellationToken::new(),
            )
            .await
            .unwrap()
            .unwrap();
        assert!(second.result.is_ok());
        assert_eq!(
            history
                .records()
                .iter()
                .filter(|m| m.message.role == ChatRole::User)
                .count(),
            1
        );
        h.close(None).await.unwrap();
    }
    struct BrokenUsage {
        hang: bool,
        entered: tokio::sync::Notify,
    }
    impl BrokenUsage {
        fn new(hang: bool) -> Arc<Self> {
            Arc::new(Self {
                hang,
                entered: Default::default(),
            })
        }
    }
    impl UsageTracker for BrokenUsage {
        fn record_event<'a>(
            &'a self,
            _: &'a SessionId,
            _: &'a UsageEvent,
        ) -> BoxFuture<'a, Result<(), YourAiError>> {
            Box::pin(async move {
                self.entered.notify_one();
                if self.hang {
                    std::future::pending().await
                } else {
                    Err(ErrorKind::Provider {
                        name: "usage",
                        message: "usage failed".into(),
                    }
                    .into())
                }
            })
        }
        fn total(&self) -> BoxFuture<'_, Result<UsageStats, YourAiError>> {
            Box::pin(async { Ok(UsageStats::default()) })
        }
        fn session_usage<'a>(
            &'a self,
            _: &'a SessionId,
        ) -> BoxFuture<'a, Result<UsageStats, YourAiError>> {
            self.total()
        }
        fn reset_session<'a>(&'a self, _: &'a SessionId) -> BoxFuture<'a, Result<(), YourAiError>> {
            Box::pin(async { Ok(()) })
        }
    }
    #[tokio::test]
    async fn usage_failure_preserves_terminal_calls_and_known_usage() {
        let history = Arc::new(History::default());
        let a = Agent::builder()
            .agent_loop(Arc::new(
                yourai_harness::default_loop::DefaultLoop::default(),
            ))
            .context_manager(history.clone())
            .model(Arc::new(Model::new(vec![calls(&["tool"])])))
            .usage(BrokenUsage::new(false))
            .build();
        let failure = a.run(In::user_text("go")).await.unwrap_err();
        assert!(failure.output.usage.is_some());
        assert_eq!(
            history
                .messages()
                .iter()
                .flat_map(|m| m.content.tool_calls())
                .count(),
            1
        );
        assert_eq!(
            history
                .messages()
                .iter()
                .flat_map(|m| m.content.tool_responses())
                .count(),
            1
        );
    }
    #[tokio::test]
    async fn cancel_interrupts_hung_terminal_usage_save() {
        let history = Arc::new(History::default());
        let usage = BrokenUsage::new(true);
        let a = Agent::builder()
            .agent_loop(Arc::new(
                yourai_harness::default_loop::DefaultLoop::default(),
            ))
            .context_manager(history)
            .model(Arc::new(Model::new(vec![answer("done")])))
            .usage(usage.clone())
            .build();
        let handle = a.start(In::user_text("go")).unwrap();
        usage.entered.notified().await;
        handle.cancel.cancel();
        let failure = tokio::time::timeout(Duration::from_secs(1), handle.join())
            .await
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            *failure.error,
            YourAiError::Aborted(AbortReason::Cancelled)
        ));
    }

    struct MaintenanceRestore {
        inner: History,
        restores: std::sync::atomic::AtomicUsize,
        entered: tokio::sync::Notify,
    }
    impl ContextManager for MaintenanceRestore {
        fn system_prompt(&self) -> String {
            self.inner.system_prompt()
        }
        fn session_id(&self) -> &SessionId {
            self.inner.session_id()
        }
        fn restore(&self) -> BoxFuture<'_, Result<(), YourAiError>> {
            Box::pin(async move {
                if self.restores.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(())
                } else {
                    self.entered.notify_one();
                    std::future::pending().await
                }
            })
        }
        fn append(&self, m: Vec<StoredMessage>) -> BoxFuture<'_, Result<(), YourAiError>> {
            self.inner.append(m)
        }
        fn records(&self) -> Vec<StoredMessage> {
            self.inner.records()
        }
        fn build_request(
            &self,
            t: &[ToolDefinition],
            m: &dyn ModelProvider,
        ) -> Result<ContextRequest, YourAiError> {
            self.inner.build_request(t, m)
        }
        fn prepare_compaction<'a>(
            &'a self,
            r: &'a CompactionRequest,
            m: &'a dyn ModelProvider,
            c: &'a CancellationToken,
        ) -> BoxFuture<'a, Result<CompactionPlan<'a>, YourAiError>> {
            self.inner.prepare_compaction(r, m, c)
        }
    }
    #[tokio::test]
    async fn cancel_interrupts_automatic_maintenance_restore() {
        let history = Arc::new(MaintenanceRestore {
            inner: History::default(),
            restores: Default::default(),
            entered: Default::default(),
        });
        history.inner.tokens.store(80, Ordering::SeqCst);
        let hooks = Arc::new(Hooks::new(|i, r| {
            if i.event.kind() == HookEventKind::PreCompact {
                block(r)
            }
        }));
        let a = Agent::builder()
            .agent_loop(Arc::new(
                yourai_harness::default_loop::DefaultLoop::default(),
            ))
            .context_manager(history.clone())
            .model(Arc::new(Model::new(vec![answer("unused")])))
            .hooks(hooks)
            .build();
        let handle = a.start(In::user_text("go")).unwrap();
        history.entered.notified().await;
        handle.cancel.cancel();
        let failure = tokio::time::timeout(Duration::from_secs(1), handle.join())
            .await
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            *failure.error,
            YourAiError::Aborted(AbortReason::Cancelled)
        ));
    }

    #[tokio::test]
    async fn sqlite_input_retry_preserves_frozen_content_and_hook_context() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(
            yourai_harness::SqliteStore::open(&yourai_harness::SqliteStore::path(dir.path()))
                .unwrap(),
        );
        let id = store
            .create_session(SessionId::new(), "system")
            .await
            .unwrap()
            .id;
        let inner = DefaultContext::new(
            id.clone(),
            yourai_harness::context::ContextServices {
                store: Some(store.clone()),
                ..Default::default()
            },
        );
        let history = Arc::new(LostAppendReply {
            inner,
            fail_user: AtomicBool::new(true),
            fail_assistant: AtomicBool::new(false),
            fail_result: AtomicBool::new(false),
        });
        let hooks = Arc::new(Hooks::new(|_, result| {
            if let HookPointOutcome::UserPromptSubmit(outcome) = &mut result.outcome {
                outcome.additional_contexts.push("admission context".into());
            }
        }));
        let path = dir.path().join("input.txt");
        std::fs::write(&path, "ORIGINAL-CONTENT").unwrap();
        let input = In::user_text_with_attachments(
            "read attached",
            vec![UserAttachment::file(path.to_string_lossy(), None)],
        );
        let model = Arc::new(Model::new(vec![answer("done")]));
        let agent = Agent::builder()
            .agent_loop(Arc::new(
                yourai_harness::default_loop::DefaultLoop::default(),
            ))
            .context_manager(history)
            .model(model.clone())
            .hooks(hooks.clone())
            .build();
        let failure = agent.run(input).await.unwrap_err();
        assert_eq!(failure.output.pending.len(), 1);
        std::fs::write(&path, "CHANGED-CONTENT").unwrap();
        let restored = DefaultContext::new(
            id,
            yourai_harness::context::ContextServices {
                store: Some(store),
                ..Default::default()
            },
        );
        let agent = Agent::builder()
            .agent_loop(Arc::new(
                yourai_harness::default_loop::DefaultLoop::default(),
            ))
            .context_manager(restored.clone())
            .model(model.clone())
            .hooks(hooks.clone())
            .build();
        agent.run(failure.output.pending[0].clone()).await.unwrap();
        let records = restored.records();
        assert_eq!(records.iter().filter(|r| r.input.is_some()).count(), 1);
        assert_eq!(records.iter().filter(|r| r.runtime_context).count(), 1);
        assert_eq!(
            hooks
                .seen
                .lock()
                .unwrap()
                .iter()
                .filter(|k| **k == HookEventKind::UserPromptSubmit)
                .count(),
            1
        );
        {
            let requests = model.requests.lock().unwrap();
            let texts: Vec<_> = requests[0]
                .request
                .messages
                .iter()
                .flat_map(|m| m.content.texts())
                .collect();
            assert!(texts.iter().any(|t| t.contains("ORIGINAL-CONTENT")));
            assert!(!texts.iter().any(|t| t.contains("CHANGED-CONTENT")));
        }

        let mut conflicting = failure.output.pending[0].clone();
        if let In::UserText { text, .. } = &mut conflicting {
            *text = "different payload".into();
        }
        let error = agent.run(conflicting).await.unwrap_err();
        assert!(error.to_string().contains("input identity conflict"));
        assert_eq!(model.requests.lock().unwrap().len(), 1);
    }

    struct RecoverEnqueue(Arc<LostAppendReply>);
    impl AgentLoop for RecoverEnqueue {
        fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
            Box::pin(async move {
                let mut turn = Turn::open(tc, ExecutionConfig::default()).await?;
                self.0.fail_assistant.store(true, Ordering::SeqCst);
                assert!(turn.enqueue("tool", json!({"value":1})).await.is_err());
                let calls = turn.pending_tools();
                assert_eq!(calls.len(), 1);
                assert!(turn
                    .tool(&calls[0].call_id)?
                    .exec(&mut turn, &calls[0].call_id)
                    .await
                    .is_err());
                turn.finish(Err(
                    ErrorKind::Loop("finish uncertain enqueue".into()).into()
                ))
                .await
            })
        }
    }
    #[tokio::test]
    async fn programmatic_call_is_owned_before_uncertain_append() {
        let history = Arc::new(LostAppendReply::new());
        let registry = Arc::new(Registry::default());
        let handler = Arc::new(Handler::new("tool", Mode::Return));
        registry.register(handler.clone());
        let agent = Agent::builder()
            .agent_loop(Arc::new(RecoverEnqueue(history.clone())))
            .context_manager(history.clone())
            .tools(registry)
            .build();
        agent.run(In::user_text("go")).await.unwrap_err();
        assert!(handler.inputs.lock().unwrap().is_empty());
        assert_eq!(
            history
                .messages()
                .iter()
                .flat_map(|m| m.content.tool_calls())
                .count(),
            1
        );
        assert_eq!(
            history
                .messages()
                .iter()
                .flat_map(|m| m.content.tool_responses())
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn truncated_response_retains_usage_without_publishing_tool_calls() {
        let history = Arc::new(History::default());
        let mut terminal = end("partial", vec![call("invalid", "tool")]);
        if let ChatStreamEvent::End(end) = &mut terminal {
            end.captured_stop_reason = Some(StopReason::MaxTokens("length".into()));
        }
        let agent = Agent::builder()
            .agent_loop(Arc::new(
                yourai_harness::default_loop::DefaultLoop::default(),
            ))
            .context_manager(history.clone())
            .model(Arc::new(Model::new(vec![events(vec![terminal])])))
            .build();
        let failure = agent.run(In::user_text("go")).await.unwrap_err();
        assert_eq!(failure.output.usage.unwrap().total_tokens, 3);
        assert!(!history
            .messages()
            .iter()
            .any(|m| !m.content.tool_calls().is_empty()));
    }
}
