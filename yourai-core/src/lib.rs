//! # yourai-core
//!
//! YourAI 机制层：**纯接口定义（trait）+ turn 运输机制，零业务实现。**
//!
//! 分工（三权分立，见 docs/architecture.md §3.1）：
//! - 本 crate 拥有**运输权**：双流（inbox/outbox）、取消、句柄、生命周期
//! - loop 拥有**解释权**：实现 [`agent_loop::AgentLoop`]，决定消息语义与消费时机
//! - 前端拥有**节奏权**：何时 `start`、串行、渲染
//!
//! 词汇（In/Out）在 `yourai-protocol`（本 crate 依赖但不拥有）；
//! genai 类型按决策 5.6 从这里 re-export，下游不直接依赖 genai。

pub mod agent_loop;
pub mod context;
pub mod context_manager;
pub mod error;
pub mod future;
pub mod hooks;
pub mod memory;
pub mod model;
pub mod observability;
pub mod sandbox;
pub mod security;
pub mod session;
pub mod skill;
pub mod tool;
pub mod ui;
pub mod usage;

/// genai 类型 re-export（决策 5.6）：下游一律写 `yourai_core::chat::Xxx`
pub mod chat {
    pub use genai::chat::{
        ChatMessage, ChatOptions, ChatRequest, ChatResponse, ChatRole, ChatStreamEvent,
        ChatStreamResponse, MessageContent, Tool, ToolCall, Usage as GenaiUsage,
    };
}

pub use error::{AbortReason, ErrorKind, YourAiError};
pub use future::BoxFuture;
pub use ui::OutSink;

/// 一步式引入全部常用项
pub mod prelude {
    pub use crate::agent_loop::{AgentLoop, TurnOutput};
    pub use crate::chat::*;
    pub use crate::context::{Agent, AgentBuilder, Context, TurnContext, TurnHandle};
    pub use crate::context_manager::ContextManager;
    pub use crate::error::{AbortReason, ErrorKind, YourAiError};
    pub use crate::future::BoxFuture;
    pub use crate::hooks::{HookEvent, HookEventType, HookHandler, HookOutcome, HookRegistry};
    pub use crate::memory::{MemoryEntry, MemoryManager};
    pub use crate::model::ModelProvider;
    pub use crate::observability::{ObservabilityProvider, Span};
    pub use crate::sandbox::{SandboxPolicy, SandboxProvider, SandboxType};
    pub use crate::security::{ApprovalDecision, SecurityContext, SecurityProvider};
    pub use crate::session::{SessionId, SessionManager, SessionMeta};
    pub use crate::skill::{SkillContent, SkillInfo, SkillProvider};
    pub use crate::tool::{ToolContext, ToolHandler, ToolRegistry};
    pub use crate::ui::OutSink;
    pub use crate::usage::{UsageStats, UsageTracker};
    pub use yourai_protocol::{In, Level, Out, Usage};
}
