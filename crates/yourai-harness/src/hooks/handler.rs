//! Hook handler 实现细节 + 工厂函数。
//!
//! `HookHandler` trait 在 `yourai-core` 定义；本模块提供具体实现和工厂函数。

use crate::hooks::wire_output::HookJsonOutput;
use std::sync::Arc;
use std::time::Duration;
use yourai_core::hooks::{HookHandler, HookInvocation, HookOutput};
use yourai_core::BoxFuture;

/// One model hook owns prompt expansion, execution and decision parsing.
struct ModelHookHandler {
    prompt: String,
    model: Option<String>,
    agentic: bool,
    fallback: Arc<dyn yourai_core::model::ModelProvider>,
}
impl ModelHookHandler {
    async fn run(
        &self,
        invocation: &HookInvocation,
    ) -> Result<serde_json::Value, yourai_core::YourAiError> {
        use yourai_core::prelude::*;
        let model = invocation.model.as_ref().unwrap_or(&self.fallback).clone();
        if self
            .model
            .as_ref()
            .is_some_and(|name| name != model.model_iden())
        {
            return Err(crate::error(
                "hook_model",
                "requested model is not configured",
            ));
        }
        let prompt = self.prompt.replace(
            "$ARGUMENTS",
            &crate::hooks::event::to_wire_json(invocation).to_string(),
        );
        let instructions = r#"Evaluate the condition. Return exactly JSON {"ok":true} or {"ok":false,"reason":"..."}."#;
        let text = if self.agentic {
            let history = crate::DefaultContext::new(
                SessionId::from(invocation.base.session_id.clone()),
                instructions.into(),
                None,
                ContextPolicy::default(),
            );
            // Evaluator agents have no HookManager, so evaluation cannot recurse.
            let mut builder = Agent::builder()
                .model(model.clone())
                .context_manager(history)
                .agent_loop(Arc::new(crate::default_loop::DefaultLoop));
            if let Some(usage) = &invocation.usage {
                builder = builder.usage(usage.clone());
            }
            let mut options = TurnOptions::default();
            options.source = Some("hook");
            let config = {
                let mut settings: ExecutionOptions = Default::default();
                settings.set_steps(Some(8));
                settings
            };
            builder
                .build()
                .run_with(In::user_text(prompt), config, options)
                .await
                .map_err(|failure| *failure.error)?
                .text
        } else {
            let response = model
                .complete(
                    ModelRequest::new(
                        ChatRequest::from_user(prompt).with_system(instructions),
                        ChatOptions::default(),
                    )
                    .with_context("hook", invocation.base.session_id.clone()),
                )
                .await?;
            if let Some(usage) = &invocation.usage {
                let id = SessionId::from(invocation.base.session_id.clone());
                let event = UsageEvent::new(
                    Some(model.model_iden().into()),
                    "hook",
                    response.usage.clone(),
                );
                if let Some(warning) = usage.record_response(&id, &event).await {
                    eprintln!("{warning}");
                }
            }
            response.content.texts().join("")
        };
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| crate::error("hook_model", e))?;
        if !value["ok"].is_boolean() {
            return Err(crate::error("hook_model", "missing boolean ok"));
        }
        Ok(value)
    }
}
impl HookHandler for ModelHookHandler {
    fn execute<'a>(
        &'a self,
        invocation: &'a HookInvocation,
    ) -> BoxFuture<'a, Result<HookOutput, yourai_core::YourAiError>> {
        Box::pin(async move {
            let decision = self.run(invocation).await?;
            let output = if decision["ok"] == true {
                serde_json::json!({})
            } else {
                let reason = decision["reason"]
                    .as_str()
                    .unwrap_or("hook condition was not met");
                if invocation.event_kind() == yourai_core::hooks::HookEventKind::PreToolUse {
                    serde_json::json!({"hookSpecificOutput": {"hookEventName":"PreToolUse", "permissionDecision":"deny", "permissionDecisionReason":reason}})
                } else {
                    serde_json::json!({"continue":false,"stopReason":reason,"decision":"block","reason":reason})
                }
            };
            Ok(HookOutput::Parsed(output))
        })
    }
}

/// 用闭包创建 native handler。
pub struct NativeHandler<F>
where
    F: Fn(&HookInvocation) -> Result<HookJsonOutput, String> + Send + Sync,
{
    func: F,
}

impl<F> NativeHandler<F>
where
    F: Fn(&HookInvocation) -> Result<HookJsonOutput, String> + Send + Sync,
{
    pub fn new(func: F) -> Self {
        Self { func }
    }
}

impl<F> HookHandler for NativeHandler<F>
where
    F: Fn(&HookInvocation) -> Result<HookJsonOutput, String> + Send + Sync,
{
    fn execute<'a>(
        &'a self,
        invocation: &'a HookInvocation,
    ) -> BoxFuture<'a, Result<HookOutput, yourai_core::YourAiError>> {
        Box::pin(async move {
            let result = (self.func)(invocation).map_err(|e| yourai_core::ErrorKind::Provider {
                name: "hook",
                message: e,
            })?;
            // Native handler 返回已解析的 JSON value
            let json_value =
                serde_json::to_value(&result).map_err(|e| yourai_core::ErrorKind::Provider {
                    name: "hook",
                    message: format!("native handler serialization: {e}"),
                })?;
            Ok(HookOutput::Parsed(json_value))
        })
    }
}

/// Compile a validated registration once; missing dependencies fail before registration.
pub(crate) fn handler_from_config(
    config: &crate::config::HandlerConfig,
    http_policy: crate::hooks::http::HttpHookPolicy,
    provider: Option<Arc<dyn yourai_core::model::ModelProvider>>,
    background: crate::hooks::command::BackgroundCommandContext,
) -> Result<Arc<dyn HookHandler>, yourai_core::YourAiError> {
    let handler: Arc<dyn HookHandler> = match config {
        crate::config::HandlerConfig::Command { command, shell, .. } => Arc::new(
            crate::hooks::command::CommandHandler::new(command.clone(), *shell)
                .with_background(background),
        ),
        crate::config::HandlerConfig::Http {
            url,
            headers,
            allowed_env_vars,
            ..
        } => Arc::new(
            crate::hooks::http::HttpHandler::new(
                url.clone(),
                headers.clone(),
                allowed_env_vars.clone(),
            )
            .with_policy(http_policy),
        ),
        crate::config::HandlerConfig::Prompt { prompt, model, .. }
        | crate::config::HandlerConfig::Agent { prompt, model, .. } => {
            let fallback = provider.ok_or_else(|| {
                yourai_core::ErrorKind::Config("prompt/agent hook requires a model".into())
            })?;
            Arc::new(ModelHookHandler {
                prompt: prompt.clone(),
                model: model.clone(),
                agentic: matches!(config, crate::config::HandlerConfig::Agent { .. }),
                fallback,
            })
        }
    };
    Ok(handler)
}

/// 对 handler 执行应用超时；未配置时使用默认上限，防止 hook 无限期挂起会话。
pub const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn execute_with_timeout(
    handler: &dyn HookHandler,
    invocation: &HookInvocation,
    timeout: Option<Duration>,
) -> Result<HookOutput, yourai_core::YourAiError> {
    let d = timeout.unwrap_or(DEFAULT_HOOK_TIMEOUT);
    match tokio::time::timeout(d, handler.execute(invocation)).await {
        Ok(result) => result,
        Err(_) => Err(yourai_core::ErrorKind::Provider {
            name: "hook",
            message: format!("hook timed out after {d:?}"),
        }
        .into()),
    }
}
