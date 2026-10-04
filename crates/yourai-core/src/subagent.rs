//! 子代理执行的公共入口（固定缝）。
//!
//! 业务实现管理子会话的打开、运行与收尾；SubagentStart / SubagentStop 的
//! hook 生命周期（Start 阻断、Stop 反馈可续跑同一子会话）由缝实现持有。

use crate::error::YourAiError;
use crate::future::BoxFuture;
use crate::tool::ToolContext;
use serde_json::Value;

/// Infrastructure bridge between the Core entry and a runtime's execution.
/// Subagent runners implement the child-session business; ordinary callers
/// use [`exec_child`], which owns the SubagentStart/SubagentStop lifecycle.
#[doc(hidden)]
pub trait SubagentOperation: Send + Sync {
    fn exec_child_bound<'a>(
        &'a self,
        tc: ToolContext<'a>,
        prompt: String,
    ) -> BoxFuture<'a, Result<Value, YourAiError>>;
}

/// 固定公共入口：执行一个子代理。始终委托给注入的框架操作。
pub fn exec_child<'a>(
    operation: &'a dyn SubagentOperation,
    tc: ToolContext<'a>,
    prompt: String,
) -> BoxFuture<'a, Result<Value, YourAiError>> {
    operation.exec_child_bound(tc, prompt)
}
