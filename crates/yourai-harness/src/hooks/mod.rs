//! # yourai-harness::hooks
//!
//! Claude Code 兼容的 Hook 系统实现：wire 协议 + 运行时 + command/HTTP 执行器。
//!
//! **协议类型和 trait 接口在 `yourai-core::hooks`**；本模块只提供实现。
//!
//! ## 架构
//!
//! ```text
//! yourai-core::hooks (协议/接口)
//!   ├── HookEvent, HookInvocation, BaseInput        (输入类型)
//!   ├── HookCommonOutcome, HookPermission, HookPointOutcome (结果类型)
//!   ├── HookDispatchResult, HookRun, HookRunStatus   (返回类型)
//!   ├── HookHandler trait, HookManager trait          (接口)
//!   └── HookSource, FailurePolicy, HookOutput         (元数据)
//!
//! yourai-harness::hooks (实现)
//!   ├── DefaultHookManager  impl HookManager         (运行时)
//!   ├── CommandHandler       impl HookHandler          (command 执行器)
//!   ├── HttpHandler          impl HookHandler          (HTTP 执行器)
//!   ├── NativeHandler        impl HookHandler          (Rust 闭包)
//!   ├── wire_output          (Claude wire JSON 反序列化)
//!   ├── matcher              (CompiledMatcher)
//!   ├── registration         (定义校验与运行注册)
//!   └── event                (wire JSON 序列化)
//! ```
//!
//! ## 用法
//!
//! ```no_run
//! use yourai_core::hooks::*;
//! use yourai_harness::hooks::*;
//!
//! # async fn example() {
//! let runtime = DefaultHookManager::new();
//!
//! // 从 Claude 兼容的 settings.json 注册
//! let config: serde_json::Value = serde_json::from_str(r#"{
//!   "hooks": {
//!     "PreToolUse": [
//!       { "matcher": "Bash", "hooks": [
//!         { "type": "command", "command": "python3 check.py", "timeout": 10 }
//!       ]}
//!     ]
//!   }
//! }"#).unwrap();
//! runtime.register_config(&config, HookSource::Project).await.unwrap();
//!
//! // 分发（通过 HookManager trait）
//! let inv = HookInvocation::new(
//!     BaseInput::new("sess_01", "/workspace"),
//!     HookEvent::PreToolUse {
//!         tool_name: "Bash".to_string(),
//!         tool_input: serde_json::json!({"command": "ls"}),
//!         tool_use_id: "call_01".to_string(),
//!     },
//! );
//! let result = HookManager::dispatch(&runtime, &inv).await.unwrap();
//!
//! match result.outcome {
//!     HookPointOutcome::PreToolUse(o) => match o.permission {
//!         HookPermission::Deny { reason } => println!("blocked: {reason}"),
//!         HookPermission::Ask { reason } => println!("ask user: {reason}"),
//!         HookPermission::Allow { .. } => println!("allowed"),
//!         HookPermission::Pass => println!("no opinion"),
//!         _ => {}
//!     },
//!     _ => {}
//! }
//! # }
//! ```

pub mod command;
pub mod event;
pub mod handler;
pub mod http;
pub mod matcher;
pub mod registration;
pub mod runtime;
pub mod wire_output;

/// Cap on hook command/HTTP output so a misbehaving hook cannot exhaust memory.
pub(crate) const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

// ── Re-exports：实现层类型 ──────────────────────────────────────────

pub use command::{BackgroundHookEvent, CommandHandler};
pub use handler::NativeHandler;
pub use http::{HttpHandler, HttpHookPolicy};
pub use matcher::CompiledMatcher;
pub use registration::{build_registrations, HandlerConfig, HookRegistration, HookShell};
pub use runtime::DefaultHookManager;

// ── Re-exports：从 yourai-core 透传协议类型 ─────────────────────────
// 用户只需 `use crate::hooks::*` 即可拿到全部类型，不必同时 depend yourai-core

pub use yourai_core::hooks::{
    BaseInput, ElicitationOutcome, FailurePolicy, HookBlockingError, HookCommonOutcome,
    HookDispatchResult, HookEvent, HookEventKind, HookHandler, HookInvocation, HookManager,
    HookMessage, HookMessageKind, HookOutput, HookPermission, HookPointOutcome, HookRun,
    HookRunStatus, HookSource, NativeHookRegistration, PermissionDeniedOutcome,
    PermissionRequestBehavior, PermissionRequestDecision, PermissionRequestOutcome,
    PostToolUseOutcome, PreToolUseOutcome, SessionStartOutcome, WatchPathsOutcome,
    WorktreeCreateOutcome,
};
