//! The single framework bridge between memory callbacks and the hook registry.
//! Provider implementations do not depend on HookHandler or HookRegistry.
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use yourai_core::prelude::*;

pub(crate) async fn register(
    hooks: &dyn HookRegistry,
    session_id: SessionId,
) -> Result<(), YourAiError> {
    let handler = Arc::new(MemoryHookAdapter { session_id });
    for event in [
        HookEventKind::SessionStart,
        HookEventKind::SessionEnd,
        HookEventKind::PreCompact,
        HookEventKind::TurnCompleted,
    ] {
        hooks
            .register(NativeHookRegistration {
                id: format!("memory:{}:{}", handler.session_id, event.as_str()),
                event,
                matcher: None,
                handler: handler.clone(),
                timeout: Some(Duration::from_secs(30)),
                source: HookSource::Session,
                failure_policy: FailurePolicy::Open,
                once: false,
            })
            .await?;
    }
    Ok(())
}
struct MemoryHookAdapter {
    session_id: SessionId,
}
impl MemoryHookAdapter {
    async fn messages(
        &self,
        sessions: &dyn SessionManager,
        after: i64,
        through: i64,
        active_only: bool,
    ) -> Result<Vec<ChatMessage>, YourAiError> {
        let mut cursor = after;
        let mut messages = vec![];
        loop {
            let page = sessions
                .read_messages(
                    &self.session_id,
                    MessageQuery {
                        after: cursor,
                        active_only,
                        limit: 256,
                    },
                )
                .await?;
            for row in page.messages {
                if row.seq > through {
                    return Ok(messages);
                }
                // Never feed retrieved memories, injected skills, runtime context,
                // or generated summaries back as newly observed conversation.
                if !row.runtime_context && !row.summary {
                    messages.push(row.message);
                }
            }
            match page.next {
                Some(next) if next > cursor && next < through => cursor = next,
                _ => return Ok(messages),
            }
        }
    }
}
impl HookHandler for MemoryHookAdapter {
    fn execute<'a>(
        &'a self,
        invocation: &'a HookInvocation,
    ) -> BoxFuture<'a, Result<HookOutput, YourAiError>> {
        Box::pin(async move {
            // Child agents can share a HookRuntime; never mirror another session.
            if invocation.base.session_id != self.session_id.as_str() {
                return Ok(HookOutput::Parsed(serde_json::json!({})));
            }
            let Some(provider) = &invocation.execution.memory else {
                return Ok(HookOutput::Parsed(serde_json::json!({})));
            };
            let sessions =
                invocation.execution.sessions.as_deref().ok_or_else(|| {
                    crate::error("memory", "session repository is not configured")
                })?;
            let cancel = CancellationToken::new();
            let _cancel_on_drop = cancel.clone().drop_guard();
            let session = MemorySession {
                session_id: self.session_id.clone(),
                cwd: PathBuf::from(&invocation.base.cwd),
            };
            match &invocation.event {
                HookEvent::SessionStart { source, .. } => {
                    provider.on_session_start(&session, source, &cancel).await?
                }
                HookEvent::SessionEnd { reason } => {
                    provider.on_session_end(&session, reason, &cancel).await?
                }
                HookEvent::PreCompact { trigger, .. } => {
                    let messages = self.messages(sessions, 0, i64::MAX, true).await?;
                    provider
                        .on_pre_compact(&session, trigger, &messages, &cancel)
                        .await?;
                }
                HookEvent::TurnCompleted {
                    turn_id,
                    after_seq,
                    through_seq,
                } => {
                    let messages = self
                        .messages(sessions, *after_seq, *through_seq, false)
                        .await?;
                    if messages.iter().any(|m| m.role == ChatRole::User) {
                        provider
                            .sync_turn(
                                &CompletedMemoryTurn {
                                    session,
                                    turn_id: turn_id.clone(),
                                    messages,
                                },
                                &cancel,
                            )
                            .await?;
                    }
                }
                _ => {}
            }
            Ok(HookOutput::Parsed(serde_json::json!({})))
        })
    }
}

#[cfg(test)]
mod binding_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Memory(AtomicUsize);
    impl MemoryProvider for Memory {
        fn on_session_end<'a>(
            &'a self,
            _: &'a MemorySession,
            _: &'a str,
            _: &'a CancellationToken,
        ) -> BoxFuture<'a, Result<(), YourAiError>> {
            Box::pin(async {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }
    }
    #[tokio::test]
    async fn lifecycle_callbacks_follow_the_invocation_snapshot() {
        let dir = tempfile::TempDir::new().unwrap();
        let sessions =
            Arc::new(crate::SqliteStore::open(&dir.path().join("sessions.sqlite3")).unwrap());
        let id = sessions.create_session("").await.unwrap().id;
        let old = Arc::new(Memory(AtomicUsize::new(0)));
        let new = Arc::new(Memory(AtomicUsize::new(0)));
        let agent = Agent::builder()
            .agent_loop(Arc::new(crate::default_loop::DefaultLoop::default()))
            .session(sessions)
            .memory(old.clone())
            .build();
        let before = agent.ctx().snapshot().unwrap();
        agent.ctx().set_memory(new.clone());
        let after = agent.ctx().snapshot().unwrap();
        let adapter = MemoryHookAdapter {
            session_id: id.clone(),
        };
        for snapshot in [&before, &after] {
            adapter
                .execute(
                    &HookInvocation::new(
                        BaseInput::new(id.as_str(), ""),
                        HookEvent::SessionEnd {
                            reason: "test".into(),
                        },
                    )
                    .with_execution(ExecutionBindings::from_snapshot(snapshot)),
                )
                .await
                .unwrap();
        }
        assert_eq!(old.0.load(Ordering::SeqCst), 1);
        assert_eq!(new.0.load(Ordering::SeqCst), 1);
    }
}
