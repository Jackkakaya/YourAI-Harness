//! # yourai-core
//!
//! YourAI 机制层：**业务接口 + 固定执行模板 + turn 运输机制。**
//! （机制实现——执行、Hook、会话/工作区生命周期、spawn、channel 等——属于本 crate 职责；
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
pub mod completion;
pub mod context;
pub mod context_manager;
pub mod error;
pub mod execution;
pub mod file_store;
pub mod future;
pub mod hooks;
pub mod interaction;
pub mod memory;
pub mod model;
pub mod model_error;
pub mod observability;
pub mod permission;
pub mod protocol;
pub mod runtime;
pub mod runtime_event;
pub mod sandbox;
pub mod security;
pub mod session;
pub mod session_runtime;
pub mod skill;
pub mod subagent;
pub mod tasks;
mod time;
pub mod tool;
pub mod tool_output;
pub mod turn;
pub mod ui;
pub mod usage;
pub mod workspace;

/// genai 类型 re-export（决策 5.6）：下游一律写 `yourai_core::chat::Xxx`
pub mod chat {
    pub use genai::chat::{
        Binary, BinarySource, ChatMessage, ChatOptions, ChatRequest, ChatResponse, ChatRole,
        ChatStreamEvent, ChatStreamResponse, ContentPart, MessageContent, StopReason, StreamChunk,
        StreamEnd, Tool as ToolDefinition, ToolCall, ToolChoice, ToolResponse, Usage as GenaiUsage,
    };
}

pub use agent_loop::{TurnFailure, TurnResult};
pub use error::{AbortReason, ErrorKind, YourAiError};
pub use future::BoxFuture;
pub use model::{ModelErrorClass, ModelRequest};
pub use ui::OutSink;

/// 一步式引入全部常用项
pub mod prelude {
    pub use crate::agent_loop::{AgentLoop, TurnFailure, TurnOutput, TurnResult};
    pub use crate::chat::*;
    pub use crate::compaction::{
        CompactAction, CompactionRequest, CompactionResult, CompactionTrigger, ContextPolicy,
    };
    pub use crate::completion::Completion;
    pub use crate::context::{
        Agent, AgentBuilder, ProviderSnapshot, Providers, TurnContext, TurnHandle,
    };
    pub use crate::context_manager::{
        CompactionJob, CompactionPlan, Compactor, ContextManager, ContextRequest,
    };
    pub use crate::error::{AbortReason, ErrorKind, YourAiError};
    pub use crate::execution::Turn;
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
    pub use crate::model::{
        Model, ModelErrorClass, ModelEventStream, ModelLimits, ModelOptions, ModelOutput,
        ModelProvider, ModelRecovery, ModelRequest, ModelTimeouts, ModelTokenBudget,
    };
    pub use crate::observability::{ObservabilityProvider, Span};
    pub use crate::protocol::{
        AttachmentData, FileRef, In, InputMode, Level, Out, Usage, UserAttachment,
    };
    pub use crate::sandbox::{SandboxPolicy, SandboxProvider, SandboxType};
    pub use crate::security::{
        ApprovalDecision, PolicyDecision, SecurityContext, SecurityProvider,
    };
    pub use crate::session::{
        CommitStatus, CompactionChange, ContextChange, MessagePage, MessageQuery, MessageStatus,
        RequestObservation, SessionId, SessionManager, SessionMeta, StoredMessage,
    };
    pub use crate::session_runtime::{InputRejected, SessionContext, SessionStatus, SessionTurn};
    pub use crate::skill::{SkillContent, SkillInfo, SkillProvider};
    pub use crate::tasks::{Task, TaskManager};
    pub use crate::tool::{ExecutedTool, Tool, ToolContext, ToolProvider, ToolRegistry};
    pub use crate::turn::{InputOptions, TurnId, TurnInfo, TurnLimits, TurnOptions};
    pub use crate::ui::OutSink;
    pub use crate::usage::{UsageEvent, UsageStats, UsageTracker};
}

pub(crate) fn error(name: &'static str, e: impl std::fmt::Display) -> YourAiError {
    ErrorKind::Provider {
        name,
        message: e.to_string(),
    }
    .into()
}
