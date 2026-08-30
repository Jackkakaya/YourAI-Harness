//! Context 容器 + Agent + turn 运输机制。
//!
//! - [`Context`]：12 个 provider 插槽，`RwLock<Option<Arc<dyn>>>`，
//!   turn 级热替换（决策 5.2）；读取缺失报 `Config`（决策 5.8），
//!   可选读取用 `try_*`（供 DefaultLoop 依赖矩阵使用）
//! - [`Agent`]：组装产物；[`Agent::start`] 是**全局唯一 spawn 点**
//! - [`TurnHandle`]：一次 turn 的句柄；**drop 即取消**（消费端离开不再耗资源）
//! - [`TurnContext`]：装配给 loop 的本次 turn 交互参数（决策 5.5），
//!   providers 是 start/run 时刻的**快照**（`ProviderSnapshot`）——
//!   一个 turn 内不可能前后使用两个实现；热替换下一 turn 生效

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, RwLock,
};

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{AgentLoop, TurnOutput};
use crate::context_manager::ContextManager;
use crate::error::{ErrorKind, YourAiError};
use crate::hooks::HookRuntime;
use crate::memory::MemoryManager;
use crate::model::ModelProvider;
use crate::observability::ObservabilityProvider;
use crate::sandbox::SandboxProvider;
use crate::security::SecurityProvider;
use crate::session::SessionManager;
use crate::skill::SkillProvider;
use crate::tool::ToolRegistry;
use crate::ui::OutSink;
use crate::usage::UsageTracker;
use yourai_protocol::{In, Out};

// region:    --- Context ---

/// 所有 provider 的容器——纯粹的 provider 容器，交互管道不在这里
///（每次 turn 由 [`Agent::start`] 装配进 [`TurnContext`]，决策 5.5）。
pub struct Context {
    agent_loop: RwLock<Option<Arc<dyn AgentLoop>>>,
    model: RwLock<Option<Arc<dyn ModelProvider>>>,
    context_manager: RwLock<Option<Arc<dyn ContextManager>>>,
    session: RwLock<Option<Arc<dyn SessionManager>>>,
    memory: RwLock<Option<Arc<dyn MemoryManager>>>,
    tools: RwLock<Option<Arc<dyn ToolRegistry>>>,
    skills: RwLock<Option<Arc<dyn SkillProvider>>>,
    sandbox: RwLock<Option<Arc<dyn SandboxProvider>>>,
    security: RwLock<Option<Arc<dyn SecurityProvider>>>,
    usage: RwLock<Option<Arc<dyn UsageTracker>>>,
    observability: RwLock<Option<Arc<dyn ObservabilityProvider>>>,
    hooks: RwLock<Option<Arc<dyn HookRuntime>>>,
}

// 必需读取器：缺失报 Config 错——缺什么在使用点报（决策 5.8）
macro_rules! getters {
    ($($name:ident : $trait:ident),* $(,)?) => {
        $(
            #[doc = concat!("读取 provider；未装配时返回 `Config` 错误（决策 5.8）")]
            pub fn $name(&self) -> Result<Arc<dyn $trait>, YourAiError> {
                self.$name
                    .read()
                    .unwrap()
                    .clone()
                    .ok_or_else(|| {
                        YourAiError::Error(ErrorKind::Config(format!(
                            "provider not configured: {}",
                            stringify!($name)
                        )))
                    })
            }
        )*
    };
}

// 可选读取器：缺失返回 None（DefaultLoop 依赖矩阵用：memory/skills/
// sandbox/security/usage/observability/hooks/session 缺失均有合理缺省）
macro_rules! try_getters {
    ($($name:ident : $field:ident, $trait:ident),* $(,)?) => {
        $(
            #[doc = concat!("可选读取：未装配返回 `None`（决策 5.8 依赖矩阵的可选侧）")]
            pub fn $name(&self) -> Option<Arc<dyn $trait>> {
                self.$field.read().unwrap().clone()
            }
        )*
    };
}

// 写入器：运行时热替换（决策 5.2，turn 级语义）
macro_rules! setters {
    ($($setter:ident : $field:ident, $trait:ident),* $(,)?) => {
        $(
            #[doc = concat!("运行时热替换（决策 5.2）：正在跑的 turn 用旧快照跑完，下一 turn 生效")]
            pub fn $setter(&self, v: Arc<dyn $trait>) {
                *self.$field.write().unwrap() = Some(v);
            }
        )*
    };
}

impl Context {
    getters! {
        agent_loop: AgentLoop,
        model: ModelProvider,
        context_manager: ContextManager,
        session: SessionManager,
        memory: MemoryManager,
        tools: ToolRegistry,
        skills: SkillProvider,
        sandbox: SandboxProvider,
        security: SecurityProvider,
        usage: UsageTracker,
        observability: ObservabilityProvider,
        hooks: HookRuntime,
    }

    try_getters! {
        try_agent_loop: agent_loop, AgentLoop,
        try_model: model, ModelProvider,
        try_context_manager: context_manager, ContextManager,
        try_session: session, SessionManager,
        try_memory: memory, MemoryManager,
        try_tools: tools, ToolRegistry,
        try_skills: skills, SkillProvider,
        try_sandbox: sandbox, SandboxProvider,
        try_security: security, SecurityProvider,
        try_usage: usage, UsageTracker,
        try_observability: observability, ObservabilityProvider,
        try_hooks: hooks, HookRuntime,
    }

    setters! {
        set_agent_loop: agent_loop, AgentLoop,
        set_model: model, ModelProvider,
        set_context_manager: context_manager, ContextManager,
        set_session: session, SessionManager,
        set_memory: memory, MemoryManager,
        set_tools: tools, ToolRegistry,
        set_skills: skills, SkillProvider,
        set_sandbox: sandbox, SandboxProvider,
        set_security: security, SecurityProvider,
        set_usage: usage, UsageTracker,
        set_observability: observability, ObservabilityProvider,
        set_hooks: hooks, HookRuntime,
    }

    /// turn 开始时的 provider 快照（决策 5.2：一个 turn 内实现恒定）。
    /// 缺少 `agent_loop` 时返回 Config 错误，不通过公开 API 暴露 panic 路径。
    pub fn snapshot(&self) -> Result<ProviderSnapshot, YourAiError> {
        Ok(ProviderSnapshot {
            agent_loop: self.agent_loop()?,
            model: self.try_model(),
            context_manager: self.try_context_manager(),
            session: self.try_session(),
            memory: self.try_memory(),
            tools: self.try_tools(),
            skills: self.try_skills(),
            sandbox: self.try_sandbox(),
            security: self.try_security(),
            usage: self.try_usage(),
            observability: self.try_observability(),
            hooks: self.try_hooks(),
        })
    }
}

// endregion: --- Context ---

// region:    --- ProviderSnapshot ---

/// turn 作用域的 provider 快照：在 `start()/run()` 时刻取一次，
/// turn 内全部读取走这里——热替换只影响下一 turn（决策 5.2）。
///
/// DefaultLoop 依赖矩阵（决策 5.8）：
/// - 必需：`agent_loop`（start 已查）、`model`、`context_manager`（用点报 Config）
/// - 工具循环需要：`tools`（无则视作空工具集）
/// - 可选：`session` / `memory` / `skills` / `sandbox` / `security`
///   （缺省 = 不拦截）/ `usage` / `observability` / `hooks`（缺省 = 不发）
#[derive(Clone)]
pub struct ProviderSnapshot {
    pub agent_loop: Arc<dyn AgentLoop>,
    pub model: Option<Arc<dyn ModelProvider>>,
    pub context_manager: Option<Arc<dyn ContextManager>>,
    pub session: Option<Arc<dyn SessionManager>>,
    pub memory: Option<Arc<dyn MemoryManager>>,
    pub tools: Option<Arc<dyn ToolRegistry>>,
    pub skills: Option<Arc<dyn SkillProvider>>,
    pub sandbox: Option<Arc<dyn SandboxProvider>>,
    pub security: Option<Arc<dyn SecurityProvider>>,
    pub usage: Option<Arc<dyn UsageTracker>>,
    pub observability: Option<Arc<dyn ObservabilityProvider>>,
    pub hooks: Option<Arc<dyn HookRuntime>>,
}

// endregion: --- ProviderSnapshot ---

// region:    --- Builder ---

/// 装配器：全部字段 Option，`build()` **不做完整性检查**（决策 5.8）——
/// 缺什么在使用点报 `Config` 错。机制（builder）与策略（CLI 读配置）分离。
#[derive(Default)]
pub struct AgentBuilder {
    agent_loop: Option<Arc<dyn AgentLoop>>,
    model: Option<Arc<dyn ModelProvider>>,
    context_manager: Option<Arc<dyn ContextManager>>,
    session: Option<Arc<dyn SessionManager>>,
    memory: Option<Arc<dyn MemoryManager>>,
    tools: Option<Arc<dyn ToolRegistry>>,
    skills: Option<Arc<dyn SkillProvider>>,
    sandbox: Option<Arc<dyn SandboxProvider>>,
    security: Option<Arc<dyn SecurityProvider>>,
    usage: Option<Arc<dyn UsageTracker>>,
    observability: Option<Arc<dyn ObservabilityProvider>>,
    hooks: Option<Arc<dyn HookRuntime>>,
}

macro_rules! builder_methods {
    ($($method:ident : $field:ident, $trait:ident),* $(,)?) => {
        $(
            pub fn $method(mut self, v: Arc<dyn $trait>) -> Self {
                self.$field = Some(v);
                self
            }
        )*
    };
}

impl AgentBuilder {
    builder_methods! {
        agent_loop: agent_loop, AgentLoop,
        model: model, ModelProvider,
        context_manager: context_manager, ContextManager,
        session: session, SessionManager,
        memory: memory, MemoryManager,
        tools: tools, ToolRegistry,
        skills: skills, SkillProvider,
        sandbox: sandbox, SandboxProvider,
        security: security, SecurityProvider,
        usage: usage, UsageTracker,
        observability: observability, ObservabilityProvider,
        hooks: hooks, HookRuntime,
    }

    /// 组装 Agent（不做完整性检查，决策 5.8）
    pub fn build(self) -> Arc<Agent> {
        let ctx = Context {
            agent_loop: RwLock::new(self.agent_loop),
            model: RwLock::new(self.model),
            context_manager: RwLock::new(self.context_manager),
            session: RwLock::new(self.session),
            memory: RwLock::new(self.memory),
            tools: RwLock::new(self.tools),
            skills: RwLock::new(self.skills),
            sandbox: RwLock::new(self.sandbox),
            security: RwLock::new(self.security),
            usage: RwLock::new(self.usage),
            observability: RwLock::new(self.observability),
            hooks: RwLock::new(self.hooks),
        };
        Arc::new(Agent { ctx: Arc::new(ctx) })
    }
}

// endregion: --- Builder ---

// region:    --- Agent ---

/// 组装产物：轻量、可 Clone（Clone 共享同一 Context）。
#[derive(Clone)]
pub struct Agent {
    ctx: Arc<Context>,
}

impl Agent {
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
    }

    /// 读取 Context（运行期热注册/热替换入口，见 8.4）
    pub fn ctx(&self) -> &Context {
        &self.ctx
    }

    /// 非阻塞启动：spawn turn，立即返回句柄（全局唯一 spawn 点）。
    ///
    /// loop 未装配时返回 `Config` 错（决策 5.8：缺什么在使用点报）。
    ///
    /// 注意：丢弃 [`TurnHandle`] 即取消该 turn（防消费端离开后继续耗资源）；
    /// 要 fire-and-forget 请把句柄存进任务表，不要直接丢弃。
    pub fn start(self: &Arc<Self>, first: In) -> Result<TurnHandle, YourAiError> {
        // 快照同时完成 loop 的 Config 检查，是本 turn 的恒定视图。
        let snap = self.ctx.snapshot()?;

        let (inbox_tx, mut inbox_rx) = mpsc::unbounded_channel();
        let (outbox_tx, outbox_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let _ = inbox_tx.send(first); // 启动输入 = inbox 第一条消息

        let task_cancel = cancel.clone();
        let result = tokio::spawn(async move {
            let sink = ChannelSink::new(outbox_tx);
            let loop_ = snap.agent_loop.clone();
            let tc = TurnContext {
                snap,
                inbox: &mut inbox_rx,
                outbox: &sink,
                cancel: &task_cancel,
            };
            loop_.run_turn(tc).await
            // tc drop 后 sink drop → outbox 关闭 = turn 结束信号
        });

        Ok(TurnHandle {
            inbox: inbox_tx,
            outbox: outbox_rx,
            cancel,
            result: Some(result),
        })
    }

    /// 阻塞变体（**仅非交互**）：
    /// - outbox 接 DiscardSink（不建 channel，避免无人消费堆积）
    /// - 不 spawn——在调用方 task 内直接驱动
    /// - loop 若发出 [`Out::Ask`]，内部取消立即触发，返回 `Config` 错
    ///   （交互场景请用 [`Agent::start`]；本地代码 / 脚本场景用 run）
    pub async fn run(&self, first: In) -> Result<TurnOutput, YourAiError> {
        let snap = self.ctx.snapshot()?;

        let (inbox_tx, mut inbox_rx) = mpsc::unbounded_channel();
        let _ = inbox_tx.send(first);
        let cancel = CancellationToken::new();
        let sink = NonInteractiveSink {
            cancel: cancel.clone(),
            asked: AtomicBool::new(false),
        };
        let loop_ = snap.agent_loop.clone();
        let tc = TurnContext {
            snap,
            inbox: &mut inbox_rx,
            outbox: &sink,
            cancel: &cancel,
        };
        let result = loop_.run_turn(tc).await;
        if sink.asked.load(Ordering::Acquire) {
            Err(YourAiError::Error(ErrorKind::Config(
                "non-interactive run() received Out::Ask from loop; \
                 use Agent::start() for interactive turns"
                    .into(),
            )))
        } else {
            result
        }
    }
}

// endregion: --- Agent ---

// region:    --- TurnHandle / TurnContext ---

/// 一次 turn 的句柄（随 turn 生灭；outbox 关闭 = turn 结束）。
///
/// **Drop 即取消**：句柄被丢弃 = 消费端离开，turn 立即取消，
/// 不再继续消耗模型/工具资源。
#[derive(Debug)]
pub struct TurnHandle {
    /// 外界 → loop（steer / Reply）
    pub inbox: UnboundedSender<In>,
    /// loop → 外界；`recv()` 返回 `None` 即 turn 结束
    pub outbox: UnboundedReceiver<Out>,
    /// 控制面快路径（绕过 inbox 立即生效）
    pub cancel: CancellationToken,
    result: Option<tokio::task::JoinHandle<Result<TurnOutput, YourAiError>>>,
}

impl TurnHandle {
    /// 等最终结果（消费完 outbox 后调用）
    pub async fn join(mut self) -> Result<TurnOutput, YourAiError> {
        match self.result.take() {
            Some(h) => match h.await {
                Ok(r) => r,
                Err(e) => Err(YourAiError::Error(ErrorKind::Other(format!(
                    "turn task join failed: {e}"
                )))),
            },
            None => Err(YourAiError::Error(ErrorKind::Loop(
                "turn already joined".into(),
            ))),
        }
    }

    /// 立即取消（ESC / 关停）
    pub fn interrupt(&self) {
        self.cancel.cancel();
    }
}

impl Drop for TurnHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// 每次 turn 装配给 loop 的交互参数（决策 5.5：管道是 run 的参数，不是环境状态）。
pub struct TurnContext<'a> {
    /// start/run 时刻的 provider 快照——turn 内读 providers 一律走这里
    pub snap: ProviderSnapshot,
    /// 外界 → loop（含第一条消息）；loop 独占拉取消费
    pub inbox: &'a mut UnboundedReceiver<In>,
    /// loop → 外界（同步 fire-and-forget；返回 false = 消费端已关闭）
    pub outbox: &'a dyn OutSink,
    /// 控制面，绕过 inbox 立即生效
    pub cancel: &'a CancellationToken,
}

// endregion: --- TurnHandle / TurnContext ---

// region:    --- 内置 OutSink 实现 ---

/// 把 Out 推进 channel（start() 装配用）
struct ChannelSink {
    tx: UnboundedSender<Out>,
}

impl ChannelSink {
    fn new(tx: UnboundedSender<Out>) -> Self {
        Self { tx }
    }
}

impl OutSink for ChannelSink {
    fn send(&self, m: Out) -> bool {
        self.tx.send(m).is_ok() // false = 消费端已关闭
    }
}

/// 黑洞（run() 变体用：无消费者，不堆积）
pub struct DiscardSink;

impl OutSink for DiscardSink {
    fn send(&self, _: Out) -> bool {
        true
    }
}

/// run() 的 sink：非交互模式，loop 一发 Ask 立即触发内部取消，
/// loop 经 select!（契约要求）感知并返回（快路径，不等 recv 超时）。
struct NonInteractiveSink {
    cancel: CancellationToken,
    asked: AtomicBool,
}

impl OutSink for NonInteractiveSink {
    fn send(&self, m: Out) -> bool {
        if let Out::Ask { .. } = m {
            self.asked.store(true, Ordering::Release);
            self.cancel.cancel();
        }
        true
    }
}

/// 裸 sender 也可直接当 sink 用
impl OutSink for UnboundedSender<Out> {
    fn send(&self, m: Out) -> bool {
        self.send(m).is_ok()
    }
}

// endregion: --- 内置 OutSink 实现 ---

// region:    --- 机制自测（stub loop，非插件实现） ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AbortReason;
    use yourai_protocol::Out;

    /// 测试桩：读第一条消息 → 发 Chunk+Message → 返回文本。
    /// 仅用于验证 core 的运输机制，不是插件实现。
    struct EchoLoop;

    impl AgentLoop for EchoLoop {
        fn run_turn<'a>(
            &'a self,
            tc: TurnContext<'a>,
        ) -> crate::future::BoxFuture<'a, Result<TurnOutput, YourAiError>> {
            Box::pin(async move {
                match tc.inbox.recv().await {
                    Some(In::UserText { text }) => {
                        tc.outbox.send(Out::Chunk {
                            text: format!("echo: {text}"),
                        });
                        tc.outbox.send(Out::Message {
                            text: format!("done: {text}"),
                        });
                        Ok(TurnOutput::new(format!("done: {text}")))
                    }
                    _ => Err(YourAiError::Error(ErrorKind::Loop(
                        "expected UserText".into(),
                    ))),
                }
            })
        }
    }

    fn agent() -> Arc<Agent> {
        Agent::builder().agent_loop(Arc::new(EchoLoop)).build()
    }

    #[tokio::test]
    async fn start_outbox_join_full_flow() {
        let agent = agent();
        let mut handle = agent
            .start(In::user_text("hello"))
            .expect("agent_loop configured");

        // 逐条收事件
        let first = handle.outbox.recv().await.expect("chunk event");
        assert!(matches!(first, Out::Chunk { .. }));

        let second = handle.outbox.recv().await.expect("message event");
        assert!(matches!(second, Out::Message { .. }));

        // outbox 关闭 = turn 结束
        assert!(handle.outbox.recv().await.is_none());

        // join 拿最终结果
        let output = handle.join().await.expect("turn ok");
        assert_eq!(output.text, "done: hello");
        assert!(output.pending.is_empty());
    }

    #[tokio::test]
    async fn start_without_loop_reports_config() {
        let agent = Agent::builder().build(); // 未装配 loop
        let err = agent.start(In::user_text("hi")).unwrap_err();
        assert!(
            matches!(err, YourAiError::Error(ErrorKind::Config(_))),
            "应为 Config 错误，实际: {err:?}"
        );
    }

    #[tokio::test]
    async fn cancel_aborts_via_select() {
        struct WaitingLoop;
        impl AgentLoop for WaitingLoop {
            fn run_turn<'a>(
                &'a self,
                tc: TurnContext<'a>,
            ) -> crate::future::BoxFuture<'a, Result<TurnOutput, YourAiError>> {
                Box::pin(async move {
                    // 正确姿势：任何等待都与 cancel 一起 select!
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {
                            Ok(TurnOutput::new("never"))
                        }
                        _ = tc.cancel.cancelled() => {
                            Err(YourAiError::Aborted(AbortReason::Cancelled))
                        }
                    }
                })
            }
        }

        let agent = Agent::builder().agent_loop(Arc::new(WaitingLoop)).build();
        let handle = agent.start(In::user_text("hi")).expect("ok");
        handle.cancel.cancel(); // ESC
        let err = handle.join().await.unwrap_err();
        assert!(
            matches!(err, YourAiError::Aborted(AbortReason::Cancelled)),
            "应为 Aborted，实际: {err:?}"
        );
    }

    #[tokio::test]
    async fn run_blocking_variant_no_events() {
        let agent = agent();
        let output = agent.run(In::user_text("hi")).await.expect("turn ok");
        assert_eq!(output.text, "done: hi");
    }

    #[tokio::test]
    async fn hot_swap_replaces_provider() {
        struct LoopA;
        impl AgentLoop for LoopA {
            fn run_turn<'a>(
                &'a self,
                _tc: TurnContext<'a>,
            ) -> crate::future::BoxFuture<'a, Result<TurnOutput, YourAiError>> {
                Box::pin(async { Ok(TurnOutput::new("A")) })
            }
        }
        struct LoopB;
        impl AgentLoop for LoopB {
            fn run_turn<'a>(
                &'a self,
                _tc: TurnContext<'a>,
            ) -> crate::future::BoxFuture<'a, Result<TurnOutput, YourAiError>> {
                Box::pin(async { Ok(TurnOutput::new("B")) })
            }
        }

        let agent = Agent::builder().agent_loop(Arc::new(LoopA)).build();
        assert_eq!(agent.run(In::user_text("x")).await.unwrap().text, "A");

        agent.ctx().set_agent_loop(Arc::new(LoopB)); // 热替换
        assert_eq!(agent.run(In::user_text("x")).await.unwrap().text, "B");
    }

    /// 决策 5.2 收紧后的语义：turn 内 providers 来自快照——
    /// 热替换不影响正在跑的 turn，下一 turn 才生效。
    #[tokio::test]
    async fn turn_reads_snapshot_not_live_context() {
        struct ModelProbeLoop {
            entered: Arc<tokio::sync::Notify>,
            resume: Arc<tokio::sync::Notify>,
        }
        impl AgentLoop for ModelProbeLoop {
            fn run_turn<'a>(
                &'a self,
                tc: TurnContext<'a>,
            ) -> crate::future::BoxFuture<'a, Result<TurnOutput, YourAiError>> {
                Box::pin(async move {
                    let before = tc.snap.model.is_some();
                    if before {
                        return Ok(TurnOutput::new("has-model"));
                    }
                    self.entered.notify_one();
                    self.resume.notified().await;
                    let after = tc.snap.model.is_some();
                    Ok(TurnOutput::new(format!("{before}-{after}")))
                })
            }
        }

        struct StubModel;
        impl ModelProvider for StubModel {
            fn complete<'a>(
                &'a self,
                _req: crate::model::ModelRequest,
            ) -> crate::future::BoxFuture<'a, Result<crate::chat::ChatResponse, YourAiError>>
            {
                unimplemented!("机制测试不触达模型")
            }
            fn stream<'a>(
                &'a self,
                _req: crate::model::ModelRequest,
            ) -> crate::future::BoxFuture<'a, Result<crate::chat::ChatStreamResponse, YourAiError>>
            {
                unimplemented!("机制测试不触达模型")
            }
            fn model_iden(&self) -> &str {
                "stub"
            }
        }

        let entered = Arc::new(tokio::sync::Notify::new());
        let resume = Arc::new(tokio::sync::Notify::new());
        let agent = Agent::builder()
            .agent_loop(Arc::new(ModelProbeLoop {
                entered: entered.clone(),
                resume: resume.clone(),
            }))
            .build();

        // 第一 turn 已取到“不含 model”的快照后，再从 Agent 管理入口热替换。
        let entered_wait = entered.notified();
        let handle = agent.start(In::user_text("x")).expect("ok");
        entered_wait.await;
        agent.ctx().set_model(Arc::new(StubModel));
        resume.notify_one();
        assert_eq!(
            handle.join().await.unwrap().text,
            "false-false",
            "正在运行的 turn 应继续使用旧快照"
        );

        // 第二 turn：热替换已经进入新快照。
        assert_eq!(
            agent.run(In::user_text("x")).await.unwrap().text,
            "has-model"
        );
    }

    /// 丢弃 TurnHandle = 取消 turn（防消费端离开后后台继续耗资源）。
    #[tokio::test]
    async fn dropping_handle_cancels_turn() {
        struct HangingLoop {
            observed_cancel: Arc<AtomicBool>,
        }
        impl AgentLoop for HangingLoop {
            fn run_turn<'a>(
                &'a self,
                tc: TurnContext<'a>,
            ) -> crate::future::BoxFuture<'a, Result<TurnOutput, YourAiError>> {
                Box::pin(async move {
                    let _ = tc.inbox.recv().await;
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {
                            Ok(TurnOutput::new("never"))
                        }
                        _ = tc.cancel.cancelled() => {
                            self.observed_cancel.store(true, Ordering::SeqCst);
                            Err(YourAiError::Aborted(AbortReason::Cancelled))
                        }
                    }
                })
            }
        }

        let observed = Arc::new(AtomicBool::new(false));
        let agent = Agent::builder()
            .agent_loop(Arc::new(HangingLoop {
                observed_cancel: observed.clone(),
            }))
            .build();
        let handle = agent.start(In::user_text("hi")).expect("ok");
        drop(handle); // 消费端离开，不做任何显式取消
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            observed.load(Ordering::SeqCst),
            "句柄 drop 应已触发 turn 取消"
        );
    }

    /// OutSink::send 返回 false = 消费端已关闭，loop 可立即以
    /// Disconnected 中止（不必等到 recv 点）。
    #[tokio::test]
    async fn outbox_disconnect_is_detectable() {
        struct SendAwareLoop;
        impl AgentLoop for SendAwareLoop {
            fn run_turn<'a>(
                &'a self,
                tc: TurnContext<'a>,
            ) -> crate::future::BoxFuture<'a, Result<TurnOutput, YourAiError>> {
                Box::pin(async move {
                    let alive = tc.outbox.send(Out::Chunk { text: "x".into() });
                    if !alive {
                        return Err(YourAiError::Aborted(AbortReason::Disconnected));
                    }
                    Ok(TurnOutput::new("ok"))
                })
            }
        }

        let agent = Agent::builder().agent_loop(Arc::new(SendAwareLoop)).build();
        let mut handle = agent.start(In::user_text("hi")).expect("ok");
        handle.outbox.close(); // 消费端关闭（不等价于 turn 结束）
        let err = handle.join().await.unwrap_err();
        assert!(
            matches!(err, YourAiError::Aborted(AbortReason::Disconnected)),
            "应为 Disconnected，实际: {err:?}"
        );
    }

    /// run() 是非交互模式：loop 发 Ask → 立即失败（不会永久等待）。
    #[tokio::test]
    async fn run_is_non_interactive_ask_fails_fast() {
        struct AskLoop;
        impl AgentLoop for AskLoop {
            fn run_turn<'a>(
                &'a self,
                tc: TurnContext<'a>,
            ) -> crate::future::BoxFuture<'a, Result<TurnOutput, YourAiError>> {
                Box::pin(async move {
                    let _ = tc.inbox.recv().await; // 先消费首条（现实 loop 的行为）
                    let _ = tc.outbox.send(Out::Ask {
                        id: "ask-1".into(),
                        payload: serde_json::json!({"kind": "approval"}),
                    });
                    // 即使自定义 loop 错误地忽略 cancel 并返回 Ok，
                    // run() 也必须根据 sink 的 Ask 标志拒绝结果。
                    Ok(TurnOutput::new("incorrect-success"))
                })
            }
        }

        let agent = Agent::builder().agent_loop(Arc::new(AskLoop)).build();
        let err = agent.run(In::user_text("hi")).await.unwrap_err();
        assert!(
            matches!(err, YourAiError::Error(ErrorKind::Config(ref m)) if m.contains("Ask")),
            "run() 遇 Ask 应报 Config 错，实际: {err:?}"
        );
    }

    /// 非交互 run 的普通取消不能被误报成 Ask。
    #[tokio::test]
    async fn run_preserves_unrelated_cancellation() {
        struct SelfCancellingLoop;
        impl AgentLoop for SelfCancellingLoop {
            fn run_turn<'a>(
                &'a self,
                tc: TurnContext<'a>,
            ) -> crate::future::BoxFuture<'a, Result<TurnOutput, YourAiError>> {
                Box::pin(async move {
                    tc.cancel.cancel();
                    Err(YourAiError::Aborted(AbortReason::Cancelled))
                })
            }
        }

        let agent = Agent::builder()
            .agent_loop(Arc::new(SelfCancellingLoop))
            .build();
        let err = agent.run(In::user_text("hi")).await.unwrap_err();
        assert!(
            matches!(err, YourAiError::Aborted(AbortReason::Cancelled)),
            "非 Ask 的取消必须保留原始语义，实际: {err:?}"
        );
    }
}

// endregion: --- 机制自测 ---
