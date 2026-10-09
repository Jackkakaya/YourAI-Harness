//! A child lifecycle is one public exec around private business work.
use crate::{prelude::*, runtime::SessionHost};
use serde_json::{json, Value};
use std::sync::{Arc, Weak};

/// Only session assembly varies. The core owns running, hooks and cleanup.
pub trait SubagentFactory: Send + Sync {
    fn create<'a>(
        &'a self,
        parent: &'a SessionHost,
        id: SessionId,
        tc: &'a ToolContext<'_>,
    ) -> BoxFuture<'a, Result<Arc<SessionHost>, YourAiError>>;
}

pub struct Subagent {
    parent: Weak<SessionHost>,
    factory: Arc<dyn SubagentFactory>,
    agent_type: String,
    max_continuations: u32,
}
impl Subagent {
    pub fn new(
        parent: &Arc<SessionHost>,
        factory: Arc<dyn SubagentFactory>,
        agent_type: String,
        max_continuations: u32,
    ) -> Self {
        Self {
            parent: Arc::downgrade(parent),
            factory,
            agent_type,
            max_continuations,
        }
    }
    pub async fn exec(
        &self,
        tc: ToolContext<'_>,
        prompt: String,
    ) -> Result<(SessionId, String), YourAiError> {
        let parent = self
            .parent
            .upgrade()
            .ok_or_else(|| crate::error("subagent", "parent session gone"))?;
        let _activity = parent.activity()?;
        if tc.cancel.is_cancelled() {
            return Err(AbortReason::Cancelled.into());
        }
        let operation = async {
            // Agent execution identity is allocated without creating a persistent session.
            let id = SessionId::new();
            let start = parent
                .dispatch(HookEvent::SubagentStart {
                    agent_id: id.to_string(),
                    agent_type: self.agent_type.clone(),
                })
                .await?;
            start.ensure_allowed()?;
            // Notices belong to the parent; startup instructions belong to the child.
            let notices: Vec<_> = start.notices().collect();
            if !notices.is_empty() {
                parent
                    .post_event_async(crate::runtime_event::RuntimeEvent {
                        id: uuid::Uuid::new_v4().to_string(),
                        context: None,
                        notice: Some(notices.join("\n")),
                        wake: false,
                    })
                    .await?;
            }
            let child = self.factory.create(&parent, id.clone(), &tc).await?;
            let child_id = child.context().id;
            let _cleanup = ChildCleanup {
                parent: parent.clone(),
                id: child_id.clone(),
                child: child.clone(),
            };
            parent.register_child(&child)?;
            if child_id != id {
                return Err(crate::error(
                    "subagent",
                    "factory returned a different child identity",
                ));
            }
            if !start.additional_contexts().is_empty() {
                child
                    .agent()
                    .ctx()
                    .context_manager()?
                    .append(vec![StoredMessage::runtime_context(format!(
                        "[SubagentStart context]\n{}",
                        start.additional_contexts().join("\n")
                    ))])
                    .await?;
            }
            child
                .submit_async(In::user_text(prompt))
                .await
                .map_err(|e| crate::error("subagent", e))?;
            let mut last = String::new();
            for continuation in 0..=self.max_continuations {
                if let Some(text) = self.run(&child, &tc, id.as_str()).await? {
                    last = text;
                }
                let stop = parent
                    .dispatch(HookEvent::SubagentStop {
                        stop_hook_active: continuation > 0,
                        agent_id: id.to_string(),
                        agent_transcript_path: child
                            .context()
                            .transcript_path
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        agent_type: self.agent_type.clone(),
                        last_assistant_message: Some(last.clone()),
                    })
                    .await?;
                parent.apply_hook(&stop).await?;
                if stop.common.blocking_errors.is_empty() {
                    child.close(None).await?;
                    return Ok((id, last));
                }
                if continuation == self.max_continuations {
                    child.close(None).await?;
                    return Err(crate::error("subagent", "SubagentStop continuation limit"));
                }
                child
                    .submit_async(In::user_text(stop.blocking_messages().join("\n")))
                    .await
                    .map_err(|e| crate::error("subagent", e))?;
            }
            unreachable!("bounded continuation returns")
        };
        tokio::select! {
            biased;
            _ = parent.closing().cancelled() => Err(AbortReason::Cancelled.into()),
            _ = tc.cancel.cancelled() => Err(AbortReason::Cancelled.into()),
            result = operation => result,
        }
    }
    async fn run(
        &self,
        child: &Arc<SessionHost>,
        tc: &ToolContext<'_>,
        id: &str,
    ) -> Result<Option<String>, YourAiError> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let work = child.run_next(TurnLimits::default(), &tx, tc.cancel);
        tokio::pin!(work);
        let report = loop {
            tokio::select! { biased;
                result = &mut work => break result?,
                Some(event) = rx.recv() => match event {
                    Out::Ask { id: request_id, payload } => {
                        let reply = tc.ask(InteractionKind::Question, json!({"child_id":id,"request":payload})).await?;
                        child.submit_async(In::Reply { id: request_id, payload: reply }).await.map_err(|e| crate::error("subagent", e))?;
                    }
                    other => { if !tc.emit_progress(json!({"child_id":id,"event":other})) { child.interrupt(); return Err(AbortReason::Disconnected.into()); } }
                }
            }
        };
        report
            .map(|report| {
                report
                    .result
                    .map(|out| out.text)
                    .map_err(|error| *error.error)
            })
            .transpose()
    }
}
impl ToolProvider for Subagent {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new("subagent").with_schema(json!({"type":"object","properties":{"prompt":{"type":"string"}},"required":["prompt"],"additionalProperties":false}))
    }
    fn security_context(&self, input: &Value, _cwd: Option<&std::path::Path>) -> SecurityContext {
        SecurityContext {
            action: "subagent".into(),
            input: input.clone(),
            is_destructive: false,
            is_network: true,
        }
    }
    fn run<'a>(
        &'a self,
        tc: ToolContext<'a>,
        input: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            let prompt = input["prompt"]
                .as_str()
                .ok_or_else(|| crate::error("subagent", "prompt required"))?;
            let (id, text) = self.exec(tc, prompt.into()).await?;
            Ok(json!({"agent_id": id.to_string(), "text": text}))
        })
    }
}
struct ChildCleanup {
    parent: Arc<SessionHost>,
    id: SessionId,
    child: Arc<SessionHost>,
}
impl Drop for ChildCleanup {
    fn drop(&mut self) {
        if self.child.status() == SessionStatus::Closed {
            self.parent.release_child(&self.id);
        } else {
            self.parent.supervise_child(self.child.clone());
        }
    }
}
