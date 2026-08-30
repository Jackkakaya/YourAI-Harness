//! Hook 协议层：事件类型 + 结果类型 + trait 接口。
//!
//! **本模块只定义协议和接口，不含任何实现。**
//! 具体实现（command 执行器、HTTP 执行器、运行时注册表）在 `yourai-hooks` crate。
//!
//! ## 协议层次
//!
//! ```text
//! Loop 构造 HookInvocation（类型化）
//!   → HookRuntime::dispatch（trait，实现方在 yourai-hooks）
//!   → 返回 HookDispatchResult（类型化）
//!   → Loop 消费 outcome
//! ```
//!
//! 外部 wire 协议（snake_case 输入 / camelCase 输出）是实现细节，
//! 由 `yourai-hooks` 的 `wire_output` 模块负责序列化/反序列化。

use crate::error::YourAiError;
use crate::future::BoxFuture;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

// ════════════════════════════════════════════════════════════════════
//  输入：BaseInput + HookEvent + HookInvocation
// ════════════════════════════════════════════════════════════════════

/// 所有事件共享的公共字段（对应 Claude `BaseHookInput`）。
#[derive(Debug, Clone)]
pub struct BaseInput {
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    pub permission_mode: Option<String>,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
}

impl BaseInput {
    pub fn new(session_id: impl Into<String>, cwd: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            transcript_path: String::new(),
            cwd: cwd.into(),
            permission_mode: None,
            agent_id: None,
            agent_type: None,
        }
    }
}

/// 类型化 Hook 事件。每个变体对应一个 `hook_event_name`。
///
/// `#[non_exhaustive]` 保证未来加事件不炸下游穷举 match。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum HookEvent {
    PreToolUse {
        tool_name: String,
        tool_input: Value,
        tool_use_id: String,
    },
    PostToolUse {
        tool_name: String,
        tool_input: Value,
        tool_response: Value,
        tool_use_id: String,
    },
    PostToolUseFailure {
        tool_name: String,
        tool_input: Value,
        tool_use_id: String,
        error: String,
        is_interrupt: Option<bool>,
    },
    PermissionRequest {
        tool_name: String,
        tool_input: Value,
        permission_suggestions: Option<Vec<Value>>,
    },
    PermissionDenied {
        tool_name: String,
        tool_input: Value,
        tool_use_id: String,
        reason: String,
    },
    Notification {
        message: String,
        title: Option<String>,
        notification_type: String,
    },
    UserPromptSubmit {
        prompt: String,
    },
    SessionStart {
        source: String,
        model: Option<String>,
    },
    SessionEnd {
        reason: String,
    },
    Stop {
        stop_hook_active: bool,
        last_assistant_message: Option<String>,
    },
    StopFailure {
        error: String,
        error_details: Option<String>,
        last_assistant_message: Option<String>,
    },
    SubagentStart {
        agent_id: String,
        agent_type: String,
    },
    SubagentStop {
        stop_hook_active: bool,
        agent_id: String,
        agent_transcript_path: String,
        agent_type: String,
        last_assistant_message: Option<String>,
    },
    PreCompact {
        trigger: String,
        custom_instructions: Option<String>,
    },
    PostCompact {
        trigger: String,
        compact_summary: String,
    },
    Setup {
        trigger: String,
    },
    TeammateIdle {
        teammate_name: String,
        team_name: String,
    },
    TaskCreated {
        task_id: String,
        task_subject: String,
        task_description: Option<String>,
        teammate_name: Option<String>,
        team_name: Option<String>,
    },
    TaskCompleted {
        task_id: String,
        task_subject: String,
        task_description: Option<String>,
        teammate_name: Option<String>,
        team_name: Option<String>,
    },
    Elicitation {
        mcp_server_name: String,
        message: String,
        mode: Option<String>,
        url: Option<String>,
        elicitation_id: Option<String>,
        requested_schema: Option<Value>,
    },
    ElicitationResult {
        mcp_server_name: String,
        elicitation_id: Option<String>,
        mode: Option<String>,
        action: String,
        content: Option<Value>,
    },
    ConfigChange {
        source: String,
        file_path: Option<String>,
    },
    InstructionsLoaded {
        file_path: String,
        memory_type: String,
        load_reason: String,
        globs: Option<Vec<String>>,
        trigger_file_path: Option<String>,
        parent_file_path: Option<String>,
    },
    WorktreeCreate {
        name: String,
    },
    WorktreeRemove {
        worktree_path: String,
    },
    CwdChanged {
        old_cwd: String,
        new_cwd: String,
    },
    FileChanged {
        file_path: String,
        event: String,
    },
}

/// Claude Hook 事件判别器。与 wire 字段 `hook_event_name` 一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HookEventKind {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    PermissionRequest,
    PermissionDenied,
    Notification,
    UserPromptSubmit,
    SessionStart,
    SessionEnd,
    Stop,
    StopFailure,
    SubagentStart,
    SubagentStop,
    PreCompact,
    PostCompact,
    Setup,
    TeammateIdle,
    TaskCreated,
    TaskCompleted,
    Elicitation,
    ElicitationResult,
    ConfigChange,
    InstructionsLoaded,
    WorktreeCreate,
    WorktreeRemove,
    CwdChanged,
    FileChanged,
}

impl HookEventKind {
    pub const ALL: [Self; 27] = [
        Self::PreToolUse,
        Self::PostToolUse,
        Self::PostToolUseFailure,
        Self::PermissionRequest,
        Self::PermissionDenied,
        Self::Notification,
        Self::UserPromptSubmit,
        Self::SessionStart,
        Self::SessionEnd,
        Self::Stop,
        Self::StopFailure,
        Self::SubagentStart,
        Self::SubagentStop,
        Self::PreCompact,
        Self::PostCompact,
        Self::Setup,
        Self::TeammateIdle,
        Self::TaskCreated,
        Self::TaskCompleted,
        Self::Elicitation,
        Self::ElicitationResult,
        Self::ConfigChange,
        Self::InstructionsLoaded,
        Self::WorktreeCreate,
        Self::WorktreeRemove,
        Self::CwdChanged,
        Self::FileChanged,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::PermissionRequest => "PermissionRequest",
            Self::PermissionDenied => "PermissionDenied",
            Self::Notification => "Notification",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::Stop => "Stop",
            Self::StopFailure => "StopFailure",
            Self::SubagentStart => "SubagentStart",
            Self::SubagentStop => "SubagentStop",
            Self::PreCompact => "PreCompact",
            Self::PostCompact => "PostCompact",
            Self::Setup => "Setup",
            Self::TeammateIdle => "TeammateIdle",
            Self::TaskCreated => "TaskCreated",
            Self::TaskCompleted => "TaskCompleted",
            Self::Elicitation => "Elicitation",
            Self::ElicitationResult => "ElicitationResult",
            Self::ConfigChange => "ConfigChange",
            Self::InstructionsLoaded => "InstructionsLoaded",
            Self::WorktreeCreate => "WorktreeCreate",
            Self::WorktreeRemove => "WorktreeRemove",
            Self::CwdChanged => "CwdChanged",
            Self::FileChanged => "FileChanged",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|event| event.as_str() == name)
    }
}

impl HookEvent {
    pub fn kind(&self) -> HookEventKind {
        match self {
            HookEvent::PreToolUse { .. } => HookEventKind::PreToolUse,
            HookEvent::PostToolUse { .. } => HookEventKind::PostToolUse,
            HookEvent::PostToolUseFailure { .. } => HookEventKind::PostToolUseFailure,
            HookEvent::PermissionRequest { .. } => HookEventKind::PermissionRequest,
            HookEvent::PermissionDenied { .. } => HookEventKind::PermissionDenied,
            HookEvent::Notification { .. } => HookEventKind::Notification,
            HookEvent::UserPromptSubmit { .. } => HookEventKind::UserPromptSubmit,
            HookEvent::SessionStart { .. } => HookEventKind::SessionStart,
            HookEvent::SessionEnd { .. } => HookEventKind::SessionEnd,
            HookEvent::Stop { .. } => HookEventKind::Stop,
            HookEvent::StopFailure { .. } => HookEventKind::StopFailure,
            HookEvent::SubagentStart { .. } => HookEventKind::SubagentStart,
            HookEvent::SubagentStop { .. } => HookEventKind::SubagentStop,
            HookEvent::PreCompact { .. } => HookEventKind::PreCompact,
            HookEvent::PostCompact { .. } => HookEventKind::PostCompact,
            HookEvent::Setup { .. } => HookEventKind::Setup,
            HookEvent::TeammateIdle { .. } => HookEventKind::TeammateIdle,
            HookEvent::TaskCreated { .. } => HookEventKind::TaskCreated,
            HookEvent::TaskCompleted { .. } => HookEventKind::TaskCompleted,
            HookEvent::Elicitation { .. } => HookEventKind::Elicitation,
            HookEvent::ElicitationResult { .. } => HookEventKind::ElicitationResult,
            HookEvent::ConfigChange { .. } => HookEventKind::ConfigChange,
            HookEvent::InstructionsLoaded { .. } => HookEventKind::InstructionsLoaded,
            HookEvent::WorktreeCreate { .. } => HookEventKind::WorktreeCreate,
            HookEvent::WorktreeRemove { .. } => HookEventKind::WorktreeRemove,
            HookEvent::CwdChanged { .. } => HookEventKind::CwdChanged,
            HookEvent::FileChanged { .. } => HookEventKind::FileChanged,
        }
    }

    /// 返回 Claude 协议中的事件名字符串（`hook_event_name` 字段值）。
    pub fn event_name(&self) -> &'static str {
        self.kind().as_str()
    }

    /// 返回该事件用于 matcher 匹配的字符串。无 matcher 的事件返回 `None`。
    pub fn match_query(&self) -> Option<&str> {
        match self {
            HookEvent::PreToolUse { tool_name, .. }
            | HookEvent::PostToolUse { tool_name, .. }
            | HookEvent::PostToolUseFailure { tool_name, .. }
            | HookEvent::PermissionRequest { tool_name, .. }
            | HookEvent::PermissionDenied { tool_name, .. } => Some(tool_name),
            HookEvent::SessionStart { source, .. } => Some(source),
            HookEvent::Setup { trigger } => Some(trigger),
            HookEvent::PreCompact { trigger, .. } | HookEvent::PostCompact { trigger, .. } => {
                Some(trigger)
            }
            HookEvent::Notification {
                notification_type, ..
            } => Some(notification_type),
            HookEvent::SessionEnd { reason } => Some(reason),
            HookEvent::StopFailure { error, .. } => Some(error),
            HookEvent::SubagentStart { agent_type, .. }
            | HookEvent::SubagentStop { agent_type, .. } => Some(agent_type),
            HookEvent::Elicitation {
                mcp_server_name, ..
            }
            | HookEvent::ElicitationResult {
                mcp_server_name, ..
            } => Some(mcp_server_name),
            HookEvent::ConfigChange { source, .. } => Some(source),
            HookEvent::InstructionsLoaded { load_reason, .. } => Some(load_reason),
            HookEvent::FileChanged { file_path, .. } => std::path::Path::new(file_path)
                .file_name()
                .and_then(|n| n.to_str()),
            HookEvent::UserPromptSubmit { .. }
            | HookEvent::Stop { .. }
            | HookEvent::TeammateIdle { .. }
            | HookEvent::TaskCreated { .. }
            | HookEvent::TaskCompleted { .. }
            | HookEvent::WorktreeCreate { .. }
            | HookEvent::WorktreeRemove { .. }
            | HookEvent::CwdChanged { .. } => None,
        }
    }
}

/// 一次完整的 Hook 调用：base 上下文 + 事件。
#[derive(Debug, Clone)]
pub struct HookInvocation {
    pub base: BaseInput,
    pub event: HookEvent,
}

impl HookInvocation {
    pub fn new(base: BaseInput, event: HookEvent) -> Self {
        Self { base, event }
    }

    pub fn event_name(&self) -> &'static str {
        self.event.event_name()
    }

    pub fn event_kind(&self) -> HookEventKind {
        self.event.kind()
    }
}

// ════════════════════════════════════════════════════════════════════
//  结果：公共效果 + HookPermission + 事件专属 Outcome
// ════════════════════════════════════════════════════════════════════

/// Hook 产生的模型/用户可见消息类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HookMessageKind {
    Success,
    NonBlockingError,
}

/// 一条普通 Hook 消息。plain text stdout 属于此通道，不是 additionalContext。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookMessage {
    pub hook_id: String,
    pub kind: HookMessageKind,
    pub content: String,
}

/// Blocking feedback。它阻断哪个操作由触发 Hook 的 Loop 消费点决定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookBlockingError {
    pub hook_id: String,
    pub message: String,
}

/// 所有事件共享的聚合结果。
///
/// continuation、blocking feedback 和工具权限必须分别表达；尤其
/// `PreToolUse permissionDecision=deny` 不得隐式设置 `prevent_continuation`。
#[derive(Debug, Clone, Default)]
pub struct HookCommonOutcome {
    pub prevent_continuation: bool,
    pub stop_reason: Option<String>,
    pub system_messages: Vec<String>,
    pub messages: Vec<HookMessage>,
    pub blocking_errors: Vec<HookBlockingError>,
}

/// PreToolUse 权限决策（由 Hook 贡献，最终与 SecurityProvider 合并）。
///
/// 聚合优先级：`deny > ask > allow`（与 Claude Code 一致）。
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub enum HookPermission {
    /// 不做权限决策，透传给后续审批流程。
    #[default]
    Pass,
    Allow {
        reason: Option<String>,
    },
    Ask {
        reason: String,
    },
    Deny {
        reason: String,
    },
}

impl HookPermission {
    /// 聚合优先级：`deny > ask > allow`。
    pub fn merge(self, other: &HookPermission) -> HookPermission {
        match (&self, other) {
            (HookPermission::Deny { .. }, HookPermission::Deny { .. }) => other.clone(),
            (HookPermission::Deny { .. }, _) => self.clone(),
            (_, HookPermission::Deny { .. }) => other.clone(),
            (HookPermission::Ask { .. }, HookPermission::Ask { .. }) => other.clone(),
            (HookPermission::Ask { .. }, _) => self.clone(),
            (_, HookPermission::Ask { .. }) => other.clone(),
            (HookPermission::Allow { .. }, HookPermission::Allow { .. }) => other.clone(),
            (HookPermission::Allow { .. }, _) => self.clone(),
            (_, HookPermission::Allow { .. }) => other.clone(),
            (HookPermission::Pass, HookPermission::Pass) => HookPermission::Pass,
        }
    }
}

// ── 事件专属 Outcome 结构体 ──────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct UserPromptSubmitOutcome {
    pub additional_contexts: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct PreToolUseOutcome {
    pub permission: HookPermission,
    pub updated_input: Option<Value>,
    pub additional_contexts: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct PostToolUseOutcome {
    pub additional_contexts: Vec<String>,
    pub updated_mcp_tool_output: Option<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionStartOutcome {
    pub additional_contexts: Vec<String>,
    pub initial_user_message: Option<String>,
    pub watch_paths: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct PermissionDeniedOutcome {
    pub retry: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PermissionRequestOutcome {
    pub decision: Option<PermissionRequestDecision>,
}

#[derive(Debug, Clone)]
pub struct PermissionRequestDecision {
    pub behavior: PermissionRequestBehavior,
    pub updated_input: Option<Value>,
    pub updated_permissions: Vec<Value>,
    pub message: Option<String>,
    pub interrupt: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionRequestBehavior {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Default)]
pub struct WatchPathsOutcome {
    pub watch_paths: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ElicitationOutcome {
    pub action: Option<String>,
    pub content: Option<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct WorktreeCreateOutcome {
    /// `None` 表示没有 Hook 覆盖默认创建位置。
    pub worktree_path: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct GenericOutcome {
    pub additional_contexts: Vec<String>,
}

// ── Outcome enum ────────────────────────────────────────────────────

/// 事件专属结果（聚合后）。Loop 按 `event_name` 取对应变体。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum HookPointOutcome {
    PreToolUse(PreToolUseOutcome),
    PostToolUse(PostToolUseOutcome),
    UserPromptSubmit(UserPromptSubmitOutcome),
    SessionStart(SessionStartOutcome),
    PermissionDenied(PermissionDeniedOutcome),
    PermissionRequest(PermissionRequestOutcome),
    CwdChanged(WatchPathsOutcome),
    FileChanged(WatchPathsOutcome),
    Elicitation(ElicitationOutcome),
    ElicitationResult(ElicitationOutcome),
    WorktreeCreate(WorktreeCreateOutcome),
    /// 没有专属 outcome 变体的事件（Stop, SessionEnd, Notification 等）。
    Generic(GenericOutcome),
}

impl HookPointOutcome {
    /// 返回该 outcome 对应的事件名。
    pub fn event_name(&self) -> &'static str {
        match self {
            HookPointOutcome::PreToolUse(_) => "PreToolUse",
            HookPointOutcome::PostToolUse(_) => "PostToolUse",
            HookPointOutcome::UserPromptSubmit(_) => "UserPromptSubmit",
            HookPointOutcome::SessionStart(_) => "SessionStart",
            HookPointOutcome::PermissionDenied(_) => "PermissionDenied",
            HookPointOutcome::PermissionRequest(_) => "PermissionRequest",
            HookPointOutcome::CwdChanged(_) => "CwdChanged",
            HookPointOutcome::FileChanged(_) => "FileChanged",
            HookPointOutcome::Elicitation(_) => "Elicitation",
            HookPointOutcome::ElicitationResult(_) => "ElicitationResult",
            HookPointOutcome::WorktreeCreate(_) => "WorktreeCreate",
            HookPointOutcome::Generic(_) => "Generic",
        }
    }
}

// ── Dispatch result + 执行记录 ──────────────────────────────────────

/// 一次 dispatch 的完整返回。
#[derive(Debug, Clone)]
pub struct HookDispatchResult {
    pub event: HookEventKind,
    pub common: HookCommonOutcome,
    pub outcome: HookPointOutcome,
    pub runs: Vec<HookRun>,
}

impl HookDispatchResult {
    /// 无 hook 匹配时的空结果。
    pub fn empty(event: HookEventKind) -> Self {
        let outcome = match event.as_str() {
            "PreToolUse" => HookPointOutcome::PreToolUse(PreToolUseOutcome::default()),
            "PostToolUse" => HookPointOutcome::PostToolUse(PostToolUseOutcome::default()),
            "UserPromptSubmit" => {
                HookPointOutcome::UserPromptSubmit(UserPromptSubmitOutcome::default())
            }
            "SessionStart" => HookPointOutcome::SessionStart(SessionStartOutcome::default()),
            "PermissionDenied" => {
                HookPointOutcome::PermissionDenied(PermissionDeniedOutcome::default())
            }
            "PermissionRequest" => {
                HookPointOutcome::PermissionRequest(PermissionRequestOutcome::default())
            }
            "CwdChanged" => HookPointOutcome::CwdChanged(WatchPathsOutcome::default()),
            "FileChanged" => HookPointOutcome::FileChanged(WatchPathsOutcome::default()),
            "Elicitation" => HookPointOutcome::Elicitation(ElicitationOutcome::default()),
            "ElicitationResult" => {
                HookPointOutcome::ElicitationResult(ElicitationOutcome::default())
            }
            "WorktreeCreate" => HookPointOutcome::WorktreeCreate(WorktreeCreateOutcome::default()),
            _ => HookPointOutcome::Generic(GenericOutcome::default()),
        };
        Self {
            event,
            common: HookCommonOutcome::default(),
            outcome,
            runs: vec![],
        }
    }
}

/// 单个 handler 的执行记录（用于日志/可观测性）。
#[derive(Debug, Clone)]
pub struct HookRun {
    pub hook_id: String,
    pub source: String,
    pub status: HookRunStatus,
    pub started_at: i64,
    pub duration: Duration,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub exit_code: Option<i32>,
    /// Hook 请求隐藏本次 stdout/stderr 的用户展示；结构化效果仍然生效。
    pub suppress_output: bool,
    pub status_message: Option<String>,
    pub background_task_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HookRunStatus {
    Completed,
    Backgrounded,
    Failed,
    Blocked,
    Cancelled,
    TimedOut,
}

// ════════════════════════════════════════════════════════════════════
//  元数据类型
// ════════════════════════════════════════════════════════════════════

/// Hook 来源。Runtime 只记录来源；宿主策略层可据此决定信任和装配顺序。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HookSource {
    Managed,
    User,
    Project,
    Plugin,
    Session,
}

impl HookSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            HookSource::Managed => "managed",
            HookSource::User => "user",
            HookSource::Project => "project",
            HookSource::Plugin => "plugin",
            HookSource::Session => "session",
        }
    }
}

/// 失败策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum FailurePolicy {
    /// 失败时记录但继续（默认）。
    #[default]
    Open,
    /// 失败时阻止主操作。
    Closed,
}

// ════════════════════════════════════════════════════════════════════
//  Trait 接口
// ════════════════════════════════════════════════════════════════════

/// Hook 运行时接口。
///
/// 实现方在 `yourai-hooks` crate（`ConcreteHookRuntime`）。
/// Loop 通过此 trait 分发 Hook 调用并消费类型化结果。
///
/// ## 职责边界
///
/// - **HookRuntime 负责**：匹配、执行、超时、取消、解析、校验、聚合
/// - **HookRuntime 不负责**：修改 ContextManager、执行工具、向用户提问
/// - **Loop 负责**：消费 `HookDispatchResult`，应用效果到业务流程
pub trait HookRuntime: Send + Sync {
    /// 分发一次 Hook 调用。
    ///
    /// 实现方应：
    /// 1. 按 `event_name` + matcher 过滤注册项
    /// 2. 并行执行匹配 handler（带各自超时）
    /// 3. 解析输出（command exit code / HTTP status / JSON）
    /// 4. 校验 `hookSpecificOutput.hookEventName` 与输入事件一致
    /// 5. 按 `deny > ask > allow` 聚合权限
    /// 6. 返回 `HookDispatchResult`
    fn dispatch<'a>(
        &'a self,
        invocation: &'a HookInvocation,
    ) -> BoxFuture<'a, Result<HookDispatchResult, YourAiError>>;
}

/// 运行期注册一个原生 Hook 所需的完整元数据。
#[derive(Clone)]
pub struct NativeHookRegistration {
    pub id: String,
    pub event: HookEventKind,
    pub matcher: Option<String>,
    pub handler: Arc<dyn HookHandler>,
    pub timeout: Option<Duration>,
    pub source: HookSource,
    pub failure_policy: FailurePolicy,
    pub once: bool,
}

/// Hook 注册接口（运行期热注册/热注销）。
///
/// 与 [`HookRuntime`] 分离：`HookRuntime` 是 Loop 消费的执行接口，
/// `HookRegistry` 是管理方的注册接口。一个实现可以同时实现两者。
pub trait HookRegistry: Send + Sync {
    /// 注册一个带完整匹配和执行元数据的原生 handler。
    fn register<'a>(
        &'a self,
        registration: NativeHookRegistration,
    ) -> BoxFuture<'a, Result<(), YourAiError>>;
    /// 按 ID 注销。
    fn unregister<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<bool, YourAiError>>;
    /// 返回所有已注册 handler 的 ID。
    fn handler_ids<'a>(&'a self) -> BoxFuture<'a, Vec<String>>;
}

/// Hook handler 接口。
///
/// 实现方在 `yourai-hooks` crate（`CommandHandler`、`HttpHandler`、`NativeHandler`）。
/// `HookRuntime` 实现方持有 `Arc<dyn HookHandler>` 并在 dispatch 时调用。
pub trait HookHandler: Send + Sync {
    /// 执行 handler，返回原始输出（由 runtime 解析为类型化结果）。
    fn execute<'a>(
        &'a self,
        invocation: &'a HookInvocation,
    ) -> BoxFuture<'a, Result<HookOutput, YourAiError>>;
}

/// Handler 原始输出（实现细节，由 runtime 统一解析）。
///
/// 三种后端各自产生不同的原始形态：
/// - `Parsed` — Native handler 直接返回已解析的 JSON
/// - `Command` — 子进程 stdout + stderr + exit code
/// - `Http` — HTTP response body + status code
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum HookOutput {
    /// Native handler 直接返回已解析的 JSON。
    Parsed(serde_json::Value),
    /// Command handler：stdout + stderr + exit code。
    Command {
        stdout: String,
        stderr: String,
        exit_code: i32,
    },
    /// HTTP handler：body + status code。
    Http { body: String, status: u16 },
    /// Handler 已转入后台执行。
    Backgrounded { task_id: String },
}

#[cfg(test)]
mod tests {
    use super::HookEventKind;

    #[test]
    fn claude_event_discriminators_are_complete_and_unique() {
        assert_eq!(HookEventKind::ALL.len(), 27);
        let names: std::collections::HashSet<_> = HookEventKind::ALL
            .into_iter()
            .map(HookEventKind::as_str)
            .collect();
        assert_eq!(names.len(), 27);
        assert!(names.contains("PreToolUse"));
        assert!(names.contains("FileChanged"));
    }
}
