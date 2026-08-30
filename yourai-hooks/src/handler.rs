//! Hook handler 实现细节 + 工厂函数。
//!
//! `HookHandler` trait 在 `yourai-core` 定义；本模块提供具体实现和工厂函数。

use crate::wire_output::HookJsonOutput;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use yourai_core::hooks::{HookHandler, HookInvocation, HookOutput};

/// Boxed future（与 core 的 BoxFuture 一致）。
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Handler 分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookHandlerKind {
    Native,
    Command,
    Http,
    Prompt,
    Agent,
}

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
pub trait HookModelExecutor: Send + Sync {
    fn evaluate<'a>(
        &'a self,
        request: HookModelRequest,
    ) -> BoxFuture<'a, Result<HookModelDecision, yourai_core::YourAiError>>;
}

struct ModelHookHandler {
    prompt: String,
    model: Option<String>,
    agentic: bool,
    executor: std::sync::Arc<dyn HookModelExecutor>,
}

impl HookHandler for ModelHookHandler {
    fn execute<'a>(
        &'a self,
        invocation: &'a HookInvocation,
    ) -> BoxFuture<'a, Result<HookOutput, yourai_core::YourAiError>> {
        let wire_input = crate::event::to_wire_json(invocation).to_string();
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

/// 从配置创建对应的 handler。
pub fn handler_from_config(config: &crate::config::HandlerConfig) -> Box<dyn HookHandler> {
    handler_from_config_with_http_policy(config, crate::http::HttpHookPolicy::default(), None)
}

pub(crate) fn handler_from_config_with_http_policy(
    config: &crate::config::HandlerConfig,
    http_policy: crate::http::HttpHookPolicy,
    model_executor: Option<std::sync::Arc<dyn HookModelExecutor>>,
) -> Box<dyn HookHandler> {
    match config {
        crate::config::HandlerConfig::Command { command, shell, .. } => {
            Box::new(crate::command::CommandHandler::new(command.clone(), *shell))
        }
        crate::config::HandlerConfig::Http {
            url,
            headers,
            allowed_env_vars,
            ..
        } => Box::new(
            crate::http::HttpHandler::new(url.clone(), headers.clone(), allowed_env_vars.clone())
                .with_policy(http_policy),
        ),
        crate::config::HandlerConfig::Prompt { prompt, model, .. } => match model_executor {
            Some(executor) => Box::new(ModelHookHandler {
                prompt: prompt.clone(),
                model: model.clone(),
                agentic: false,
                executor,
            }),
            None => Box::new(UnsupportedHandler::new("prompt", prompt.clone())),
        },
        crate::config::HandlerConfig::Agent { prompt, model, .. } => match model_executor {
            Some(executor) => Box::new(ModelHookHandler {
                prompt: prompt.clone(),
                model: model.clone(),
                agentic: true,
                executor,
            }),
            None => Box::new(UnsupportedHandler::new("agent", prompt.clone())),
        },
    }
}

/// 未安装可选执行能力时使用的占位 handler。
pub struct UnsupportedHandler {
    kind: String,
    detail: String,
}

impl UnsupportedHandler {
    pub fn new(kind: impl Into<String>, detail: String) -> Self {
        Self {
            kind: kind.into(),
            detail,
        }
    }
}

impl HookHandler for UnsupportedHandler {
    fn execute<'a>(
        &'a self,
        _invocation: &'a HookInvocation,
    ) -> BoxFuture<'a, Result<HookOutput, yourai_core::YourAiError>> {
        let kind = self.kind.clone();
        let detail = self.detail.clone();
        Box::pin(async move {
            Err(yourai_core::ErrorKind::Config(format!(
                "handler type '{kind}' not yet implemented (detail: {detail})"
            ))
            .into())
        })
    }
}

/// 对 handler 执行应用超时。
pub async fn execute_with_timeout(
    handler: &dyn HookHandler,
    invocation: &HookInvocation,
    timeout: Option<Duration>,
) -> Result<HookOutput, yourai_core::YourAiError> {
    match timeout {
        Some(d) => match tokio::time::timeout(d, handler.execute(invocation)).await {
            Ok(result) => result,
            Err(_) => Err(yourai_core::ErrorKind::Provider {
                name: "hook",
                message: format!("hook timed out after {d:?}"),
            }
            .into()),
        },
        None => handler.execute(invocation).await,
    }
}
