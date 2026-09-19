//! The single framework bridge between memory callbacks and the hook registry.
//! Provider implementations do not depend on HookHandler or HookRegistry.
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use yourai_core::prelude::*;

pub(crate) async fn register(
    hooks: &dyn HookRegistry,
    provider: Arc<dyn MemoryProvider>,
    sessions: Arc<dyn SessionManager>,
    session_id: SessionId,
) -> Result<(), YourAiError> {
    let handler = Arc::new(MemoryHookAdapter {
        provider,
        sessions,
        session_id,
    });
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
    provider: Arc<dyn MemoryProvider>,
    sessions: Arc<dyn SessionManager>,
    session_id: SessionId,
}
impl MemoryHookAdapter {
    async fn messages(
        &self,
        after: i64,
        through: i64,
        active_only: bool,
    ) -> Result<Vec<ChatMessage>, YourAiError> {
        let mut cursor = after;
        let mut messages = vec![];
        loop {
            let page = self
                .sessions
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
            if invocation.base.session_id != self.session_id.0 {
                return Ok(HookOutput::Parsed(serde_json::json!({})));
            }
            let cancel = CancellationToken::new();
            let _cancel_on_drop = cancel.clone().drop_guard();
            let session = MemorySession {
                session_id: self.session_id.clone(),
                cwd: PathBuf::from(&invocation.base.cwd),
            };
            match &invocation.event {
                HookEvent::SessionStart { source, .. } => {
                    self.provider
                        .on_session_start(&session, source, &cancel)
                        .await?
                }
                HookEvent::SessionEnd { reason } => {
                    self.provider
                        .on_session_end(&session, reason, &cancel)
                        .await?
                }
                HookEvent::PreCompact { trigger, .. } => {
                    let messages = self.messages(0, i64::MAX, true).await?;
                    self.provider
                        .on_pre_compact(&session, trigger, &messages, &cancel)
                        .await?;
                }
                HookEvent::TurnCompleted {
                    turn_id,
                    after_seq,
                    through_seq,
                } => {
                    let messages = self.messages(*after_seq, *through_seq, false).await?;
                    if messages.iter().any(|m| m.role == ChatRole::User) {
                        self.provider
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
