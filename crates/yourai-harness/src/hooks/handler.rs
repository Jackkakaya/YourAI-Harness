//! Hook handler 实现细节 + 工厂函数。
//!
//! `HookHandler` trait 在 `yourai-core` 定义；本模块提供具体实现和工厂函数。

use crate::hooks::wire_output::HookJsonOutput;
use std::sync::Arc;
use std::time::Duration;
use yourai_core::hooks::{HookHandler, HookInvocation, HookOutput};
use yourai_core::BoxFuture;

#[derive(Debug, Clone)]
pub struct HookModelRequest {
    pub prompt: String,
    pub model: Option<String>,
    pub agentic: bool,
    pub invocation: HookInvocation,
}

#[derive(Debug, Clone)]
pub struct HookModelDecision {
    pub ok: bool,
    pub reason: Option<String>,
}

/// Prompt/Agent Hook 所需的宿主模型能力。具体模型与多轮 Agent 实现在独立 crate 注入。
pub trait HookEvaluator: Send + Sync {
    fn evaluate<'a>(
        &'a self,
        request: HookModelRequest,
    ) -> BoxFuture<'a, Result<HookModelDecision, yourai_core::YourAiError>>;
}

struct ModelHookHandler {
    prompt: String,
    model: Option<String>,
    agentic: bool,
    executor: std::sync::Arc<dyn HookEvaluator>,
}

impl HookHandler for ModelHookHandler {
    fn execute<'a>(
        &'a self,
        invocation: &'a HookInvocation,
    ) -> BoxFuture<'a, Result<HookOutput, yourai_core::YourAiError>> {
        let wire_input = crate::hooks::event::to_wire_json(invocation).to_string();
        let event = invocation.event_kind();
        let request = HookModelRequest {
            prompt: self.prompt.replace("$ARGUMENTS", &wire_input),
            model: self.model.clone(),
            agentic: self.agentic,
            invocation: invocation.clone(),
        };
        Box::pin(async move {
            let decision = self.executor.evaluate(request).await?;
            let output = if decision.ok {
                serde_json::json!({})
            } else {
                let reason = decision
                    .reason
                    .unwrap_or_else(|| "hook condition was not met".to_string());
                if event == yourai_core::hooks::HookEventKind::PreToolUse {
                    serde_json::json!({
                        "hookSpecificOutput": {
                            "hookEventName": "PreToolUse",
                            "permissionDecision": "deny",
                            "permissionDecisionReason": reason,
                        }
                    })
                } else {
                    serde_json::json!({
                        "continue": false,
                        "stopReason": reason,
                        "decision": "block",
                        "reason": reason,
                    })
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
    config: &crate::hooks::config::HandlerConfig,
    http_policy: crate::hooks::http::HttpHookPolicy,
    evaluator: Option<Arc<dyn HookEvaluator>>,
    background: crate::hooks::command::BackgroundCommandContext,
) -> Result<Arc<dyn HookHandler>, yourai_core::YourAiError> {
    let handler: Arc<dyn HookHandler> = match config {
        crate::hooks::config::HandlerConfig::Command { command, shell, .. } => Arc::new(
            crate::hooks::command::CommandHandler::new(command.clone(), *shell)
                .with_background(background),
        ),
        crate::hooks::config::HandlerConfig::Http {
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
        crate::hooks::config::HandlerConfig::Prompt { prompt, model, .. }
        | crate::hooks::config::HandlerConfig::Agent { prompt, model, .. } => {
            let executor = evaluator.ok_or_else(|| {
                yourai_core::ErrorKind::Config("prompt/agent hook requires a HookEvaluator".into())
            })?;
            Arc::new(ModelHookHandler {
                prompt: prompt.clone(),
                model: model.clone(),
                agentic: matches!(config, crate::hooks::config::HandlerConfig::Agent { .. }),
                executor,
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
