//! # yourai-core
//!
//! YourAI 机制层：**接口定义（trait）+ turn 运输机制，零业务 Provider 实现。**
//! （机制实现——spawn、channel、句柄、SessionId 等——属于本 crate 职责；
//! 业务 Provider 一律在外部 crate。）
//!
//! 分工（三权分立，见 docs/architecture.md §3.1）：
//! - 本 crate 拥有**运输权**：双流（inbox/outbox）、取消、句柄、生命周期
//! - loop 拥有**解释权**：实现 [`agent_loop::AgentLoop`]，决定消息语义与消费时机
//! - 前端拥有**节奏权**：何时 `start`、串行、渲染
//!
//! Shared In/Out messages live in [`protocol`] and are re-exported by the prelude.
//! genai 类型按决策 5.6 从这里 re-export，下游不直接依赖 genai。
//!
//! # 机制速览（可运行 doctest）
//!
//! ```
//! use std::sync::Arc;
//! use yourai_core::prelude::*;
//!
//! struct EchoLoop;
//!
//! impl AgentLoop for EchoLoop {
//!     fn run_turn<'a>(
//!         &'a self,
//!         tc: TurnContext<'a>,
//!     ) -> BoxFuture<'a, TurnResult> {
//!         Box::pin(async move {
//!             match tc.inbox.recv().await {
//!                 Some(In::UserText { text, .. }) => {
//!                     let alive = tc.outbox.send(Out::Chunk { text: format!("echo: {text}") });
//!                     if !alive {
//!                         return Err(YourAiError::Aborted(AbortReason::Disconnected).into());
//!                     }
//!                     Ok(TurnOutput::new(format!("done: {text}")))
//!                 }
//!                 _ => Err(ErrorKind::Loop("expected UserText".into()).into()),
//!             }
//!         })
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     let agent = Agent::builder().agent_loop(Arc::new(EchoLoop)).build();
//!     let mut handle = agent.start(In::user_text("hi")).unwrap();
//!     while let Some(_event) = handle.outbox.recv().await {} // outbox 关闭 = turn 结束
//!     let out = handle.join().await.unwrap();
//!     assert_eq!(out.text, "done: hi");
//! }
//! ```

pub mod agent_loop;
pub mod compaction;
pub mod context;
pub mod context_manager;
pub mod error;
pub mod future;
pub mod hooks;
pub mod interaction;
pub mod memory;
pub mod model;
pub mod observability;
pub mod protocol;
pub mod runtime_event;
pub mod sandbox;
pub mod security;
pub mod session;
pub mod session_runtime;
pub mod skill;
pub mod tool;
pub mod turn;
pub mod ui;
pub mod usage;

/// genai 类型 re-export（决策 5.6）：下游一律写 `yourai_core::chat::Xxx`
pub mod chat {
    pub use genai::chat::{
        Binary, BinarySource, ChatMessage, ChatOptions, ChatRequest, ChatResponse, ChatRole,
        ChatStreamEvent, ChatStreamResponse, ContentPart, MessageContent, StopReason, StreamChunk,
        StreamEnd, Tool, ToolCall, ToolResponse, Usage as GenaiUsage,
    };
}

pub use agent_loop::{TurnFailure, TurnResult};
pub use error::{AbortReason, ErrorKind, YourAiError};
pub use future::BoxFuture;
pub use model::ModelRequest;
pub use ui::OutSink;

/// 一步式引入全部常用项
pub mod prelude {
    pub use crate::agent_loop::{AgentLoop, TurnFailure, TurnOutput, TurnResult};
    pub use crate::chat::*;
    pub use crate::compaction::{
        CompactAction, CompactionRequest, CompactionResult, CompactionTrigger, ContextPolicy,
    };
    pub use crate::context::{
        Agent, AgentBuilder, Context, ProviderSnapshot, TurnContext, TurnHandle,
    };
    pub use crate::context_manager::{ContextExecution, ContextManager, ContextRequest};
    pub use crate::error::{AbortReason, ErrorKind, YourAiError};
    pub use crate::future::BoxFuture;
    pub use crate::hooks::{
        BaseInput, FailurePolicy, HookBlockingError, HookCommonOutcome, HookDispatchResult,
        HookEvent, HookEventKind, HookHandler, HookInvocation, HookMessage, HookMessageKind,
        HookOutput, HookPermission, HookPointOutcome, HookRegistry, HookRun, HookRunStatus,
        HookRuntime, HookSource, NativeHookRegistration,
    };
    pub use crate::interaction::{InteractionKind, InteractionRequest, ToolInteraction};
    pub use crate::memory::{
        CompletedMemoryTurn, MemoryEntry, MemoryManager, MemoryProvider, MemorySession,
        RecallRequest, RecalledMemory,
    };
    pub use crate::model::{ModelEventStream, ModelProvider, ModelRecovery, ModelRequest};
    pub use crate::observability::{ObservabilityProvider, Span};
    pub use crate::protocol::{
        In, InputMode, Level, Out, Usage, UserAttachment, MAX_USER_ATTACHMENT_BYTES,
    };
    pub use crate::sandbox::{SandboxPolicy, SandboxProvider, SandboxType};
    pub use crate::security::{
        ApprovalDecision, PolicyDecision, SecurityContext, SecurityProvider,
    };
    pub use crate::session::{
        CompactionChange, ContextChange, MessagePage, MessageQuery, MessageStatus,
        RequestObservation, SessionId, SessionManager, SessionMeta, StoredMessage,
    };
    pub use crate::session_runtime::{
        InputRejected, SessionContext, SessionRuntime, SessionStatus, SessionTurn,
    };
    pub use crate::skill::{SkillContent, SkillInfo, SkillProvider};
    pub use crate::tool::{ToolContext, ToolHandler, ToolRegistry};
    pub use crate::turn::{TurnId, TurnInfo, TurnLimits, TurnOptions};
    pub use crate::ui::OutSink;
    pub use crate::usage::{UsageEvent, UsageStats, UsageTracker};
}
