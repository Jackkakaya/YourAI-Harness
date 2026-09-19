use crate::{error, MemoryContext};
use std::{sync::Arc, time::Duration};
use yourai_core::prelude::*;
use yourai_hooks::{HookModelDecision, HookModelExecutor, HookModelRequest};
/// Both prompt and agent hooks use the supplied (optionally shared-metered) model.
/// Evaluator agents deliberately have no HookRuntime, preventing recursive evaluation.
pub struct DefaultHookModelExecutor {
    pub model: Arc<dyn ModelProvider>,
    pub tools: Option<Arc<dyn ToolRegistry>>,
    pub usage: Option<Arc<dyn UsageTracker>>,
    pub timeout: Duration,
    pub max_model_calls: u32,
}
impl HookModelExecutor for DefaultHookModelExecutor {
    fn evaluate<'a>(
        &'a self,
        request: HookModelRequest,
    ) -> BoxFuture<'a, Result<HookModelDecision, YourAiError>> {
        Box::pin(async move {
            if request
                .model
                .as_ref()
                .is_some_and(|m| m != self.model.model_iden())
            {
                return Err(error("hook_model", "requested model is not configured"));
            }
            let instructions="Evaluate the condition. Return exactly JSON {\"ok\":true} or {\"ok\":false,\"reason\":\"...\"}.";
            let work = async {
                let text = if request.agentic {
                    let history = MemoryContext::memory(SessionId(
                        request.invocation.base.session_id.clone(),
                    ));
                    let mut builder = Agent::builder()
                        .model(self.model.clone())
                        .context_manager(history)
                        .agent_loop(Arc::new(yourai_loop::DefaultLoop::new(
                            yourai_loop::LoopConfig {
                                system_prompt: Some(instructions.into()),
                                max_model_calls: self.max_model_calls,
                                ..Default::default()
                            },
                        )));
                    if let Some(usage) = &self.usage {
                        builder = builder.usage(Arc::new(HookUsage(usage.clone())));
                    }
                    if let Some(tools) = &self.tools {
                        builder = builder.tools(tools.clone());
                    }
                    builder
                        .build()
                        .run(In::user_text(request.prompt))
                        .await
                        .map_err(|f| *f.error)?
                        .text
                } else {
                    let response = self
                        .model
                        .complete(ModelRequest::new(
                            ChatRequest::from_user(request.prompt).with_system(instructions),
                            ChatOptions::default(),
                        ))
                        .await?;
                    if let Some(usage) = &self.usage {
                        usage
                            .record_event(
                                &SessionId(request.invocation.base.session_id.clone()),
                                &UsageEvent::new(
                                    Some(self.model.model_iden().into()),
                                    "hook",
                                    response.usage.clone(),
                                ),
                            )
                            .await?;
                    }
                    response.content.texts().join("")
                };
                let value: serde_json::Value =
                    serde_json::from_str(&text).map_err(|e| error("hook_model", e))?;
                Ok(HookModelDecision {
                    ok: value["ok"]
                        .as_bool()
                        .ok_or_else(|| error("hook_model", "missing boolean ok"))?,
                    reason: value["reason"].as_str().map(str::to_owned),
                })
            };
            tokio::time::timeout(self.timeout, work)
                .await
                .map_err(|_| error("hook_model", "evaluation timeout"))?
        })
    }
}

struct HookUsage(Arc<dyn UsageTracker>);
impl UsageTracker for HookUsage {
    fn record_event<'a>(
        &'a self,
        id: &'a SessionId,
        event: &'a UsageEvent,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async move {
            let mut event = event.clone();
            event.source = "hook".into();
            self.0.record_event(id, &event).await
        })
    }

    fn total<'a>(&'a self) -> BoxFuture<'a, Result<UsageStats, YourAiError>> {
        self.0.total()
    }
    fn session_usage<'a>(
        &'a self,
        id: &'a SessionId,
    ) -> BoxFuture<'a, Result<UsageStats, YourAiError>> {
        self.0.session_usage(id)
    }
    fn reset_session<'a>(&'a self, id: &'a SessionId) -> BoxFuture<'a, Result<(), YourAiError>> {
        self.0.reset_session(id)
    }
}
