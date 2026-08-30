//! 两级错误（决策 5.7）。
//!
//! 第一级按**性质**二分：[`YourAiError::Aborted`]（终止，不是故障）与
//! [`YourAiError::Error`]（一切故障）。消费方只看第一级；
//! 来源细分在第二级 [`ErrorKind`]。

/// 框架统一错误。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum YourAiError {
    /// 终止——不是故障（ESC、消费端消失、关停）。前端渲染为灰色"已停止"。
    #[error("turn aborted: {0}")]
    Aborted(#[from] AbortReason),

    /// 一切故障的统一入口。前端渲染为红色错误。
    #[error("{0}")]
    Error(#[from] ErrorKind),
}

/// 终止的原因。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AbortReason {
    /// cancel token 触发（ESC / Shutdown）
    #[error("cancelled")]
    Cancelled,

    /// inbox/outbox 对端消失（调用方已离开）
    #[error("client disconnected")]
    Disconnected,
}

/// 故障来源细分（第二级）。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ErrorKind {
    /// LLM 失败——唯一有具体类型的下沉点，原样携带 genai 错误
    #[error("model error: {source}")]
    Model {
        #[from]
        source: genai::Error,
    },

    /// 其他 provider 运行失败（统一形状，不强造子类）
    #[error("provider '{name}' failed: {message}")]
    Provider { name: &'static str, message: String },

    /// 工具执行失败（DefaultLoop 消化为 ToolDone{is_error}，不逃逸成 turn 失败）
    #[error("tool '{name}' failed: {message}")]
    Tool { name: String, message: String },

    /// 装配/配置错误：缺必需 provider、参数非法——在**使用点**报（决策 5.8）
    #[error("config error: {0}")]
    Config(String),

    /// loop 自身逻辑错误
    #[error("loop error: {0}")]
    Loop(String),

    /// 兜底
    #[error("{0}")]
    Other(String),
}
