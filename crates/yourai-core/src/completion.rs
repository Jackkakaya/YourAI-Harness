//! 完成操作的公共入口（固定缝）。
//!
//! Stop 的生命周期（反馈可续跑、附加上下文、最终提交）由缝实现持有；
//! 完成状态机（未结工具校验、局部状态保持）是运行时业务。

use crate::error::YourAiError;
use crate::future::BoxFuture;

/// 一次完成请求的结果：完成，或 hook 反馈要求继续工作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    Completed,
    NeedsMoreWork,
}

/// Infrastructure bridge between the Core entry and a runtime's execution.
#[doc(hidden)]
pub trait CompletionOperation: Send {
    fn complete_bound<'a>(
        &'a mut self,
        text: String,
    ) -> BoxFuture<'a, Result<Completion, YourAiError>>;
}

/// 固定公共入口：接受一个业务答案（Stop 生命周期在框架操作内）。
pub fn complete<'a>(
    operation: &'a mut dyn CompletionOperation,
    text: String,
) -> BoxFuture<'a, Result<Completion, YourAiError>> {
    operation.complete_bound(text)
}
