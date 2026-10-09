//! Providers 容器 + Agent + turn 运输机制。
//!
//! - [`Providers`]：12 个 provider 插槽，`RwLock<Option<Arc<dyn>>>`，
//!   turn 级热替换（决策 5.2）；读取缺失报 `Config`（决策 5.8），
//!   可选读取用 `try_*`（供 DefaultLoop 依赖矩阵使用）
//! - [`Agent`]：组装产物；[`Agent::start`] 是**全局唯一 spawn 点**
//! - [`TurnHandle`]：一次 turn 的句柄；drop 请求协作取消
//! - [`TurnContext`]：装配给 loop 的本次 turn 交互参数（决策 5.5），
//!   providers 是 start/run 时刻的**快照**（`ProviderSnapshot`）——
//!   一个 turn 内不可能前后使用两个实现；热替换下一 turn 生效

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, RwLock,
};

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{AgentLoop, TurnFailure, TurnOutput, TurnResult};
use crate::context_manager::ContextManager;
use crate::error::{ErrorKind, YourAiError};
use crate::hooks::HookRuntime;
use crate::memory::MemoryProvider;
use crate::model::ModelProvider;
use crate::observability::ObservabilityProvider;
use crate::protocol::{In, Out};
use crate::sandbox::SandboxProvider;
use crate::security::SecurityProvider;
use crate::session::SessionManager;
use crate::skill::SkillProvider;
use crate::tool::ToolRegistry;
use crate::turn::{TurnInfo, TurnOptions};
use crate::ui::OutSink;
use crate::usage::UsageTracker;

// region:    --- Providers ---

/// 单一处声明全部 provider 插槽，生成 Providers / ProviderSnapshot / AgentBuilder
/// 与三组访问器（读取/可选读取/热替换）——新增插槽只改这份清单。
/// 第一行是必需槽（ProviderSnapshot 中非 Option），其余为可选槽。
macro_rules! provider_slots {
    (
        required $req:ident : $req_try:ident : $req_set:ident : $req_trait:ident
        $(, optional $field:ident : $try:ident : $setter:ident : $trait:ident)*
        $(,)?
    ) => {
        /// 所有 provider 的容器——纯粹的 provider 容器，交互管道不在这里
        ///（每次 turn 由 [`Agent::start`] 装配进 [`TurnContext`]，决策 5.5）。
        pub struct Providers { bindings: RwLock<AgentBuilder> }
        impl Providers {
            /// Read one coherent configuration, including during session startup.
            pub fn bindings(&self) -> AgentBuilder {
                self.bindings.read().unwrap_or_else(|p| p.into_inner()).clone()
            }
            /// Publish related providers together. A panic leaves the old set intact.
            /// The callback must not call back into this Providers container.
            pub fn update(&self, change: impl FnOnce(&mut AgentBuilder)) {
                let mut current = self.bindings.write().unwrap_or_else(|p| p.into_inner());
                let mut next = current.clone();
                change(&mut next);
                *current = next;
            }
            $(
                pub fn $field(&self) -> Result<Arc<dyn $trait>, YourAiError> {
                    self.$try().ok_or_else(|| ErrorKind::Config(format!("provider not configured: {}", stringify!($field))).into())
                }
                pub fn $try(&self) -> Option<Arc<dyn $trait>> {
                    self.bindings.read().unwrap_or_else(|p| p.into_inner()).$field.clone()
                }
                pub fn $setter(&self, value: Arc<dyn $trait>) { self.update(|p| p.$field = Some(value)); }
            )*
            pub fn $req(&self) -> Result<Arc<dyn $req_trait>, YourAiError> {
                self.$req_try().ok_or_else(|| ErrorKind::Config(format!("provider not configured: {}", stringify!($req))).into())
            }
            pub fn $req_try(&self) -> Option<Arc<dyn $req_trait>> {
                self.bindings.read().unwrap_or_else(|p| p.into_inner()).$req.clone()
            }
            pub fn $req_set(&self, value: Arc<dyn $req_trait>) { self.update(|p| p.$req = Some(value)); }
            pub fn snapshot(&self) -> Result<ProviderSnapshot, YourAiError> {
                let p = self.bindings();
                Ok(ProviderSnapshot {
                    $req: p.$req.clone().ok_or_else(|| ErrorKind::Config(format!("provider not configured: {}", stringify!($req))))?,
                    $( $field: p.$field.clone(), )*
                })
            }
        }

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
            pub $req: Arc<dyn $req_trait>,
            $( pub $field: Option<Arc<dyn $trait>>, )*
        }

        /// 装配器：全部字段 Option，`build()` **不做完整性检查**（决策 5.8）——
        /// 缺什么在使用点报 `Config` 错。机制（builder）与策略（CLI 读配置）分离。
        #[derive(Clone, Default)]
        pub struct AgentBuilder {
            pub $req: Option<Arc<dyn $req_trait>>,
            $( pub $field: Option<Arc<dyn $trait>>, )*
        }

        impl AgentBuilder {
            pub fn $req(mut self, v: Arc<dyn $req_trait>) -> Self {
                self.$req = Some(v);
                self
            }
            $(
                pub fn $field(mut self, v: Arc<dyn $trait>) -> Self {
                    self.$field = Some(v);
                    self
                }
            )*

            /// 组装 Agent（不做完整性检查，决策 5.8）
            pub fn build(self) -> Arc<Agent> {
                let ctx = Providers { bindings: RwLock::new(self) };
                Arc::new(Agent { ctx: Arc::new(ctx) })
            }
        }
    };
}

provider_slots! {
    //      字段            可选读取            热替换             trait
    required   agent_loop:        try_agent_loop:        set_agent_loop:        AgentLoop,
    optional   model:             try_model:             set_model:             ModelProvider,
    optional   context_manager:   try_context_manager:   set_context_manager:   ContextManager,
    optional   session:           try_session:           set_session:           SessionManager,
    optional   memory:            try_memory:            set_memory:            MemoryProvider,
    optional   tools:             try_tools:             set_tools:             ToolRegistry,
    optional   skills:            try_skills:            set_skills:            SkillProvider,
    optional   sandbox:           try_sandbox:           set_sandbox:           SandboxProvider,
    optional   security:          try_security:          set_security:          SecurityProvider,
    optional   usage:             try_usage:             set_usage:             UsageTracker,
    optional   observability:     try_observability:     set_observability:     ObservabilityProvider,
    optional   hooks:             try_hooks:             set_hooks:             HookRuntime,
}

// region:    --- Agent ---

/// 组装产物：轻量、可 Clone（Clone 共享同一 Providers）。
#[derive(Clone)]
pub struct Agent {
    ctx: Arc<Providers>,
}

impl Agent {
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
    }

    /// 读取 Providers（运行期热注册/热替换入口，见 8.4）
    pub fn ctx(&self) -> &Providers {
        &self.ctx
    }

    /// 非阻塞启动：spawn turn，立即返回句柄（全局唯一 spawn 点）。
    ///
    /// loop 未装配时返回 `Config` 错（决策 5.8：缺什么在使用点报）。
    ///
    /// 注意：丢弃 [`TurnHandle`] 请求取消该 turn，Loop/Provider 须协作清理；
    /// 要 fire-and-forget 请把句柄存进任务表，不要直接丢弃。
    pub fn start(self: &Arc<Self>, first: In) -> Result<TurnHandle, YourAiError> {
        self.start_with(first, TurnOptions::default())
    }

    /// 带会话身份和执行限制启动。只做装配检查；限制由 Loop 执行。
    /// 启动前失败尚未转移到后台，调用方应保留首条输入以便重试。
    pub fn start_with(
        self: &Arc<Self>,
        mut first: In,
        options: TurnOptions,
    ) -> Result<TurnHandle, YourAiError> {
        // 快照同时完成 loop 的 Config 检查，是本 turn 的恒定视图。
        let snap = self.ctx.snapshot()?;
        validate_session_binding(&snap, &options)?;
        first.ensure_id();
        let info = TurnInfo::new(options);

        let (inbox_tx, mut inbox_rx) = mpsc::unbounded_channel();
        let (outbox_tx, outbox_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let _ = inbox_tx.send(first); // 启动输入 = inbox 第一条消息

        let task_cancel = cancel.clone();
        let task_info = info.clone();
        let result = tokio::spawn(async move {
            let sink = ChannelSink::new(outbox_tx);
            let loop_ = snap.agent_loop.clone();
            let tc = TurnContext {
                info: &task_info,
                snap,
                inbox: &mut inbox_rx,
                outbox: &sink,
                cancel: &task_cancel,
            };
            let mut result = run_loop(loop_.as_ref(), tc).await;
            collect_pending(&mut result, &mut inbox_rx);
            result
            // tc drop 后 sink drop → outbox 关闭 = turn 结束信号
        });

        Ok(TurnHandle {
            info,
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
    pub async fn run(&self, first: In) -> TurnResult {
        self.run_with(first, TurnOptions::default()).await
    }

    /// 非交互执行并传递会话身份与限制。失败也保留未处理输入。
    pub async fn run_with(&self, first: In, options: TurnOptions) -> TurnResult {
        let snap = match self.ctx.snapshot().and_then(|snap| {
            validate_session_binding(&snap, &options)?;
            Ok(snap)
        }) {
            Ok(snap) => snap,
            Err(error) => {
                let mut output = TurnOutput::new("");
                output.pending.push(first);
                return Err(TurnFailure::new(error, output));
            }
        };
        let info = TurnInfo::new(options);

        let (inbox_tx, mut inbox_rx) = mpsc::unbounded_channel();
        let _ = inbox_tx.send(first);
        let cancel = CancellationToken::new();
        let sink = NonInteractiveSink {
            cancel: cancel.clone(),
            asked: AtomicBool::new(false),
        };
        let loop_ = snap.agent_loop.clone();
        let tc = TurnContext {
            info: &info,
            snap,
            inbox: &mut inbox_rx,
            outbox: &sink,
            cancel: &cancel,
        };
        let mut result = run_loop(loop_.as_ref(), tc).await;
        collect_pending(&mut result, &mut inbox_rx);
        if sink.asked.load(Ordering::Acquire) {
            let output = match result {
                Ok(output) => output,
                Err(failure) => failure.output,
            };
            Err(TurnFailure::new(
                ErrorKind::Config(
                    "non-interactive run() received Out::Ask from loop; \
                 use Agent::start() for interactive turns"
                        .into(),
                ),
                output,
            ))
        } else {
            result
        }
    }
}

fn validate_session_binding(
    snap: &ProviderSnapshot,
    options: &TurnOptions,
) -> Result<(), YourAiError> {
    if let (Some(session), Some(history)) = (&options.session, &snap.context_manager) {
        if &session.id != history.session_id() {
            return Err(ErrorKind::Config(
                "turn session does not match ContextManager session".into(),
            )
            .into());
        }
    }
    Ok(())
}

/// 先关闭接收端，使晚到的 send 明确失败；再回收所有已经接纳的消息。
/// Loop 已经取走的消息只能由 Loop 放进 output.pending；panic 无法恢复其局部状态。
fn collect_pending(result: &mut TurnResult, inbox: &mut UnboundedReceiver<In>) {
    inbox.close();
    let output = match result {
        Ok(output) => output,
        Err(failure) => &mut failure.output,
    };
    while let Ok(input) = inbox.try_recv() {
        output.pending.push(input);
    }
}

/// Grace the loop gets past its deadline to run cooperative cleanup before
/// the mechanism-level watchdog drops it.
const WATCHDOG_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// Drive one turn with mechanism-level defense: the deadline backstops loops
/// that ignore it, and a panicking loop still yields a typed failure instead
/// of losing every pending input.
async fn run_loop(loop_: &dyn AgentLoop, tc: TurnContext<'_>) -> TurnResult {
    use futures_util::FutureExt;
    let deadline = tc.info.options.limits.deadline;
    let cancel = tc.cancel.clone();
    // Construct the provider's future inside the unwind guard too: a plugin
    // may panic before returning its future, not only while it is polled.
    let future = std::panic::AssertUnwindSafe(async { loop_.run_turn(tc).await }).catch_unwind();
    let result = match deadline {
        Some(deadline) => {
            let watchdog = tokio::time::Instant::from_std(deadline + WATCHDOG_GRACE);
            match tokio::time::timeout_at(watchdog, future).await {
                Ok(result) => result,
                Err(_) => {
                    cancel.cancel();
                    return Err(TurnFailure::new(
                        ErrorKind::LoopTerminated(
                            "deadline exceeded; watchdog forcibly dropped the loop".into(),
                        ),
                        TurnOutput::new(""),
                    ));
                }
            }
        }
        None => future.await,
    };
    result.unwrap_or_else(|panic| {
        cancel.cancel();
        Err(TurnFailure::new(
            YourAiError::Error(ErrorKind::LoopTerminated(format!(
                "agent loop panicked: {}",
                panic_message(panic.as_ref())
            ))),
            TurnOutput::new(""),
        ))
    })
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = panic.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else {
        "unknown panic payload".into()
    }
}

// endregion: --- Agent ---

// region:    --- TurnHandle / TurnContext ---

/// 一次 turn 的句柄（随 turn 生灭；outbox 关闭 = turn 结束）。
///
/// **Drop 请求取消**：Loop/Provider 必须观察 token 并清理资源；不强杀任务或进程。
#[derive(Debug)]
pub struct TurnHandle {
    pub info: TurnInfo,
    /// 外界 → loop（steer / Reply）
    pub inbox: UnboundedSender<In>,
    /// loop → 外界；`recv()` 返回 `None` 即 turn 结束
    pub outbox: UnboundedReceiver<Out>,
    /// 控制面快路径（绕过 inbox 立即生效）
    pub cancel: CancellationToken,
    result: Option<tokio::task::JoinHandle<TurnResult>>,
}

impl TurnHandle {
    /// Last-resort host shutdown after cooperative cleanup timed out.
    /// Local Loop state cannot be recovered; the host must quarantine/reconcile history.
    pub async fn abort(mut self) {
        self.cancel.cancel();
        if let Some(task) = self.result.take() {
            task.abort();
            let _ = task.await;
        }
    }
    /// 等最终结果（消费完 outbox 后调用）
    pub async fn join(mut self) -> TurnResult {
        match self.result.take() {
            Some(h) => match h.await {
                Ok(r) => r,
                Err(e) => {
                    Err(ErrorKind::LoopTerminated(format!("turn task join failed: {e}")).into())
                }
            },
            None => Err(ErrorKind::Loop("TurnHandle::join called twice".into()).into()),
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
    /// 本次执行身份、绑定会话和限制；不是可变全局配置。
    pub info: &'a TurnInfo,
    /// start/run 时刻的 provider 快照——turn 内读 providers 一律走这里
    pub snap: ProviderSnapshot,
    /// 外界 → loop（含第一条消息）；loop 独占拉取消费
    pub inbox: &'a mut UnboundedReceiver<In>,
    /// loop → 外界（同步 fire-and-forget；返回 false = 消费端已关闭）
    pub outbox: &'a dyn OutSink,
    /// 控制面，绕过 inbox 立即生效
    pub cancel: &'a CancellationToken,
}

impl TurnContext<'_> {
    /// 检查取消和总截止时间。Loop 仍需在所有异步等待中监听取消与超时，
    /// 并在模型/工具调用边界检查自己维护的次数计数。
    pub fn check_control(&self) -> Result<(), YourAiError> {
        if self.cancel.is_cancelled() {
            return Err(crate::AbortReason::Cancelled.into());
        }
        if self
            .info
            .options
            .limits
            .deadline
            .is_some_and(|deadline| std::time::Instant::now() >= deadline)
        {
            return Err(crate::AbortReason::DeadlineExceeded.into());
        }
        Ok(())
    }
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
    fn closed(&self) -> crate::BoxFuture<'_, ()> {
        Box::pin(self.tx.closed())
    }
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
    fn closed(&self) -> crate::BoxFuture<'_, ()> {
        Box::pin(UnboundedSender::closed(self))
    }
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
    use crate::protocol::Out;

    /// 测试桩：读第一条消息 → 发 Chunk+Message → 返回文本。
    /// 仅用于验证 core 的运输机制，不是插件实现。
    struct EchoLoop;

    impl AgentLoop for EchoLoop {
        fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> crate::future::BoxFuture<'a, TurnResult> {
            Box::pin(async move {
                match tc.inbox.recv().await {
                    Some(In::UserText { text, .. }) => {
                        tc.outbox.send(Out::Chunk {
                            text: format!("echo: {text}"),
                        });
                        tc.outbox.send(Out::Message {
                            text: format!("done: {text}"),
                        });
                        Ok(TurnOutput::new(format!("done: {text}")))
                    }
                    _ => Err(ErrorKind::Loop("expected UserText".into()).into()),
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
            ) -> crate::future::BoxFuture<'a, TurnResult> {
                Box::pin(async move {
                    // 正确姿势：任何等待都与 cancel 一起 select!
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {
                            Ok(TurnOutput::new("never"))
                        }
                        _ = tc.cancel.cancelled() => {
                            Err(YourAiError::Aborted(AbortReason::Cancelled).into())
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
            matches!(*err.error, YourAiError::Aborted(AbortReason::Cancelled)),
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
            ) -> crate::future::BoxFuture<'a, TurnResult> {
                Box::pin(async { Ok(TurnOutput::new("A")) })
            }
        }
        struct LoopB;
        impl AgentLoop for LoopB {
            fn run_turn<'a>(
                &'a self,
                _tc: TurnContext<'a>,
            ) -> crate::future::BoxFuture<'a, TurnResult> {
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
            ) -> crate::future::BoxFuture<'a, TurnResult> {
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
            fn stream_events<'a>(
                &'a self,
                _req: crate::model::ModelRequest,
            ) -> crate::future::BoxFuture<'a, Result<crate::model::ModelEventStream, YourAiError>>
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
            ) -> crate::future::BoxFuture<'a, TurnResult> {
                Box::pin(async move {
                    let _ = tc.inbox.recv().await;
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {
                            Ok(TurnOutput::new("never"))
                        }
                        _ = tc.cancel.cancelled() => {
                            self.observed_cancel.store(true, Ordering::SeqCst);
                            Err(YourAiError::Aborted(AbortReason::Cancelled).into())
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
            ) -> crate::future::BoxFuture<'a, TurnResult> {
                Box::pin(async move {
                    let alive = tc.outbox.send(Out::Chunk { text: "x".into() });
                    if !alive {
                        return Err(YourAiError::Aborted(AbortReason::Disconnected).into());
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
            matches!(*err.error, YourAiError::Aborted(AbortReason::Disconnected)),
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
            ) -> crate::future::BoxFuture<'a, TurnResult> {
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
            matches!(*err.error, YourAiError::Error(ErrorKind::Config(ref m)) if m.contains("Ask")),
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
            ) -> crate::future::BoxFuture<'a, TurnResult> {
                Box::pin(async move {
                    tc.cancel.cancel();
                    Err(YourAiError::Aborted(AbortReason::Cancelled).into())
                })
            }
        }

        let agent = Agent::builder()
            .agent_loop(Arc::new(SelfCancellingLoop))
            .build();
        let err = agent.run(In::user_text("hi")).await.unwrap_err();
        assert!(
            matches!(*err.error, YourAiError::Aborted(AbortReason::Cancelled)),
            "非 Ask 的取消必须保留原始语义，实际: {err:?}"
        );
    }

    #[tokio::test]
    async fn unexpectedly_aborted_turn_task_reports_uncertain_execution() {
        let agent = Agent::builder().agent_loop(Arc::new(EchoLoop)).build();
        let handle = agent.start(In::user_text("hi")).unwrap();
        handle.result.as_ref().unwrap().abort();
        let failure = handle.join().await.unwrap_err();
        assert!(matches!(
            *failure.error,
            YourAiError::Error(ErrorKind::LoopTerminated(_))
        ));
    }
}

// endregion: --- 机制自测 ---
