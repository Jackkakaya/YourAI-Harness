//! 工具：定义侧（给模型的 schema）与执行侧（ToolContext 赋能）分离。
//!
//! 执行签名带 [`ToolContext`]——工具有自己的身份（call_id）、能发进度、
//! 能被取消、能拿到两层审批能力（决策 5.5 + 收敛修订）。
//!
//! **两层审批**（架构收敛结论）：
//! - 第一层（loop）：`check_tool_call`——loop 在调度工具前统一问
//! - 第二层（具体工具）：`check_command` / `check_file_access` 与
//!   `sandbox.apply(&mut Command)`——只有工具自己知道 Command/路径，
//!   由工具经 [`ToolContext`] 注入的能力自行调用
//!
//! 工具仍然不知道 UI 存在——只对 outbox 讲协议（Ask/Reply 也经 loop 中介）。

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

/// 工具执行期的能力注入（由 loop 在执行时装配）。
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
    /// 工具执行期间请求用户交互；Loop 必须同时服务其内部请求通道。
    /// None 表示不支持交互，ask() 立即报 Config，而不是永久等待。
    pub interaction: Option<&'a dyn ToolInteraction>,
}

impl ToolContext<'_> {
    /// 发一条进度事件（id 自动取 call_id）；返回 false = 消费端已关闭
    pub fn emit_progress(&self, payload: Value) -> bool {
        self.emit.send(yourai_protocol::Out::ToolProgress {
            id: self.call_id.clone(),
            payload,
        })
    }

    /// 向 Loop 提问，自动生成独立请求 id 并绑定本次工具调用。
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

pub trait ToolHandler: Send + Sync {
    /// 工具名（模型调用时使用）
    fn name(&self) -> &str;

    /// 给模型看的 schema（genai::chat::Tool）
    fn definition(&self) -> Tool;

    /// 为第一层审批描述这次调用。
    ///
    /// loop 在执行前通过 registry 调用本方法，再把结果交给
    /// `SecurityProvider::check_tool_call`。工具必须显式标注破坏性和网络属性，
    /// 避免 loop 根据名字猜测安全语义。
    fn security_context(&self, input: &Value) -> SecurityContext;

    /// 普通错误由 Loop 转成工具失败结果；Turn 取消/总超时/额度耗尽向上收尾，
    /// 不能把所有 Aborted 都转换成普通工具错误继续模型循环。
    /// future 必须 cancellation-safe：取消/超时时 Loop 会丢弃它并取消子 token。
    /// 实现须通过 RAII 或有界清理回收自身进程/任务，不能脱离 Turn 留下执行。
    fn execute<'a>(
        &'a self,
        tc: ToolContext<'a>,
        input: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>>;
}

pub trait ToolRegistry: Send + Sync {
    fn register(&self, handler: Arc<dyn ToolHandler>);
    /// 批量注册（MCP connect → Vec 一批进）
    fn extend(&self, handlers: Vec<Arc<dyn ToolHandler>>) {
        for h in handlers {
            self.register(h);
        }
    }
    fn unregister(&self, name: &str);
    fn has(&self, name: &str) -> bool;
    /// 所有工具的 schema（发给模型）
    fn definitions(&self) -> Vec<Tool>;
    /// 一次调用只解析一次。Loop 持有返回的 Arc 完成描述、审批和执行，
    /// 期间 register/unregister 不得改变这次调用的目标。
    fn resolve(&self, name: &str) -> Result<Arc<dyn ToolHandler>, YourAiError>;
    /// 获取一次调用的第一层审批上下文（handler 查找由 registry 负责）。
    /// 便捷查询；完整审批流程应使用同一个 resolve() 返回的 handler。
    fn security_context(&self, name: &str, input: &Value) -> Result<SecurityContext, YourAiError> {
        Ok(self.resolve(name)?.security_context(input))
    }
    /// 统一执行入口：registry 负责 handler 查找与调用；
    /// 错误到 ToolResponse 的转换由 loop 统一完成。
    fn execute<'a>(
        &'a self,
        tc: ToolContext<'a>,
        name: &'a str,
        input: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move { self.resolve(name)?.execute(tc, input).await })
    }
    fn count(&self) -> usize;
}

/// 便捷构造：工具执行失败 → 错误 ToolResponse（loop 用）
pub fn tool_error_response(call_id: &str, name: &str, err: &YourAiError) -> ToolResponse {
    ToolResponse::new(call_id, format!("ERROR: {err}")).with_fn_name(name.to_string())
}
