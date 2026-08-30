//! Context 容器 + Agent + turn 运输机制。
//!
//! - [`Context`]：12 个 provider 插槽，`RwLock<Option<Arc<dyn>>>`，
//!   turn 级热替换（决策 5.2）；读取缺失报 `Config`（决策 5.8）
//! - [`Agent`]：组装产物；[`Agent::start`] 是**全局唯一 spawn 点**
//! - [`TurnHandle`]：一次 turn 的句柄（inbox/outbox/cancel/join）
//! - [`TurnContext`]：装配给 loop 的本次 turn 交互参数（决策 5.5）

use std::sync::{Arc, RwLock};

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{AgentLoop, TurnOutput};
use crate::context_manager::ContextManager;
use crate::error::{ErrorKind, YourAiError};
use crate::hooks::HookRegistry;
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
    hooks: RwLock<Option<Arc<dyn HookRegistry>>>,
}

// 读取器：缺失报 Config 错——缺什么在使用点报（决策 5.8）
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

// 写入器：运行时热替换（决策 5.2，turn 级语义）
macro_rules! setters {
    ($($setter:ident : $field:ident, $trait:ident),* $(,)?) => {
        $(
            #[doc = concat!("运行时热替换（决策 5.2）：当前 turn 用旧值跑完，下一 turn 生效")]
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
        hooks: HookRegistry,
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
        set_hooks: hooks, HookRegistry,
    }
}

// endregion: --- Context ---

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
    hooks: Option<Arc<dyn HookRegistry>>,
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
        hooks: hooks, HookRegistry,
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
    pub fn start(self: &Arc<Self>, first: In) -> Result<TurnHandle, YourAiError> {
        let loop_ = self.ctx.agent_loop()?;

        let (inbox_tx, mut inbox_rx) = mpsc::unbounded_channel();
        let (outbox_tx, outbox_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let _ = inbox_tx.send(first); // 启动输入 = inbox 第一条消息

        let ctx = self.ctx.clone();
        let task_cancel = cancel.clone();
        let result = tokio::spawn(async move {
            let sink = ChannelSink::new(outbox_tx);
            let tc = TurnContext {
                ctx: &ctx,
                inbox: &mut inbox_rx,
                outbox: &sink,
                cancel: &task_cancel,
            };
            loop_.run_turn(tc).await
            // tc drop 后 sink drop → outbox 关闭 = turn 结束信号
        });

        Ok(TurnHandle { inbox: inbox_tx, outbox: outbox_rx, cancel, result })
    }

    /// 阻塞变体：outbox 接 DiscardSink（不建 channel，避免无人消费堆积）；
    /// 不 spawn——在调用方 task 内直接驱动。
    pub async fn run(&self, first: In) -> Result<TurnOutput, YourAiError> {
        let loop_ = self.ctx.agent_loop()?;

        let (inbox_tx, mut inbox_rx) = mpsc::unbounded_channel();
        let _ = inbox_tx.send(first);
        let cancel = CancellationToken::new();
        let sink = DiscardSink;
        let tc = TurnContext {
            ctx: &self.ctx,
            inbox: &mut inbox_rx,
            outbox: &sink,
            cancel: &cancel,
        };
        loop_.run_turn(tc).await
    }
}

// endregion: --- Agent ---

// region:    --- TurnHandle / TurnContext ---

/// 一次 turn 的句柄（随 turn 生灭；outbox 关闭 = turn 结束）。
#[derive(Debug)]
pub struct TurnHandle {
    /// 外界 → loop（steer / Reply）
    pub inbox: UnboundedSender<In>,
    /// loop → 外界；`recv()` 返回 `None` 即 turn 结束
    pub outbox: UnboundedReceiver<Out>,
    /// 控制面快路径（绕过 inbox 立即生效）
    pub cancel: CancellationToken,
    result: tokio::task::JoinHandle<Result<TurnOutput, YourAiError>>,
}

impl TurnHandle {
    /// 等最终结果（消费完 outbox 后调用）
    pub async fn join(self) -> Result<TurnOutput, YourAiError> {
        match self.result.await {
            Ok(r) => r,
            Err(e) => Err(YourAiError::Error(ErrorKind::Other(format!(
                "turn task join failed: {e}"
            )))),
        }
    }

    /// 立即取消（ESC / 关停）
    pub fn interrupt(&self) {
        self.cancel.cancel();
    }
}

/// 每次 turn 装配给 loop 的交互参数（决策 5.5：管道是 run 的参数，不是环境状态）。
pub struct TurnContext<'a> {
    /// 全局 providers
    pub ctx: &'a Context,
    /// 外界 → loop（含第一条消息）；loop 独占拉取消费
    pub inbox: &'a mut UnboundedReceiver<In>,
    /// loop → 外界（同步 fire-and-forget）
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
    fn send(&self, m: Out) {
        let _ = self.tx.send(m); // 消费端已离开 → 静默丢弃
    }
}

/// 黑洞（run() 变体用：无消费者，不堆积）
pub struct DiscardSink;

impl OutSink for DiscardSink {
    fn send(&self, _: Out) {}
}

/// 裸 sender 也可直接当 sink 用
impl OutSink for UnboundedSender<Out> {
    fn send(&self, m: Out) {
        let _ = self.send(m);
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
    &self,
    tc: TurnContext<'a>,
) -> crate::future::BoxFuture<'a, Result<TurnOutput, YourAiError>> {
            Box::pin(async move {
                match tc.inbox.recv().await {
                    Some(In::UserText { text }) => {
                        tc.outbox.send(Out::Chunk { text: format!("echo: {text}") });
                        tc.outbox.send(Out::Message { text: format!("done: {text}") });
                        Ok(TurnOutput::new(format!("done: {text}")))
                    }
                    _ => Err(YourAiError::Error(ErrorKind::Loop("expected UserText".into()))),
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
                &self,
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
    &self,
    _tc: TurnContext<'a>,
) -> crate::future::BoxFuture<'a, Result<TurnOutput, YourAiError>> {
                Box::pin(async { Ok(TurnOutput::new("A")) })
            }
        }
        struct LoopB;
        impl AgentLoop for LoopB {
            fn run_turn<'a>(
    &self,
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
}

// endregion: --- 机制自测 ---
