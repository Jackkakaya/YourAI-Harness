# YourAI 架构设计文档

## 1. 设计理念

**Everything is a Plugin.** 

YourAI 是一个"乐高积木"式的 agent 框架。`yourai-core` 定义接口（trait）+ turn 运输机制，零业务 Provider 实现。
用户想改什么，就实现对应的 trait 换上去——包括 agent loop 本身。

### 核心原则

1. **Core 只定义 trait，不含任何实现** —— 所有默认实现在细粒度的 crate 中
2. **机制与词汇分离** —— 词汇（In/Out 消息）定义在独立叶子 crate `yourai-protocol`（零依赖），core 依赖它并以**具体类型**签名；改词汇不碰 core 源码，`#[non_exhaustive]` 保证加变体不炸下游插件
3. **Context 容器模式** —— 全局 `Context` 持有所有 provider，明确字段，类型安全
4. **AgentLoop 最小签名** —— 只给 `TurnContext`（providers 引用 + inbox/outbox/cancel）；loop 是 inbox 的**独占拉取消费者**，用户完全自由编排
5. **全部可热替换** —— 所有 provider 是 `Arc<dyn Trait>`，运行时可替换
6. **复用 genai** —— 模型层直接使用 [rust-genai](https://github.com/jeremychone/rust-genai) 的类型（ChatRequest/ChatResponse/ChatMessage/Tool/Usage 等），不重新造轮子

---

## 2. 调研参考

### 2.1 codex-rs（OpenAI Codex Rust 版）

**借鉴点：**
- 小 crate、薄接口的 workspace 架构
- `#[async_trait]` 不推荐，用 `Pin<Box<dyn Future>>` 兼容 trait object
- SandboxPolicy enum 设计（DangerFullAccess / ReadOnly / WorkspaceWrite / ExternalSandbox）
- Hooks 事件驱动系统（PreToolUse / PostToolUse / PreCompact / SessionStart / Stop）
- ExtensionData 用 TypeId 做类型擦除的 key-value 存储
- 代码规范：模块 < 500 LoC、`#[non_exhaustive]`、Newtype、私有模块 + 显式 pub use

**未借鉴：**
- codex 的 SQ/EQ（Submission/Event）通信模式 —— 我们用更直接的 In/Out 协议 + TurnHandle（且无 dispatcher 中间层，见 6.2）
- codex 的 Contributor Trait 体系（按生命周期维度拆分 11 个 trait）—— 太重，我们用更简洁的单 trait

**multi-agent / plan-execute 实证（重要）：**
- **不存在第二种 loop。** codex 没有 plan-execute 或 multi-agent 专用循环，ReAct 是唯一的 loop
- `SessionTask` 抽象（`core/src/tasks/mod.rs:214`）：turn 之上的任务接口，`run(session, ctx: Arc<TurnContext>, input, cancellation_token)` —— 与我们的 `AgentLoop::run_turn` 同构（连 TurnContext 名字都一样）。四个实现：`RegularTask`（一次普通 turn）、`ReviewTask`（内部启动完整子 codex 会话再回来，`review.rs:74`）、`CompactTask`、`UserShellCommandTask`
- multi-agent = **工具面**（`tools/handlers/multi_agents*.rs`）：`spawn_agent` / `send_message` / `wait_agent` / `close_agent` / `interrupt_agent` / `list_agents` / `followup_task` / `resume_agent`。每个子 agent = 完整子 codex session（`SubAgentSource::ThreadSpawn`），编排的"控制流"就是父 agent 的 ReAct 循环调这些工具
- plan mode = `CollaborationMode::Plan` 换注入指令和工具面（`inject.rs:58`），底下还是同一个 ReAct loop。plan-execute 是提示词 + plan 工具模式，不是代码结构

### 2.2 DeepSeek Harness (dsh)

**借鉴点：**
- "Everything is a Plugin" 的核心理念，连 agent loop 都是插件
- Capability Seam 三角色：Service Definition / Provider / Consumer
- 5 种事件分发模式（emit/waterfall/parallel/serial/bail）
- "Model-visible means logged" —— session log 是唯一真相源
- Profile/Bundle 组合模式

**未借鉴：**
- Cordis 框架的动态 `ctx.xxx` 属性 —— Rust 静态类型不支持，我们用明确字段
- TypeScript 的 GC 热拔插安全网 —— Rust 需要精确的生命周期管理

### 2.3 pi（badlogic/pi-mono）

**借鉴点（执行模型的主参照）：**
- 交互管道是**每次运行的参数**而非环境状态：`runAgentLoop(prompts, context, config, emit, signal)`
- 低层 `agentLoop()` 内部 `void runAgentLoop(...)` fire-and-forget + 返回 turn 作用域的 EventStream（非阻塞启动）；高层 harness 直接 await（阻塞语义）。双层共享同一个 loop 实现
- 主交互循环极其朴素（`interactive-mode.ts:908`）：`while(true) { await getUserInput(); await session.prompt(input); }` —— await 只挂起当前 async 函数不挂起事件循环，ESC 回调/流式渲染照常并发；循环结构天然强制单 turn 串行
- steer 语义（`streamingBehavior: "steer"` / `"followUp"` 两种注入模式）

**实证：故意不内置 multi-agent**
- README: "**No sub-agents.** There's many ways to do this. Spawn pi instances via tmux, or build your own with extensions."
- usage.md: "intentionally does not include built-in MCP, sub-agents, permission popups, plan mode, to-dos, or background bash"
- 官方答案 = `examples/extensions/subagent/`：`registerTool("subagent")`，每个子 agent spawn **独立 pi 进程**（JSON 模式收结构化输出），支持 single/parallel/chain 编排，Ctrl+C 传播杀死子进程

### 2.4 Hermes（Python asyncio）

**反面教材价值（执行模型）：**
- 核心 = 纯同步阻塞的 `run_conversation()`，并发全部外包给 gateway：`asyncio.ensure_future(loop.run_in_executor(...))` + 每 5 秒轮询 + 备用中断检测（防 monitor task 静默死亡，`run.py:19225`）+ 超时自管 + 手动 `_stream_consumer.finish()`
- 事件输出 = agent 构造参数（15 个 callback），粒度粗于 pi 的 run 参数
- 取消 = 单元素 list 传可变引用出去（`agent_ref[0]`），另一线程调 `agent.interrupt()` 设 flag
- **结论：没有 handle 的代价** —— 轮询兜底、手动完成信号、共享容器传结果，这些正是 TurnHandle 要收编进 core 的东西

---

## 3. 整体架构

### 3.1 全景架构图（所有组件）

```
════════════════════════════════════════════════════════════════════
 第 0 层 · 入口
 ┌─────────────────────────────────────────────────────────────────┐
 │ yourai-cli：读 config.toml → 类型化直调各实现 crate → builder 组装 │
 └───────────────────────────────┬─────────────────────────────────┘
                                 │ build()
════════════════════════════════════════════════════════════════════
 第 1 层 · 前端（节奏权）
 ┌─────────────────────────────────────────────────────────────────┐
 │  yourai-tui / yourai-web                                         │
 │  · 入向 adapter：键盘/HTTP ──► In::UserText                       │
 │  · 消费循环：外层 while{start→consume} ＋ 内层 select!{outbox,键盘} │
 │  · 渲染 registry：按 tool name 把 Out 画成画面                     │
 └──────────┬──────────────────────────────────▲───────────────────┘
      ①start(In)                        outbox.recv()→Out
      ②inbox.send(In)/cancel            join()→TurnOutput；关闭=结束
            ▼                                     │
════════════════════════════════════════════════════════════════════
 第 2 层 · 机制层（运输权）
 ┌─────────────────────────────────────────────────────────────────┐
 │  yourai-core（trait + turn 运输机制，零业务 Provider 实现）                                 │
 │  ┌───────────────┐  ┌─────────────────┐  ┌────────────────────┐ │
 │  │ Agent + Builder│─►│ TurnHandle      │  │ Context            │ │
 │  │ start()/run()  │  │ inbox / outbox  │  │ 12 个 provider     │ │
 │  │ 唯一 spawn 点   │  │ cancel / join() │  │ trait 的句柄容器    │ │
 │  └───────────────┘  └────────┬────────┘  └─────────┬──────────┘ │
 │                              │ 装配                 │ 引用        │
 │                              ▼                     │            │
 │                   ┌────────────────────┐           │            │
 │                   │ TurnContext        │           │            │
 │                   │ {ctx, snap(快照), │           │            │
 │                   │  inbox, outbox,   │           │            │
 │                   │  cancel}          │           │            │
 │  OutSink · YourAiError{Aborted | Error(ErrorKind)} │            │
 └─────────────────────────────┼───────────────────────┼────────────┘
                ③run_turn(tc)  │           ⑤调用 providers
                               ▼                       │
════════════════════════════════════════════════════════════════════
 第 3 层 · 编排层（解释权）
 ┌─────────────────────────────────────────────────────────────────┐
 │  yourai-loop-default：DefaultLoop = ReAct（可整体替换）            │
 │  drain inbox → LLM 流式 → 审批门 → 工具执行 → 循环                 │
 └───────────────────────────────┬─────────────────────────────────┘
                     ④outbox.send │
════════════════════════════════════════════════════════════════════
 第 4 层 · 能力层（实现 crate 全部可热换；12 个 trait 可多实现）
 ┌─────────────────────────────────────────────────────────────────┐
 │  model-genai      context-inmemory   session-sqlite             │
 │  memory-sqlite    tools-builtin      tools-mcp                  │
 │  skill-filesystem sandbox-seatbelt   sandbox-seccomp            │
 │  security-preset  usage-inmemory     usage-sqlite               │
 │  observability-otel / tracing        hooks-inmemory             │
 └─────────────────────────────────────────────────────────────────┘

═══════ 语言层（横切所有层）：yourai-protocol（叶子 crate）═══════════
 ┌─────────────────────────────────────────────────────────────────┐
 │  In{UserText, Reply}    Out{Chunk, Reasoning, Message,          │
 │                            ToolStarted, ToolProgress, ToolDone, │
 │                            Ask, Usage, Notice}                  │
 │  #[non_exhaustive] —— 加变体只改这个 crate，core 源码不动          │
 └─────────────────────────────────────────────────────────────────┘
```

**三权分立**（每层只有一种权力，互不越界）：

| 角色 | 权力 | 刻意不拥有 |
|---|---|---|
| 前端 | 节奏：何时开 turn、串行、渲染 | 不进 turn 内部 |
| core | 运输：流、句柄、生命周期、取消 | 不解释消息语义 |
| loop | 解释：消息含义、消费时机、何时结束 | 不 spawn、不知道 UI 存在 |

**刻意减法**（每个减法对应一个研究过的框架的复杂度来源）：没有 dispatcher（codex submission_loop）、没有 actor 框架、没有 pubsub、没有 core 内 session 循环、没有背压。

### 3.2 组件协作（九步走一遍）

```
① 用户敲字      前端入向 adapter ──► In::UserText
② 启动          前端 Agent::start(first) ──► core 建双流+token、spawn、返回 TurnHandle
③ 接管          core 调 AgentLoop::run_turn(TurnContext) ──► loop 开跑
④ 消费          loop 拉取 inbox 第一条消息 → ContextManager 入历史 → build_request
⑤ 生成          loop 调 ModelProvider 流式 → Out::Chunk/Reasoning 走 outbox → 前端渲染
⑥ 用工具        SecurityProvider 判定 ─ Ask? Out::Ask → 等 In::Reply
                → SandboxProvider 包裹 → ToolRegistry 执行（ToolContext 发 ToolProgress）
                → 结果入历史 → 回 ④
⑦ 收尾          无 tool_calls → TurnOutput{text,usage,pending} → outbox 关闭
⑧ 交付          前端 recv()=None → join() → pending 非空则按序 start 逐条续 turn
⑨ 打断          任何时刻 cancel → select! 全场打断 → Aborted（半截文本入历史）

横切：⑤⑥的调用链上，Usage 记账 / Observability 打点 / HookRegistry 拦截
变体：脚本用 run()（await 到底）；服务器每请求 start()，outbox 接 SSE
```

**协作三原则：**

1. **数据面排队**——In/Out 走单写单读流，loop 独占拉取，消费时机是 loop 的自由
2. **控制面旁路**——cancel 不入队（否则打断不了正在等消息的 loop），`select!` 全场监听
3. **生命周期不用消息表达**——开始 = start 返回；结束 = outbox 关闭；失败 = join 的 Err

### 3.3 模块设计一览

| 模块 | 职责 | 关键设计 | 详见 |
|---|---|---|---|
| `yourai-protocol` | 共同语言 | In 2 变体 / Out 9 变体；Ask/Reply 唯一一问一答；无工具专属变体；`#[non_exhaustive]` | §4.13 |
| `yourai-core` | 机制/运输 | start 唯一 spawn；TurnHandle/TurnContext；RwLock 热替换；两级错误；genai 0.6.5 re-export | §4、§5.5–5.7 |
| `yourai-loop-default` | 编排/解释 | ReAct；工具错误消化为 is_error；SteerMode；唯一 loop | §7 |
| `yourai-tui / web` | 节奏/渲染 | 双层循环（pi 外层 + codex 内层）；渲染 registry；adapter | §6.5 |
| 实现 crate × N | 能力 | 全部 `Arc<dyn>` 可热换（一个 trait 可多实现）；MCP=ToolHandler；ContextManager 持 session_id | §4.2–4.12 |
| `yourai-cli` | 装配策略 | toml → 类型化直调 → builder；core 不读文件、impl crate 不读文件 | §5.8 |

### 3.4 Crate 结构

```
yourai/                              # workspace root
│
├── yourai-core/                     # 接口 + 运输机制（零业务 Provider 实现）
│   └── src/
│       ├── lib.rs                   # 入口 + prelude
│       ├── context.rs               # Context 容器 + Agent + AgentBuilder
│       ├── agent_loop.rs            # AgentLoop trait
│       ├── model.rs                 # ModelProvider trait
│       ├── context_manager.rs       # ContextManager trait
│       ├── session.rs               # SessionManager trait
│       ├── memory.rs                # MemoryManager trait
│       ├── tool.rs                  # ToolHandler + ToolRegistry trait
│       ├── skill.rs                 # SkillProvider trait
│       ├── sandbox.rs               # SandboxProvider trait + SandboxPolicy
│       ├── security.rs              # SecurityProvider trait
│       ├── usage.rs                 # UsageTracker trait
│       ├── observability.rs         # ObservabilityProvider trait
│       ├── hooks.rs                 # HookHandler + HookRegistry trait
│       ├── ui.rs                    # OutSink trait（机制）；词汇在 yourai-protocol
│       ├── error.rs                 # YourAiError
│       └── future.rs                # BoxFuture 类型别名
│
├── yourai-protocol/                   # 叶子 crate：In/Out 词汇定义（零依赖，core 依赖它）
├── yourai-loop-default/               # 默认 AgentLoop 实现（讲 yourai-protocol）
├── yourai-model-genai/              # ModelProvider: 封装 genai::Client
├── yourai-context-inmemory/         # ContextManager: 内存实现
├── yourai-session-sqlite/           # SessionManager: SQLite 持久化
├── yourai-session-jsonl/            # SessionManager: JSONL 文件持久化
├── yourai-memory-sqlite/            # MemoryManager: SQLite + 语义搜索
├── yourai-tools-builtin/            # ToolRegistry + 内置工具（shell/read_file/...）
├── yourai-tools-mcp/                # ToolRegistry: MCP server 适配
├── yourai-skill-filesystem/         # SkillProvider: 文件系统目录发现
├── yourai-sandbox-seatbelt/         # SandboxProvider: macOS Seatbelt
├── yourai-sandbox-seccomp/          # SandboxProvider: Linux seccomp/bwrap
├── yourai-security-preset/          # SecurityProvider: 预设策略（workspace-write/...）
├── yourai-usage-inmemory/           # UsageTracker: 内存实现
├── yourai-usage-sqlite/             # UsageTracker: SQLite 持久化
├── yourai-observability-otel/       # ObservabilityProvider: OpenTelemetry 桥接
├── yourai-observability-tracing/    # ObservabilityProvider: tracing 日志
├── yourai-hooks-inmemory/           # HookRegistry: 内存实现
│
├── yourai-tui/                      # TUI 前端
├── yourai-web/                      # Web 前端
└── yourai-cli/                      # CLI 入口（组装默认 crate 组合）
```

每个实现 crate 只依赖 `yourai-core` + 自己需要的第三方库，互不依赖。
用户按需选择 crate 组装，不要的就不引入。

### 3.5 Context 容器

`Context` 是所有 provider 的容器。AgentLoop 从 Context 取所需 provider 来编排。

```
Context (容器)                          TurnContext (每次 turn 的交互参数)
├── agent_loop:      AgentLoop          ├── ctx:    &Context
├── model:           ModelProvider      ├── outbox: &dyn OutSink       ← send(Out)
├── context_manager: ContextManager     ├── inbox:  &mut Receiver<In>  ← 外界 → loop
├── session:         SessionManager     └── cancel: &CancellationToken ← 控制面
├── memory:          MemoryManager
├── tools:           ToolRegistry
├── skills:          SkillProvider
├── sandbox:         SandboxProvider
├── security:        SecurityProvider
├── usage:           UsageTracker
├── observability:   ObservabilityProvider
└── hooks:           HookRegistry
```

**命名澄清：** `Context`（容器）和 `ContextManager`（对话历史管理）是两个不同的东西。

**Context 是纯粹的 provider 容器**——交互管道（inbox/outbox/cancel）不在 Context 里，而是每次 turn 由 `Agent::start()` 装配进 `TurnContext` 传入（见第 6 章，借鉴 pi 的"交互管道是 run 的参数"）。消息类型是 `yourai-protocol` 的具体 `In`/`Out`（4.13）——core 依赖协议 crate 但不拥有词汇。

### 3.6 数据流

```
用户输入（前端解析键盘/HTTP → In::UserText）
  ↓
agent.start(first: In) ──► TurnHandle{inbox, outbox, cancel}
  ↓ tokio::spawn（core 唯一 spawn 点）
AgentLoop::run_turn(tc)             ← 用户自定义的编排逻辑
  │
  ├── tc.inbox.recv()               → 第一条消息 = In::UserText
  ├── tc.snap.context_manager       → 添加用户消息到历史 / build_request()
  ├── tc.snap.model                 → stream(ModelRequest)
  ├── tc.snap.security              → check_tool_call()（第一层审批）
  ├── tc.snap.tools                 → execute(tc, ...)（ToolContext 可发进度/可取消）
  ├── tc.snap.memory / usage        → store / record
  ├── tc.snap.observability / hooks → span/metric / dispatch
  └── tc.outbox.send(Out::*)        → outbox → 调用方渲染（false = 消费端关闭）
                                        （sandbox/二层的 check_* 经 ToolContext 由工具自调）
  ↓
TurnOutput { text, usage, pending }  → join() 交付；pending 非空由调用方续 turn
```

---

## 4. 各组件接口定义

### 4.1 AgentLoop

**职责：** 编排 turn 流程（模型调用 → 工具执行 → 重复直到完成）

**设计决策：最小签名，最大自由。** 只给 TurnContext（providers 引用 + inbox/outbox/cancel），用户完全自己编排。启动输入不是特殊参数——**它是 inbox 的第一条消息**，loop 只有一条读路径。

```rust
pub trait AgentLoop: Send + Sync {
    // 生命周期绑定 &self：有状态 loop 可在 async block 里借用自身字段
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, Result<TurnOutput, YourAiError>>;
}

pub struct TurnContext<'a> {
    pub ctx:    &'a Context,                          // 全局 providers
    pub inbox:  &'a mut UnboundedReceiver<In>,        // 外界 → loop（含第一条消息）
    pub outbox: &'a dyn OutSink,                      // loop → 外界，send(Out)，同步 fire-and-forget
    pub cancel: &'a CancellationToken,                // 控制面，绕过 inbox 立即生效
}

pub trait OutSink: Send + Sync { fn send(&self, m: Out); }

pub struct TurnOutput {
    pub text:    String,           // agent 最终回答
    pub usage:   Option<Usage>,
    pub pending: Vec<In>,          // 退出前 inbox 里没消费的残留，调用方负责续 turn
}

impl Agent {
    pub fn start(&self, first: In) -> TurnHandle;              // 全局唯一 spawn 点
    pub async fn run(&self, first: In) -> Result<TurnOutput>;  // 丢弃 outbox 的阻塞变体
}
// start 内部：建 channels → first 发入 inbox → spawn(run_turn) → 返回 TurnHandle
```

消息类型是 `yourai-protocol` 的具体 `In`/`Out`（见 4.13）——协议是独立叶子 crate，core 依赖它但不拥有词汇；加变体改 protocol crate，core 源码不动。

用户实现示例：
```rust
impl AgentLoop for MyLoop {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, Result<TurnOutput, YourAiError>> {
        Box::pin(async move {
            let model = tc.snap.model.ok_or_else(missing("model"))?;
            let history = tc.snap.context_manager.ok_or_else(missing("context_manager"))?;

            // 第一条消息 = 用户输入，直接 match，无装箱
            if let Some(In::UserText { text }) = tc.inbox.recv().await {
                history.add_user_message(&text).await?;
            }

            let resp = model.complete(history.build_request()).await?;
            let text = resp.into_first_text().unwrap_or_default();
            history.add_assistant_message(&text).await?;
            Ok(TurnOutput { text, usage: None, pending: vec![] })
        })
    }
}
```

### 4.2 ModelProvider

**职责：** 封装 LLM API 调用

**设计决策：** 直接使用 genai 的 ChatRequest/ChatResponse/ChatStreamResponse 类型；调用参数是 `ModelRequest`——`ChatRequest` + `ChatOptions` 一起走（capture_usage/capture_tool_calls 等选项由 ContextManager 组装请求时带出，不再断链）。

```rust
pub struct ModelRequest {
    pub request: ChatRequest,
    pub options: ChatOptions,
}

pub trait ModelProvider: Send + Sync {
    fn complete<'a>(&'a self, req: ModelRequest) -> BoxFuture<'a, Result<ChatResponse, YourAiError>>;
    fn stream<'a>(&'a self, req: ModelRequest) -> BoxFuture<'a, Result<ChatStreamResponse, YourAiError>>;
    fn model_iden(&self) -> &str;
}
```

默认实现 `yourai-model-genai` 中会包装 `genai::Client`；core re-export 所有 genai 类型（见 5.6）。

### 4.3 ContextManager

**职责：** 管理对话历史（发给模型的消息序列）

**注意：** 这不是 Context 容器，而是管理模型上下文窗口的 provider。

```rust
pub trait ContextManager: Send + Sync {
    fn messages(&self) -> Vec<ChatMessage>;
    fn add_user_message(&self, text: &str) -> BoxFuture<'_, Result<()>>;
    fn add_assistant_message(&self, text: &str) -> BoxFuture<'_, Result<()>>;
    fn add_tool_result(&self, response: ToolResponse) -> BoxFuture<'_, Result<()>>;  // call_id 贯穿
    fn add_message(&self, message: ChatMessage) -> BoxFuture<'_, Result<()>>;
    fn build_request(&self) -> ChatRequest;
    fn build_request_with(&self, tools: &[Tool], system: Option<&str>) -> ChatRequest;
    fn compact(&self) -> BoxFuture<'_, Result<()>>;
    fn token_count(&self) -> u64;
    fn clear(&self) -> BoxFuture<'_, Result<()>>;
    fn record_usage(&self, usage: &Usage) -> BoxFuture<'_, Result<()>>;
}
```

### 4.4 SessionManager

**职责：** 会话生命周期管理（创建/加载/保存/fork/列表/删除）

```rust
pub struct SessionId(pub String);
pub struct SessionMeta {
    pub id: SessionId,
    pub title: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub model: Option<String>,
}

pub trait SessionManager: Send + Sync {
    fn create_session(&self) -> BoxFuture<'_, Result<SessionMeta>>;
    fn load_session(&self, id: &SessionId) -> BoxFuture<'_, Result<SessionMeta>>;
    fn save_session(&self, session: &SessionMeta) -> BoxFuture<'_, Result<()>>;
    fn list_sessions(&self) -> BoxFuture<'_, Result<Vec<SessionMeta>>>;
    fn delete_session(&self, id: &SessionId) -> BoxFuture<'_, Result<()>>;
    fn fork_session(&self, id: &SessionId) -> BoxFuture<'_, Result<SessionId>>;
}
```

### 4.5 MemoryManager

**职责：** 跨会话的长期记忆（用户偏好、项目上下文、事实等）

```rust
pub struct MemoryEntry {
    pub key: String,
    pub value: String,
    pub category: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub trait MemoryManager: Send + Sync {
    fn store(&self, key: &str, value: &str, category: Option<&str>) -> BoxFuture<'_, Result<()>>;
    fn retrieve(&self, key: &str) -> BoxFuture<'_, Result<Option<String>>>;
    fn search(&self, query: &str, limit: usize) -> BoxFuture<'_, Result<Vec<MemoryEntry>>>;
    fn list(&self, category: Option<&str>) -> BoxFuture<'_, Result<Vec<MemoryEntry>>>;
    fn delete(&self, key: &str) -> BoxFuture<'_, Result<()>>;
    fn clear(&self) -> BoxFuture<'_, Result<()>>;
}
```

### 4.6 ToolRegistry + ToolHandler

**职责：** 工具注册与执行

**设计：** 分离定义侧（给模型看的 schema）和执行侧（实际运行的代码）。执行签名带 `ToolContext`——**工具有身份（call_id）、能发进度、能被取消、能拿两层审批能力**。shell 流 stdout、browser 发截图、subagent 转发子事件都靠 `tc.emit_progress()`（id 自动取 call_id）；ESC 打断长工具靠 `tc.cancel`。工具仍然不知道 UI 存在——只对 outbox 讲协议。

**两层审批**：第一层 `check_tool_call` 由 loop 在调度前统一问；第二层 `check_command`/`check_file_access` 与 `sandbox.apply(&mut Command)` 只有具体工具自己知道 Command/路径——经 ToolContext 注入的能力由工具自调。

```rust
pub struct ToolContext<'a> {
    pub call_id:  String,                            // 本次调用身份（= ToolCall.call_id）
    pub emit:     &'a dyn OutSink,                   // 发 Out（约定：工具只发 ToolProgress）
    pub cancel:   &'a CancellationToken,             // ESC 打断长工具
    pub security: Option<Arc<dyn SecurityProvider>>, // 第二层审批（快照）
    pub sandbox:  Option<Arc<dyn SandboxProvider>>,  // 工具对自建 Command 调 apply
}

impl ToolContext<'_> {
    pub fn emit_progress(&self, payload: Value) -> bool;  // id 自动 = call_id
}

pub trait ToolHandler: Send + Sync {
    fn name(&self) -> &str;
    fn definition(&self) -> Tool;                    // genai::chat::Tool
    fn execute<'a>(&'a self, tc: ToolContext<'a>, input: Value) -> BoxFuture<'a, Result<Value, YourAiError>>;
}

pub trait ToolRegistry: Send + Sync {
    fn register(&self, handler: Arc<dyn ToolHandler>);
    fn unregister(&self, name: &str);
    fn has(&self, name: &str) -> bool;
    fn definitions(&self) -> Vec<Tool>;              // 所有工具的 schema
    fn execute(&self, tc: ToolContext<'_>, name: &str, input: Value) -> BoxFuture<'_, Result<Value>>;
    fn count(&self) -> usize;
}
```

### 4.7 SkillProvider

**职责：** 技能/指令目录管理

```rust
pub struct SkillInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub category: Option<String>,
}

pub struct SkillContent {
    pub info: SkillInfo,
    pub instructions: String,        // 注入到模型上下文的指令文本
    pub tools: Vec<String>,          // 此技能提供的工具名
}

pub trait SkillProvider: Send + Sync {
    fn list(&self) -> BoxFuture<'_, Result<Vec<SkillInfo>>>;
    fn load(&self, id: &str) -> BoxFuture<'_, Result<SkillContent>>;
    fn register(&self, skill: SkillContent) -> BoxFuture<'_, Result<()>>;
    fn unregister(&self, id: &str) -> BoxFuture<'_, Result<()>>;
}
```

### 4.8 SandboxProvider

**职责：** 进程沙箱（限制 agent 生成的进程的文件系统和网络访问）

**设计：** 借鉴 codex 的 SandboxPolicy enum

```rust
pub enum SandboxPolicy {
    DangerFullAccess,
    ReadOnly { network_access: bool },
    WorkspaceWrite { writable_roots: Vec<PathBuf>, network_access: bool },
    ExternalSandbox { network_access: bool },
}

pub enum SandboxType {
    None,
    MacosSeatbelt,
    LinuxSeccomp,
    WindowsRestrictedToken,
}

pub trait SandboxProvider: Send + Sync {
    fn policy(&self) -> SandboxPolicy;
    fn sandbox_type(&self) -> SandboxType;
    fn apply(&self, command: &mut Command) -> Result<()>;
    fn is_active(&self) -> bool;
}
```

### 4.9 SecurityProvider

**职责：** 审批和权限决策（工具调用前检查）

```rust
pub enum ApprovalDecision {
    Allow,
    Deny,
    Ask,
}

pub struct SecurityContext {
    pub action: String,
    pub input: Value,
    pub is_destructive: bool,
    pub is_network: bool,
}

pub trait SecurityProvider: Send + Sync {
    // 第一层：工具调用级审批（loop 在调度前统一问）
    fn check_tool_call(&self, ctx: &SecurityContext) -> BoxFuture<'_, Result<ApprovalDecision>>;
    // 第二层：命令/文件级（shell/fs 工具经 ToolContext 自调）
    fn check_command(&self, command: &str) -> BoxFuture<'_, Result<ApprovalDecision>>;
    fn check_file_access(&self, path: &str, write: bool) -> BoxFuture<'_, Result<ApprovalDecision>>;
}
```

### 4.10 UsageTracker

**职责：** token 用量和成本统计

```rust
pub struct UsageStats {
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_tokens: u64,
    pub request_count: u64,
}

pub trait UsageTracker: Send + Sync {
    fn record(&self, session_id: &SessionId, usage: &Usage) -> BoxFuture<'_, Result<()>>;
    fn total(&self) -> BoxFuture<'_, Result<UsageStats>>;
    fn session_usage(&self, session_id: &SessionId) -> BoxFuture<'_, Result<UsageStats>>;
    fn reset_session(&self, session_id: &SessionId) -> BoxFuture<'_, Result<()>>;
}
```

### 4.11 ObservabilityProvider

**职责：** trace + metrics + log

**设计：** 基于 tracing，实现可桥接到 OpenTelemetry

```rust
pub trait Span: Send + Sync {
    fn record(&self, key: &str, value: &str);
    fn record_error(&self, error: &str);
}

pub trait ObservabilityProvider: Send + Sync {
    fn span(&self, name: &str) -> Arc<dyn Span>;
    fn child_span(&self, name: &str, parent: &dyn Span) -> Arc<dyn Span>;
    fn metric(&self, name: &str, value: f64, tags: &[(&str, &str)]);
    fn increment(&self, name: &str, tags: &[(&str, &str)]) { ... }
    fn timing(&self, name: &str, seconds: f64, tags: &[(&str, &str)]) { ... }
}
```

### 4.12 HookRegistry + HookHandler

**职责：** 事件驱动的拦截点

**多 handler 合并规则（实现方必须遵守）：** ① 按注册顺序串行分发；② 任一 `Block` 短路立即返回；③ `Modify` 链式传递（前一个的 `new_input` 成为下一个的 `tool_input`）；④ 全 Continue → `Continue`，有 Modify 无 Block → 最后一个 Modify。

```rust
pub enum HookEventType {
    PreToolUse, PostToolUse,
    PreCompact, PostCompact,
    SessionStart, SessionEnd,
    TurnStart, TurnEnd,
    UserPromptSubmit,
}

pub struct HookEvent {
    pub event_type: HookEventType,
    pub tool_name: Option<String>,
    pub tool_input: Option<Value>,
    pub tool_output: Option<Value>,
    pub user_message: Option<String>,
}

pub enum HookOutcome {
    Continue,
    Block { reason: String },
    Modify { new_input: Value },
}

pub trait HookHandler: Send + Sync {
    fn id(&self) -> &str;
    fn event_types(&self) -> &[HookEventType];
    fn handle(&self, event: &HookEvent) -> BoxFuture<'_, Result<HookOutcome>>;
}

pub trait HookRegistry: Send + Sync {
    fn register(&self, handler: Arc<dyn HookHandler>);
    fn unregister(&self, id: &str);
    fn dispatch(&self, event: HookEvent) -> BoxFuture<'_, Result<HookOutcome>>;
    fn handler_ids(&self) -> Vec<String>;
}
```

### 4.13 协议层（yourai-protocol）

**职责：** 定义 loop 与外界之间所有消息的**词汇**——`In`/`Out` 两个 enum。

**结构：独立叶子 crate，core 依赖它。**

```
yourai-protocol  ：零依赖（仅 serde），In/Out 定义 + 便捷构造器（In::user_text 等）
yourai-core      ：依赖 protocol，trait 签名直呼具体类型
所有插件          ：依赖 protocol + core，match 直接写
```

- **加变体 = 改 protocol crate**，core 源码不动，重新编译即可
- **`#[non_exhaustive]`** 标注两个 enum：下游不许穷举 match，加变体不炸插件
- **cargo 版本统一**：同一二进制共享同一个 protocol 版本，类型一致由编译器保证
- 生态需要**一门共同语言**，不是 N 门——协议是集成点本身；外部系统讲自己的协议时走 adapter 翻译（TUI 的键盘解析、Web 的 JSON 序列化本来就是 adapter）

```rust
// 外界 → loop（2 个变体；取消不走 In，是机制性的 cancel）
#[non_exhaustive]
pub enum In {
    UserText { text: String },               // 对话输入 & steer（同一条路）
    Reply    { id: String, payload: Value }, // 对一切 Out::Ask 的答复
}

// loop → 外界（9 个变体）
#[non_exhaustive]
pub enum Out {
    Chunk        { text: String },                              // 正文流式
    Reasoning    { text: String },                              // 思考流式（thinking/R1）
    Message      { text: String },                              // 完整消息
    ToolStarted  { id: String, name: String, input: Value },
    ToolProgress { id: String, payload: Value },                // 长工具增量通道
    ToolDone     { id: String, name: String, output: Value, is_error: bool },
    Ask          { id: String, payload: Value },                // 一切"loop 问外界"
    Usage        { usage: Usage },
    Notice       { level: Level, message: String },             // 压缩/降级/非致命错误
}
// In/Out derive Serialize/Deserialize + Debug（边界 adapter 序列化/日志用）
```

**语义约定：**

- **Ask/Reply 是唯一的一问一答机制**：审批、提问、多选、表单、计划确认、MCP elicitation 全是 payload 约定，机制只有一对。审批 = `Ask{payload: 审批描述}` + `Reply{payload: approve/deny}`
- **取消不走 In**（cancel token，控制面）；SetModel/Compact/Shutdown 不是 turn 消息，是 Agent 方法（热替换走 Context 的 RwLock，见 5.2 / 8.4）
- **协议不携带任何工具的专属变体**：工具三段式（Started/Progress/Done）是骨架，input/output/payload 是结构化 Value；browser 截图、subagent 事件树、shell stdout 都走 ToolProgress 载荷
- **渲染知识在前端**：前端按 tool name 注册 renderer，新工具出现协议零改动

**覆盖验证：**

| 显示需求 | 走法 |
|---|---|
| browser use | ToolStarted(browser_*) → ToolProgress(截图/URL/动作) → ToolDone；截图放 payload（base64/文件引用） |
| subagent | 子 agent 的 outbox 逐条 Out → 序列化进 ToolProgress.payload 转发，UI 渲染成树 |
| shell | stdout 逐行 → ToolProgress；ESC → 工具级 cancel（ToolContext） |
| skill | 技能加载 → Notice；技能提供的工具 → 正常三段式 |
| plan 模式 | Out::Ask{payload: 计划 markdown} → In::Reply{approve/修改意见} |
| 提问/表单 | Out::Ask → In::Reply，payload 约定 |

---

## 5. 设计决策记录

### 5.1 AgentLoop 默认实现 ✅ 已决策

**决策：** `yourai-loop-default` 提供开箱即用的 AgentLoop（`DefaultLoop`）。
YourAI 是开箱即用的 agent，用户可以零定制直接跑，也可以替换任何组件。

### 5.2 热替换的原子性 ✅ 已决策

**决策：** `RwLock<Option<Arc<dyn Trait>>>` 够用，不引入 `arc-swap`。

**语义（收敛修订：由"读取时取最新"收紧为 **turn 开始时快照**）：** `start()/run()` 在启动时把 12 个插槽快照成 `ProviderSnapshot` 装进 `TurnContext`——一个 turn 内所有 provider 读取都走快照，**热替换必然只影响下一 turn**，语义可预测且不需要任何锁协议配合。`tc.ctx`（live Context）只留给 admin 用（`set_*`、运行期注册），loop 不从它读 providers。替换后旧 Arc 由快照持有，进行中的调用安全完成。agent 瓶颈在 IO，锁竞争可忽略；未来如需优化，换 `arc-swap` 是实现细节，不影响 trait 定义。

### 5.3 MCP 集成 ✅ 已决策

**决策：** 方案 B——MCP 工具作为 `ToolHandler` 注册到现有 `ToolRegistry`。

- `yourai-tools-mcp` 只提供 `McpClient::connect() → Vec<Arc<dyn ToolHandler>>`
- 用户自然混用内置工具和 MCP 工具，都注册到同一个 registry
- MCP server 生命周期管理（进程启动/重启/健康检查）是 `yourai-tools-mcp` 内部事务
- core 零新增 trait

### 5.4 Session 与 ContextManager 关系 ✅ 已决策

**决策：** 方案 D——ContextManager 持有 session_id，自己 load/save。

- `SessionManager` 只管元数据（title、时间戳、model）
- ContextManager 实现绑定 session_id：构造时 load 历史，每次 add_message 自动持久化
- 恢复 session = 用 `SqliteContextManager::open(db, session_id)` 重建，历史自动恢复
- core trait 无需改动，持久化策略（内存/SQLite/JSONL）是实现层的事

### 5.5 交互模型与协议分层 ✅ 已决策

**决策：具体协议类型 + 独立叶子协议 crate。** 词汇见 4.13（`yourai-protocol`），机制见第 6 章。核心结论：

- **词汇在 `yourai-protocol`**（独立叶子 crate，零依赖，core 依赖它）：In 2 变体（UserText/Reply）+ Out 9 变体；Ask/Reply 是唯一的一问一答机制（审批/提问/表单/计划确认全是 payload 约定）
- **不用类型擦除**（`dyn Any` 曾被考虑后否决）：生态需要一门共同语言而不是 N 门；具体类型带来编译期安全 + 无装箱噪音；加变体改 protocol crate，core 源码不动；`#[non_exhaustive]` 保证演化不炸下游。这正是 codex 的真实结构（`codex-protocol` 叶子 crate，`codex-core` 依赖它）
- **交互管道是 run 的参数**（借鉴 pi）：由 `Agent::start()` 每次 turn 装配进 TurnContext，不放 Context
- **`start()` 是唯一 spawn 点**（对应 pi 低层 fire-and-forget）→ 返回 TurnHandle；`run()` 是丢弃 outbox 的阻塞变体（不建 channel，避免无人消费堆积）
- **双路径**：取消走 CancellationToken 快路径（绕过 inbox 立即生效，能打断"正在等消息的 loop"本身）；其余一切消息走 inbox 慢路径（loop 独占拉取，消费时机是 loop 的自由）
- **工具可发消息、可被取消**：`ToolHandler::execute(tc: ToolContext, input)`——subagent/browser/长任务可显示的先决条件
- **生命周期不用消息表达**：开始 = start 返回；结束 = outbox 关闭；失败 = join 的 Err
- **丢弃 TurnHandle 即取消**（drop → cancel token）：消费端离开（SSE 断开、客户端丢失句柄）后 turn 不再继续耗模型/工具资源；要 fire-and-forget 就把句柄存进任务表，不要丢弃
- **`OutSink::send` 返回 `bool`**（false = 消费端已关闭）：loop 在每个 emit 点都能检测对端死亡，立即以 `Aborted(Disconnected)` 中止，不必等到下一个 recv 点
- **`run()` 是非交互模式**：loop 发出 `Out::Ask` 时内部取消立即触发，`run()` 返回 `Config` 错——交互场景一律用 `start()`（脚本场景 run 永不挂死）
- **inbox/outbox 为无界 channel（v1 已知限制）**：策略 = 不丢弃、不合并，靠取消兜底；单事件大小预算（如 ToolProgress payload ≤ 1MB）由 loop 实现约定；慢消费者风险由消费端自担（文档明示）

### 5.6 genai 版本 ✅ 已决策

**决策：使用最新稳定版 `genai = "0.6.5"` + core re-export。**

- 查证（crates.io）：最新稳定 = 0.6.5；0.7.0-beta.19 仍在高频 churn（6 月起 12 个 beta），**beta 期结束前不碰**
- 0.6.5 能力已验证覆盖设计所需：`ChatStreamEvent` 流式 / `StreamEnd.captured_tool_calls()` 工具调用捕获 / `captured_usage` / `captured_reasoning_content`（支撑 Out::Usage / Out::Reasoning）
- **core re-export 所有用到的 genai 类型**（`pub use genai::chat::{ChatRequest, ChatMessage, ...}`），下游一律写 `yourai_core::ChatRequest`——类型真相源收口，将来升级只动 core + model-genai 两处
- 下游 crate（loop/tui/tools）不直接依赖 genai

### 5.7 Error 类型设计 ✅ 已决策

**决策：两级错误。第一级按性质二分（终止 vs 故障），第二级按来源细分。**

```rust
/// 第一级：所有消费方只需要认识这一层
#[non_exhaustive]
pub enum YourAiError {
    /// 终止——不是故障（ESC、消费端消失、关停）
    Aborted(AbortReason),
    /// 一切故障的统一入口
    Error(ErrorKind),
}

#[non_exhaustive]
pub enum AbortReason {
    Cancelled,     // cancel token
    Disconnected,  // 对端消失（inbox 发送端 drop；或 outbox send 返回 false）
}

/// 第二级：各种类型的 Error 都住这里
#[non_exhaustive]
pub enum ErrorKind {
    Model    { source: genai::Error },                    // LLM 失败，原样携带（唯一有具体类型的下沉点）
    Provider { name: &'static str, message: String },     // 其他 provider 运行失败（统一形状，不强造子类）
    Tool     { name: String, message: String },           // 工具失败
    Config   (String),                                    // 装配/配置错误（缺必需 provider、参数非法）
    Loop     (String),                                    // loop 自身逻辑错误
    Other    (String),
}
```

**消费规则：**

- 前端只 match 第一级：`Aborted → 灰色"已停止"`，`Error(kind) → 红色错误`——永远不碰第二级细节
- loop 内部 `?` 直接流转，只在 `select!` 处造 `Aborted`
- 工具错误在 loop 层消化：`Err → ToolDone{is_error: true}` 喂回模型，不逃逸成 turn 失败
- `Config` 错误在**使用点**报（与 5.8 联动）：只有 loop 知道自己需要什么 provider，装配点不猜
- `#[non_exhaustive]` 保证加变体不炸下游

### 5.8 插件注册方式 ✅ 已决策

**决策：机制与策略分离——core 只有 builder + registry 机制；配置驱动是 CLI 层策略；inventory 永不。**

**机制层（core，唯一注册机制，两个时机共用）：**

```rust
// 装配期
Agent::builder().agent_loop(...).model(...)...build();   // 全字段 Option，不猜完整性
// 运行期（同一 API，MCP 重连/TUI 斜杠命令装工具免费获得）
agent.ctx().tools()?.register(handler);   // 单个
agent.ctx().tools()?.extend(handlers);    // 批量（MCP connect → Vec 一批进）
```

- **build() 不做必需性检查**——缺什么在使用点报 `Config` 错。原则：只有 loop 知道自己需要什么，装配点猜"必需/可选"必然猜错（trivial loop 不要 tools，headless loop 可没有 model）。typestate builder（编译期强制完整性）因此否决
- **Context 双读法**：`model()/tools()/...` 缺失报 `Config` 错；`try_model()/try_tools()/...` 缺失返回 `None`——loop 按下面的依赖矩阵自选读法
- **turn 开始时快照**（见 5.2）：`ProviderSnapshot` 里必需项直接是字段，可选项是 `Option<Arc<dyn>>`

**DefaultLoop 依赖矩阵：**

| Provider | DefaultLoop 视角 | 缺失时行为 |
|---|---|---|
| agent_loop | 必需（start 已查） | `Config` 错 |
| model / context_manager | 必需 | 首次使用时报 `Config` |
| tools | 工具循环需要 | 无则视作空工具集（纯聊天可用） |
| security | 可选 | 缺省 = 不拦截，直接执行 |
| sandbox | 可选 | 缺省 = 不施加沙箱 |
| session / memory / skills / usage / observability / hooks | 可选 | 缺省 = 跳过对应职责 |
- **安装期零依赖**：插件 install 时不给任何依赖，所有协作推迟到运行期经 Context 解决——消掉注册顺序问题

**策略层（CLI）：类型化直调，不做通用 from_config**

- CLI 编译期知道自己链了哪些 crate，直接调类型化构造函数（`GenaiModel::new` / `SeatbeltSandbox::new` / `McpServerConfig{..}`）
- 配置文件只是 builder 的序列化形式，section 与 crate 一一硬映射
- 三个"不"：**core 不认识配置格式**（永不读文件）、**impl crate 不读文件**（只收类型化参数）、**只有 CLI 认识 toml**

**Plugin（bundle）trait：v1 不加**——一个伴生 crate 常贡献多类组件（工具+hook+技能），bundle trait 可提供单一安装点，但 v1 函数返回值 + builder 手动接线够用；**触发条件**：CLI 需要"配置里按名字启用插件包"时再加（与通用 from_config 同一触发点，即动态扩展/wasm 时代）

**inventory 永不**，三条硬理由：cargo test 链接全部注册项（测试隔离性报废）；注册顺序未定义且与条件编译冲突；注册无代码位置，出问题无法 grep

---

## 6. 交互与并发模型

### 6.1 设计动机：为什么需要 spawn

**目标：turn 在等的时候，外面的事必须能继续做。**

一个 turn 的生命是一串等待（LLM 流式响应、工具子进程、人类审批），而每一刻等待恰好是外部有活要干的时刻：渲染 token、响应 ESC、回答审批、其他 session 并行。这是**功能需求**，不是性能优化——删掉任何一条产品就是坏的。

本质：**处理外部输入的控制流，和等待 turn 的控制流，不能是同一条。**

注意不是"腾空一个 core"——turn 挂在 `await` 上时一个核都不占，被占住的是调用方那条控制流（两个"等"被绑在同一条执行线上）。单线程 JS（pi/Claude Code 全跑在一个核上）和 tokio `current_thread` 都能跑整个 agent。core 只是控制流轮流借用的资源；agent 是 I/O 密集，借用频率极低。

事件循环 / 线程池 / spawn 是同一问题的三种回答（"当一段代码在等，谁来跑别的代码？"）：

| 机制 | 谁决定切换 | 代价 |
|---|---|---|
| 线程池（Hermes：`run_in_executor`） | OS 抢占 | 共享内存要加锁（`agent_ref[0]`、`result_holder[0]` 全是防御工事）；MB 级线程栈 |
| 事件循环（pi / JS） | 代码在 `await` 处主动让出 | 单线程死循环全卡（agent I/O 密集几乎不踩） |
| spawn（Rust / tokio） | 同事件循环，但 future 是冷的必须显式注册 | 冷 future / drop 即取消的心智负担 |

**Rust 必须显式 spawn 的原因：future 是冷的。** `select!` 会丢弃输家分支 = drop = 取消。"turn 后台跑 + 消费者等事件"在同一个 task 里做不到（事件先到，run future 就被 drop），turn 必须住在自己的 task 里。`tokio::spawn` 就是 JS `void asyncFn()` 的 Rust 拼法——pi 的 `agentLoop()` 内部正是 `void runAgentLoop(...)` fire-and-forget（JS promise 是热的创建即运行，Rust 必须写出来）。

统一模式：**自驱动的生产者（turn）→ 队列（channel）→ 按自己节奏消费的消费者（UI）**。spawn 的唯一目的：让生产者自驱动。

### 6.2 执行模型：start() 是主 API，run() 是非交互变体

core 里 spawn 只出现在一处——`Agent::start()` 内部（对应 pi 低层的 fire-and-forget）：

```rust
impl Agent {
    /// 非阻塞启动（pi 的 agentLoop()）：spawn turn，立即返回句柄
    pub fn start(self: &Arc<Self>, first: In) -> Result<TurnHandle, YourAiError> {
        let _ = self.ctx.agent_loop()?;            // 缺 loop 报 Config（5.8 使用点原则）
        let snap = self.ctx.snapshot();            // turn 级快照（5.2）
        let (inbox_tx, inbox_rx) = unbounded_channel();
        let (outbox_tx, outbox_rx) = unbounded_channel();
        let cancel = CancellationToken::new();
        let _ = inbox_tx.send(first);              // 启动输入 = inbox 第一条消息
        let result = tokio::spawn(async move {
            let loop_ = snap.agent_loop.clone();
            let tc = TurnContext {
                ctx: &ctx, snap,
                inbox: &mut inbox_rx,
                outbox: &ChannelSink::new(outbox_tx),
                cancel: &cancel,
            };
            loop_.run_turn(tc).await
            // 返回后 sink drop → outbox 关闭 = turn 结束信号
        });
        Ok(TurnHandle { inbox: inbox_tx, outbox: outbox_rx, cancel, result })
    }

    /// 阻塞变体（仅非交互）：outbox 接 DiscardSink（不建 channel）；
    /// loop 发 Ask → 内部取消 → 返回 Config 错（交互一律用 start）
    pub async fn run(&self, first: In) -> Result<TurnOutput, YourAiError> { /* NonInteractiveSink */ }
}

/// 一次 turn 的句柄——pi 的 EventStream + AbortController + result 三合一
pub struct TurnHandle {
    pub inbox:  UnboundedSender<In>,           // 外界 → loop（写消息/steer/审批答复）
    pub outbox: UnboundedReceiver<Out>,        // loop → 外界，关闭 = turn 结束
    pub cancel: CancellationToken,             // 控制面快路径
    result: Option<JoinHandle<Result<TurnOutput, YourAiError>>>,
}

impl TurnHandle {
    pub async fn join(self) -> Result<TurnOutput, YourAiError>;   // 等最终结果
    pub fn interrupt(&self);                                       // ESC
}

impl Drop for TurnHandle {
    fn drop(&mut self) { self.cancel.cancel(); }  // 丢弃句柄 = 取消 turn
}
```

### 6.3 通信原语（快慢双路径）

| 原语 | 方向 | 路径 | 语义 |
|---|---|---|---|
| `tc.outbox.send(m)` | loop → 调用方 | per-turn outbox | 同步推入永不阻塞 loop；返回 `bool`，false = 消费端已关闭（→ Disconnected）；channel 关闭 = turn 结束（零手动信号） |
| `tc.inbox` | 调用方 → loop | per-turn inbox（慢路径） | 排队信箱，loop **独占拉取**：steer、审批答复、一切业务消息；消费时机是 loop 的自由 |
| `tc.cancel` | 调用方 → loop | CancellationToken（快路径） | 绕过 inbox 立即生效；loop 用 `select!` 打断进行中的 LLM 流/工具/等待 |
| `handle.join()` | 调用方 | JoinHandle | 最终结果 + 生命周期终点 |

两条流各自**单写单读**，随 turn 生灭——不是 topic/pubsub：消费者集合固定为 1（订阅无意义）、流常驻性不成立（关闭即结束信号）、mpsc 无损（broadcast 的 Lagged 丢事件对 UI 是事故）。将来事件要多方消费，在 OutSink 层做 fan-out 组合，管道不变。

### 6.4 主函数的三种活法（四家实证）

async 之后"主线程"这个概念解体——剩下的不是一条主循环，而是**几个自治循环 + channel 编队**。主函数从不 poll agent，只等两种东西之一：

| 模式 | 主函数在等什么 | turn 的响应性谁来管 | 实证 |
|---|---|---|---|
| 脚本 | turn 结果（await 到底，然后退出） | 不需要 | pi `runPrintMode`（main.ts:841 → print-mode.ts:122）；= 我们的 `run()` |
| 交互 | 下一个用户输入（顺序对话循环） | 事件循环回调（pi）/ `select!` 分支（codex） | pi `interactive-mode.ts:908`；codex `app.rs:1168` |
| 服务器 | 关机信号（进程生命周期） | 每消息/request 各自 spawn | Hermes `main()` → `start_gateway()`（run.py:20707） |

pi 的 `await this.session.prompt()` 看似阻塞，但 JS 的 await 只挂起这一个 async 函数、不挂起事件循环：ESC keybinding 回调照样触发 `abort()`，流式 token 事件照样渲染，`{ streamingBehavior: "steer" }` 照样注入。await 只保证"上一个 turn 结束前不读下一个输入"——**循环结构天然强制单 turn 串行**。

codex-rs 是最显式的双循环形态：core 侧 spawn 的 `submission_loop` 消费 Op 通道直到 `Op::Shutdown`（session/mod.rs:725），TUI 侧 `select!` 四路复用（app.rs:1169-1216：内部 UI 事件 / core 会话事件 / 终端输入 / app-server 事件）。

### 6.5 YourAI TUI：pi 的外层 + codex 的内层

```rust
// 外层：pi 形状 —— 顺序循环，结构强制单 turn
loop {
    let first = tui.read_input().await;         // 前端 adapter：键盘 → In::UserText
    let handle = agent.start(first);             // spawn turn
    consume_turn(&mut tui, handle).await;        // 内层：codex 形状 select!
}                                                // turn 结束才回外层读下一条输入

// consume_turn 内部：select! 多路复用
tokio::select! {
    m = handle.outbox.recv() => render(m),       // match protocol::Out 渲染；None = turn 结束
    key = keyboard.next() => match key {
        Esc  => handle.cancel.cancel(),          // 快路径立即取消
        Line => { let _ = handle.inbox.send(In::UserText { text }); },  // steer
    },
}
// outbox 关闭后 join() → TurnOutput.pending 非空则逐条 start() 自动续 turn
```

服务器模式不用这个循环：每个请求 `start()` 后把 `handle.outbox` 接到 SSE 即可；**SSE 断开时丢弃 handle 即取消 turn**（Drop→cancel），无需额外清理。

**句柄生命周期备忘：** 要 fire-and-forget（如后台 turn 表），把 `TurnHandle` 存起来（`HashMap<TaskId, TurnHandle>`），不要随手 `drop`——drop 即取消。

与 codex 的两个有意差异：
1. **粒度**：codex 的 core 循环是 per-session 的（Op 队列串行消费，天然强制单 turn）；我们是 per-turn spawn，单 turn 串行由 TUI 外层结构（交互模式）或文档约定（服务器模式）保证
2. **中断路径**：codex 的 `Op::Interrupt` 要排队；我们的 `handle.cancel()` 走 CancellationToken 绕过队列立即生效

### 6.6 并发模型

**并发 = 多个独立的 turn 任务，仅此而已。**

```
内存态（独立）              持久层（共享）
┌─────────────┐
│ turn #1      │──写──┐
│ (session A)  │      │
└─────────────┘      ▼
┌─────────────┐   ┌──────────────┐
│ turn #2      │──►│ session 存储  │   ← SQLite/JSONL 文件
│ (session B)  │   │ (sessions.db) │      并发写由存储层保证
└─────────────┘   └──────────────┘
┌─────────────┐      ▲
│ turn #3      │──写──┘
│ (resume A)   │   ← 打开时从存储 load 历史
└─────────────┘
```

- 一个 turn = 一次 `start()`，交互管道（inbox/outbox channel、CancelToken）每次独立，随 TurnHandle 生命周期自动回收
- 持久层是唯一共享点，并发写由存储层（如 SQLite WAL）保证，core 完全不管
- 恢复 session = 用 `SqliteContextManager::open(db, session_id)` 重建，构造时自动 load 历史（见 5.4 方案 D）
- **同一 session 不支持并发 start**——TUI 模式由外层顺序循环结构保证；服务器模式文档约定，不做强制代码

### 6.7 multi-agent / subagent / plan-execute：工具面模式，不是第二种 loop

**调研结论（codex-rs + pi 实证）：ReAct 是唯一的 loop。不存在 plan-execute 或 multi-agent 的专用循环。**

codex 的三层做法：
1. **`SessionTask` 抽象**（tasks/mod.rs:214）——turn 之上的任务接口：`RegularTask`（一次普通 turn）、`ReviewTask`（内部启动完整子 codex 会话再回来，review.rs:74）、`CompactTask`、`UserShellCommandTask`。签名与我们的 `AgentLoop::run_turn` 同构
2. **multi-agent = 工具面**：`spawn_agent`/`send_message`/`wait_agent`/`close_agent`/`interrupt_agent`/`list_agents`/`followup_task`/`resume_agent`。每个子 agent = 完整子 codex session（`SubAgentSource::ThreadSpawn`）
3. **plan mode = `CollaborationMode::Plan` 换注入指令和工具面**（inject.rs:58），底下还是同一个 ReAct loop

pi 的做法：故意什么都不带，官方答案 = subagent 扩展（工具内 spawn 独立 pi 进程）。

**对本项目的结论：**
1. multi-agent 和 plan-execute 不是"第二种 loop"，是 ReAct 之上的两种模式：
   - multi-agent = 工具面扩展（spawn/wait/send）+ 子 agent 是独立 session
   - plan-execute = 提示词 + plan 工具，控制流还是 ReAct
   - **编排智能在模型行为里，不在代码结构里**——代码只提供工具面和子 session 管理
2. **"subagent = 一个 Tool 实现"被两家同时验证**（codex 工具内 spawn 子 session；pi 扩展内 spawn 子进程）✅
3. loop 可替换性保留但降级为保险：trait 在那，但第一方写不出需要第二个 loop 的场景。`yourai-loop-default` 出厂（5.1）的决策更稳
4. codex 的 `SessionTask` 暂不引入：review/compact 类"会话内非 turn 工作流"将来要么实现 AgentLoop 要么做成内置工具，trait 层已覆盖
5. 多 agent 协作工具（spawn/wait/send 等）属于 `yourai-tools-*` 层（如 `yourai-tools-subagent`），core 零感知

**Agent = 配置**（system prompt + 工具集 + 模型 + loop），**Session = 一次持久化对话**：

| 场景 | 本质 | 做法 |
|---|---|---|
| 同一配置开两个对话 | 2 个 session | 相同配置 builder 两次，各自 start |
| 不同配置（coder + researcher） | 2 个 session | 不同配置的 builder，各自 start |
| 子 agent | 一个 Tool 调用产生的独立 run | 见下 |

**子 agent = 一个 Tool 实现**，core 零改动：

```rust
struct SubagentTool { /* 子 agent 配置 */ }

impl ToolHandler for SubagentTool {
    fn execute<'a>(&'a self, tc: ToolContext<'a>, input: Value) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            let child = Agent::builder()
                .context_manager(Arc::new(
                    SqliteContextManager::open(db, SessionId::new())?  // 新 session
                ))
                // ...
                .build();
            let handle = child.start(In::UserText { text: task_prompt })?;
            // 子 agent 的 outbox 逐条 Out → 序列化进 ToolProgress.payload 转发
            while let Some(out) = handle.outbox.recv().await {
                let payload = serde_json::to_value(&out)?;
                if !tc.emit_progress(payload) {            // id 自动 = tc.call_id
                    return Err(YourAiError::Aborted(AbortReason::Disconnected));
                }
            }
            let output = handle.join().await?;
            Ok(json!({ "result": output.text }))
        })
    }
}
```

真正的多 agent 协作工具面（spawn/wait/send_message，参照 codex multi_agents）是后续阶段，做成 `yourai-tools-subagent` crate，不在第一期。

### 6.8 与同类 agent 的对比

| | YourAI | codex-rs | pi | Claude Code | Hermes |
|---|---|---|---|---|---|
| 循环形态 | 2 个：turn 任务 + UI select! | 2 个：core submission_loop + TUI select! | 0 显式（JS 运行时事件循环） | 0 显式（runtime + React render） | 1 编排 + sidecars（5 秒轮询） |
| 非阻塞启动 | `start()` spawn → TurnHandle | submit Op → session loop | `void agentLoop()` → EventStream | — | `run_in_executor` |
| 阻塞语义 | `run()` 非交互变体（无 channel；遇 Ask 报 Config） | — | harness `await executeTurn` | — | `asyncio.wait` 轮询 |
| 事件出 | outbox → per-turn channel | Event channel | EventStream（push 队列） | setState → Ink 重渲染 | 15 个构造器回调 |
| 命令入 | per-turn inbox（loop 拉取） | Op channel（dispatcher 解释） | 队列 getters + AbortSignal | promise resolve | 线程 flag + 轮询兜底 |
| 中断 | CancellationToken（快路径） | CancellationToken | AbortSignal | AbortSignal | 跨线程 flag |
| 单 turn 串行 | TUI 外层结构 / 文档约定 | Op 队列串行化（强制） | main await prompt（结构强制） | — | session 内部 |

压法：**pi 的"交互管道是 run 参数" + codex 的 select! 双循环 + Rust 惯用的 CancellationToken，以 per-turn handle 的粒度合成一层。**

---

## 7. DefaultLoop 设计

`yourai-loop-default` 提供的 `DefaultLoop` 是开箱即用的 ReAct 循环，基于 genai 的流式 API。

### 7.1 流程图

```
run_turn(tc)
  │
  ├─ ① tc.inbox.recv() → In::UserText → add_user_message      // 持久化用户消息
  │
  └─►┌──────────── ReAct step 循环 ────────────┐
      │                                          │
      │  ② drain tc.inbox                        │
      │     • In::UserText → 注入历史 (steer)     │
      │     • In::Reply → 路由到待审批 / 暂存      │
      │     • tc.cancel.is_cancelled() → Aborted │
      │                                          │
      │  ③ history.build_request_with(tools)     │
      │  ④ tc.snap.model → stream(ModelRequest)  │
      │     select! {                            │
      │       chunk → outbox.send(Out::Chunk)    │
      │       cancel → 半截文本入历史             │
      │             → Err(Aborted(Cancelled))    │  ← ESC 瞬间生效
      │     }                                    │
      │  ⑤ history.add_assistant_message(...)    │  // 持久化
      │                                          │
      │  ⑥ 无 tool_calls → 返回 TurnOutput       │
      │     { text, usage, pending: 残留消息 }    │
      │                                          │
      │  ⑦ 逐个执行 tool_calls：                  │
      │     • 第一层 security.check_tool_call()  │
      │       ├─ Allow → 执行                     │
      │       ├─ Deny → 结果 = "denied"          │
      │       └─ Ask   → outbox.send(Out::Ask)   │
      │                  select! 等 In::Reply     │
      │     • ToolContext{call_id, security,     │
      │       sandbox} → tools.execute(tc, ...)  │
      │       （第二层 check_command/文件检查、   │
      │        sandbox.apply(Command) 由具体工具 │
      │        用 tc 注入的能力自调）             │
      │     • history.add_tool_result(ToolResponse) │
      │                                          │
      └──────────── 回到 ②（模型对结果反应）──────┘
```

### 7.2 关键实现点

**流式 + 可取消（genai ChatStream）：**

```rust
let model = tc.snap.model.ok_or_else(missing("model"))?;
let mut stream = model.stream(req).await?.stream;      // ChatStreamResponse.stream → ChatStream
let mut text = String::new();
loop {
    tokio::select! {
        ev = stream.next() => match ev {
            Some(Ok(ChatStreamEvent::Chunk(c))) => {
                text.push_str(&c.content);
                if !tc.outbox.send(Out::Chunk { text: c.content }) {
                    return Err(YourAiError::Aborted(AbortReason::Disconnected));
                }
            }
            Some(Ok(ChatStreamEvent::End(end))) => break,   // 拿 usage + captured_tool_calls
            Some(Err(e)) => return Err(e.into()),
            None => break,
        },
        _ = tc.cancel.cancelled() => {
            // 半截文本入历史（保留，Codex 式中断标记留作未来配置项）
            return Err(YourAiError::Aborted(AbortReason::Cancelled)),   // 打断 HTTP 流
        }
    }
}
```

**等待 Ask 答复（select! 双等 + 无关消息暂存）：**

```rust
/// 等某个 Ask 的答复；无关消息（steer 等）放进暂存队列，退出前并入 TurnOutput.pending
async fn wait_reply(tc: &mut TurnContext<'_>, stash: &mut Vec<In>, id: &str) -> Result<Value> {
    loop {
        tokio::select! {
            m = tc.inbox.recv() => match m {
                Some(In::Reply { id: got, payload }) if got == id => return Ok(payload),
                Some(other) => stash.push(other),    // 无关消息不丢
                None => return Err(YourAiError::Aborted(AbortReason::Disconnected)),   // outbox 端已走
            },
            _ = tc.cancel.cancelled() => return Err(YourAiError::Aborted(AbortReason::Cancelled)),
        }
    }
}
```

**工具结果注入（genai 格式）：** 用 `ChatRequest::append_tool_use_from_stream_end` 或手动构造 assistant 消息 + tool response 消息，保持 genai 的消息格式。

### 7.3 DefaultLoop 的可选配置

```rust
pub struct DefaultLoopConfig {
    pub system_prompt: Option<String>,
    pub max_steps: u32,                    // 防无限循环，默认 32
    pub parallel_tool_calls: bool,         // 默认 false（串行）
    pub steer_mode: SteerMode,             // InjectNow / NextTurn
}

pub enum SteerMode {
    /// turn 中到达的消息立即注入下一轮（pi 的 steer）
    InjectNow,
    /// turn 中到达的消息排队，turn 结束后作为新 turn
    NextTurn,
}
```

### 7.4 用户自定义 loop 的最小示例

```rust
struct MyLoop;

impl AgentLoop for MyLoop {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, Result<TurnOutput, YourAiError>> {
        Box::pin(async move {
            let model = tc.snap.model.ok_or_else(missing("model"))?;
            let history = tc.snap.context_manager.ok_or_else(missing("context_manager"))?;

            if let Some(In::UserText { text }) = tc.inbox.recv().await {
                history.add_user_message(&text).await?;
            }
            let resp = model.complete(ModelRequest::new(
                history.build_request(), history.default_options())).await?;
            let text = resp.into_first_text().unwrap_or_default();
            history.add_assistant_message(&text).await?;
            Ok(TurnOutput { text, usage: None, pending: vec![] })
        })
    }
}
```

不想要工具、不想要审批、不想要流式？一个 loop 十行代码。这是"乐高"的意义——机制在 core，策略在插件。

---

## 8. 用户使用流程（预期）

### 8.1 快速开始（用默认实现）

```rust
use yourai_core::prelude::*;
use yourai_loop_default::DefaultLoop;
use yourai_model_genai::GenaiModel;
use yourai_context_inmemory::InMemoryContextManager;
use yourai_session_sqlite::SqliteSessionManager;
use yourai_tools_builtin::BuiltinToolRegistry;
use yourai_sandbox_seatbelt::SeatbeltSandbox;

let agent = Agent::builder()
    .agent_loop(Arc::new(DefaultLoop::new()))
    .model(Arc::new(GenaiModel::new("deepseek-chat")))
    .context_manager(Arc::new(InMemoryContextManager::new()))
    .session(Arc::new(SqliteSessionManager::open("sessions.db")?))
    .tools(Arc::new(BuiltinToolRegistry::new()))
    .sandbox(Arc::new(SeatbeltSandbox::new(SandboxPolicy::workspace_write())))
    .build();

let output = agent.run(In::user_text("hello")).await?;  // 非交互：无工具审批 Ask 时安全
```

### 8.2 自定义 AgentLoop

```rust
struct MyLoop;

impl AgentLoop for MyLoop {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, Result<TurnOutput, YourAiError>> {
        Box::pin(async move {
            // 完全自定义编排逻辑（providers 从 tc.snap 快照读）
            let model = tc.snap.model.ok_or_else(missing("model"))?;
            let history = tc.ctx.context_manager()?;
            // ...
            Ok(TurnOutput { text: "custom response".into(), usage: None, pending: vec![] })
        })
    }
}

let agent = Agent::builder()
    .agent_loop(Arc::new(MyLoop))
    // ... 其他用默认
    .build();
```

### 8.3 自定义 Tool

```rust
struct WeatherTool;

impl ToolHandler for WeatherTool {
    fn name(&self) -> &str { "get_weather" }

    fn definition(&self) -> Tool {
        // genai 0.6.5 API：Tool::new(name) + with_description / with_schema
        Tool::new("get_weather")
            .with_description("Get current weather for a city")
            .with_schema(schema!({
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"]
            }))
    }

    fn execute<'a>(&'a self, tc: ToolContext<'a>, input: Value) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            let city = input["city"].as_str().unwrap_or("unknown");
            // 调用天气 API
            Ok(serde_json::json!({ "city": city, "temp": 25 }))
        })
    }
}

// 注册到已有的 registry
let registry = agent.ctx().tools()?;
registry.register(Arc::new(WeatherTool));
```

### 8.4 运行时热替换

```rust
// 运行中切换模型
agent.ctx().set_model(Arc::new(GenaiModel::new("gpt-4o")));

// 运行中加载新工具
agent.ctx().tools()?.register(Arc::new(WeatherTool));

// 运行中切换 agent loop
agent.ctx().set_agent_loop(Arc::new(DifferentLoop));
```