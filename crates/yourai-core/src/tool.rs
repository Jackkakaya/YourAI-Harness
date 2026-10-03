//! 工具：定义侧（给模型的 schema）与执行侧（ToolContext 赋能）分离。
//!
//! 执行签名带 [`ToolContext`]——工具有自己的身份（call_id）、能发进度、
//! 能被取消、能拿到两层审批能力（决策 5.5 + 收敛修订）。
//!
//! **两层审批**（架构收敛结论）：
//! - 第一层（公共执行包装）：`check_tool_call`——执行前统一问
//! - 第二层（具体工具）：`check_command` / `check_file_access` 与
//!   `sandbox.apply(&mut Command)`——只有工具自己知道 Command/路径，
//!   由工具经 [`ToolContext`] 注入的能力自行调用
//!
//! 工具仍然不知道 UI 存在——只对 outbox 讲协议（Ask/Reply 经执行层中介）。

use crate::chat::{Tool, ToolResponse};
use crate::error::YourAiError;
use crate::future::BoxFuture;
use crate::interaction::{InteractionKind, InteractionRequest, ToolInteraction};
use crate::sandbox::SandboxProvider;
use crate::security::{SecurityContext, SecurityProvider};
use crate::ui::OutSink;
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// 工具执行期的能力注入（由公共执行包装装配）。
pub struct ToolContext<'a> {
    /// 本次工具调用的身份（= 模型 ToolCall 的 call_id）；
    /// 工具进度事件用此 id；Ask 使用独立 request_id 并关联此调用
    pub call_id: String,
    /// 发 Out（约定：工具只发 `Out::ToolProgress`；subagent 转发子事件也走这里）
    pub emit: &'a dyn OutSink,
    /// ESC 打断长工具
    pub cancel: &'a CancellationToken,
    /// 第二层审批：命令/文件级检查（快照，可能为 None = 不拦截）
    pub security: Option<Arc<dyn SecurityProvider>>,
    /// 沙箱：工具对自建的 `tokio::process::Command` 调 `apply`（快照）
    pub sandbox: Option<Arc<dyn SandboxProvider>>,
    /// 工具执行期间请求用户交互；执行包装同时服务其内部请求通道。
    /// None 表示不支持交互，ask() 立即报 Config，而不是永久等待。
    pub interaction: Option<&'a dyn ToolInteraction>,
}

impl ToolContext<'_> {
    /// 发一条进度事件（id 自动取 call_id）；返回 false = 消费端已关闭
    pub fn emit_progress(&self, payload: Value) -> bool {
        self.emit.send(crate::protocol::Out::ToolProgress {
            id: self.call_id.clone(),
            payload,
        })
    }

    /// 向执行层提问，自动生成独立请求 id 并绑定本次工具调用。
    pub fn ask<'a>(
        &'a self,
        kind: InteractionKind,
        payload: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            if self.cancel.is_cancelled() {
                return Err(crate::AbortReason::Cancelled.into());
            }
            let interaction = self.interaction.ok_or_else(|| {
                crate::ErrorKind::Config("tool interaction is not configured".into())
            })?;
            let request = InteractionRequest::new(self.call_id.clone(), kind, payload);
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => Err(crate::AbortReason::Cancelled.into()),
                result = interaction.request(request, self.cancel) => result,
            }
        })
    }
}

/// Implementation-side backend. Scheduling code uses the Harness ToolExecutor.
pub trait ToolHandler: Send + Sync {
    /// 工具名（模型调用时使用）
    fn name(&self) -> &str;

    /// 给模型看的 schema（genai::chat::Tool）
    fn definition(&self) -> Tool;

    /// 为第一层审批描述这次调用。
    ///
    /// 执行包装在执行前调用本方法，再把结果交给
    /// `SecurityProvider::check_tool_call`。工具必须显式标注破坏性和网络属性，
    /// 避免执行包装根据名字猜测安全语义。
    fn security_context(&self, input: &Value) -> SecurityContext;

    /// 普通错误由执行包装转成工具失败结果；Turn 取消/总超时/额度耗尽向上收尾，
    /// 不能把所有 Aborted 都转换成普通工具错误继续模型循环。
    /// future 必须 cancellation-safe：取消/超时时执行包装会取消子 token，宽限后丢弃 future。
    /// 实现须通过 RAII 或有界清理回收自身进程/任务，不能脱离 Turn 留下执行。
    fn run<'a>(
        &'a self,
        tc: ToolContext<'a>,
        input: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>>;
}

/// Registered execution handle. The implementation backend is held privately.
#[derive(Clone)]
pub struct ToolBinding {
    backend: Arc<dyn ToolHandler>,
}
impl ToolBinding {
    pub fn new(backend: Arc<dyn ToolHandler>) -> Self {
        Self { backend }
    }
    pub fn name(&self) -> &str {
        self.backend.name()
    }
    pub fn definition(&self) -> Tool {
        self.backend.definition()
    }
    pub fn security_context(&self, input: &Value) -> SecurityContext {
        self.backend.security_context(input)
    }
    #[doc(hidden)]
    pub fn owns_backend(&self, backend: &Arc<dyn ToolHandler>) -> bool {
        Arc::ptr_eq(&self.backend, backend)
    }
    pub fn same_backend(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.backend, &other.backend)
    }
    /// Public execution always delegates to the supplied framework operation.
    pub fn exec<'a>(
        &'a self,
        operation: &'a mut dyn ToolOperation,
        call: crate::chat::ToolCall,
    ) -> BoxFuture<'a, Result<ExecutedTool, YourAiError>> {
        operation.exec_bound(self, self.backend.clone(), call)
    }
}
#[derive(Debug, Clone)]
pub struct ExecutedTool {
    pub call: crate::chat::ToolCall,
    pub output: Value,
    pub is_error: bool,
}
/// Infrastructure bridge between the Core handle and Harness execution.
/// Tool authors implement ToolHandler; ordinary callers use ToolExecutor::exec.
#[doc(hidden)]
pub trait ToolOperation: Send {
    fn exec_bound<'a>(
        &'a mut self,
        binding: &'a ToolBinding,
        backend: Arc<dyn ToolHandler>,
        call: crate::chat::ToolCall,
    ) -> BoxFuture<'a, Result<ExecutedTool, YourAiError>>;
}

/// Registration is the implementation side; lookup returns an opaque execution handle.
pub trait ToolRegistry: Send + Sync {
    fn register(&self, handler: Arc<dyn ToolHandler>) {
        self.register_binding(ToolBinding::new(handler));
    }
    fn register_binding(&self, binding: ToolBinding);
    fn extend(&self, handlers: Vec<Arc<dyn ToolHandler>>) {
        for h in handlers {
            self.register(h);
        }
    }
    fn unregister(&self, name: &str);
    fn has(&self, name: &str) -> bool;
    fn definitions(&self) -> Vec<Tool>;
    fn resolve(&self, name: &str) -> Result<ToolBinding, YourAiError>;
    fn security_context(&self, name: &str, input: &Value) -> Result<SecurityContext, YourAiError> {
        Ok(self.resolve(name)?.security_context(input))
    }
    fn count(&self) -> usize;
}

/// 便捷构造：工具执行失败 → 错误 ToolResponse（loop 用）
pub fn tool_error_response(call_id: &str, name: &str, err: &YourAiError) -> ToolResponse {
    ToolResponse::new(call_id, format!("ERROR: {err}")).with_fn_name(name.to_string())
}
