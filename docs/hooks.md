# YourAI Hook 系统设计（历史稿）

> **状态：已被 [`hook-protocol.md`](./hook-protocol.md) 取代。** 本文保留早期设计背景，
> 其中信封协议、事件数量、串行 waterfall、旧 trait 签名等内容不再是实现规范，不能作为
> 当前代码的验收依据。架构总览见 [`architecture.md`](./architecture.md)。

## 0. 核心执行模型

### 0.1 先说结论

Hook 不是一套独立于 Loop 的流程，也不是一个能够自行修改 Agent 状态的回调。
它本质上是 Loop 在固定位置发起的一次有类型的请求：

```text
Loop 到达 Hook Point
  → 构造 HookEvent
  → HookRuntime 匹配 Handler
  → Handler 接收输入并执行
  → HookRuntime 解析、校验、聚合输出
  → 返回 HookDispatchResult
  → Loop 根据当前 Hook Point 的规则消费结果
  → Loop 继续、阻断、询问用户、修改工具输入或注入上下文
```

这里有三个职责不同的角色：

| 角色 | 负责 | 不负责 |
|---|---|---|
| Hook Point Owner | 决定何时触发，并消费最终结果；prompt/tool Hook 的 Owner 是 `DefaultLoop` | 不负责执行 shell、解析外部 JSON |
| HookRuntime | 匹配、执行、超时、取消、解析、校验和聚合 Handler | 不直接修改历史、执行工具或向用户提问 |
| Hook Handler | 根据输入产生一个结果；可以是 Rust callback、command 或 HTTP | 不直接控制 Loop，不读取 turn inbox |

所以，Hook 真正接入 Loop 的方式就是一行有语义的调用：

```rust
let result = hooks.dispatch(invocation, &tc.cancel).await?;
```

但是这行调用必须放在正确位置，而且调用后的结果必须由 Loop 明确消费。只定义
`HookRegistry::dispatch()` 而不修改 `DefaultLoop`，Hook 就没有真正接入系统。

### 0.2 通用 Hook 协议

通用 Hook 不等于所有事件共享同一坨业务字段。正确的抽象是：

```text
固定 Envelope + Hook Point 专属 Input/Output Schema
```

所有 Handler，无论是 command、HTTP 还是 Rust callback，都接收同一种调用信封：

```rust
pub struct HookInvocation<I> {
    pub protocol_version: u32,
    pub invocation_id: String,
    pub hook_point: HookPoint,
    pub context: HookContext,
    pub input: I,
}

pub struct HookContext {
    pub session_id: String,
    pub turn_id: Option<String>,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
}
```

对应的 wire JSON：

```json
{
  "protocol_version": 1,
  "invocation_id": "hook_call_01",
  "hook_point": "SomeHookPoint",
  "context": {
    "session_id": "sess_01",
    "turn_id": "turn_09",
    "cwd": "/workspace",
    "model": "model-name",
    "permission_mode": "default"
  },
  "input": {
    "field_defined_by_this_hook_point": "value"
  }
}
```

所有 Handler 也返回同一种响应信封：

```rust
pub struct HookResponse<O> {
    pub protocol_version: u32,
    pub invocation_id: String,
    pub control: HookControl,
    pub output: O,
    pub user_message: Option<String>,
}

pub enum HookControl {
    Continue,
    Block { reason: String },
}
```

对应的 wire JSON：

```json
{
  "protocol_version": 1,
  "invocation_id": "hook_call_01",
  "control": {
    "action": "continue"
  },
  "output": {
    "field_defined_by_this_hook_point": "value"
  },
  "user_message": null
}
```

其中：

- Envelope 解决版本、调用标识、运行上下文和控制流；
- `input` 是 Hook Point 对外暴露的数据；
- `output` 是 Hook Point 允许 Handler 返回的数据；
- `control` 只有通用的继续或阻断；
- 脚本不能返回任意系统 patch，也不能指定“调用某个内部函数”。

每一个 Hook Point 必须注册一份契约：

```rust
pub trait HookPointSpec {
    type Input: Serialize;
    type Output: DeserializeOwned;
    type MergedOutput;

    const POINT: HookPoint;

    fn validate(output: &Self::Output) -> Result<(), HookProtocolError>;
    fn reduce(outputs: Vec<Self::Output>) -> Result<Self::MergedOutput, HookError>;
}
```

因此，系统的“通用”发生在执行机制上；具体 Hook Point 仍然拥有自己的输入 schema、
输出 schema、聚合规则和应用位置。否则一个万能 `serde_json::Value` 最终只会把类型
判断散落到整个 Loop。

### 0.3 返回参数怎么进入系统

外部程序的 stdout 不能自动进入系统。它必须经过四道边界：

```text
raw stdout
  → Executor 解析为 HookResponse<Output>
  → HookPointSpec 校验 Output
  → HookPointSpec.reduce 聚合多个 Handler 输出
  → Hook Point Owner 把 MergedOutput 应用到当前操作状态
```

通用接入代码可以固定成：

```rust
// HookRuntime 是装进 ProviderSnapshot 的 trait object，因此边界处类型擦除为 Value。
pub trait HookRuntime: Send + Sync {
    fn dispatch<'a>(
        &'a self,
        invocation: HookInvocation<Value>,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<RawHookDispatchResult, HookError>>;
}

pub struct RawHookDispatchResult {
    pub responses: Vec<HookResponse<Value>>,
    pub runs: Vec<HookRun>,
}

// 泛型只存在于 Hook Point adapter，不放进 dyn HookRuntime。
async fn run_hook_point<P: HookPointSpec>(
    runtime: &dyn HookRuntime,
    context: HookContext,
    input: P::Input,
    cancel: &CancellationToken,
) -> Result<HookPointResult<P::MergedOutput>, HookError> {
    let invocation_id = new_id();
    let raw = runtime.dispatch(HookInvocation {
        protocol_version: 1,
        invocation_id: invocation_id.clone(),
        hook_point: P::POINT,
        context,
        input: serde_json::to_value(input)?,
    }, cancel).await?;

    let mut outputs = Vec::with_capacity(raw.responses.len());
    let mut controls = Vec::with_capacity(raw.responses.len());
    for response in raw.responses {
        validate_envelope(&invocation_id, P::POINT, &response)?;
        let output: P::Output = serde_json::from_value(response.output)?;
        P::validate(&output)?;
        controls.push(response.control);
        outputs.push(output);
    }

    Ok(HookPointResult {
        control: reduce_controls(controls),
        output: P::reduce(outputs)?,
        runs: raw.runs,
    })
}
```

业务系统中的接入点只有两步：捕获输入，然后应用输出。

```rust
let hook_input = Point::capture(&operation_state);
let hook_result = run_hook_point::<Point>(
    hooks,
    hook_context,
    hook_input,
    &tc.cancel,
).await?;

match hook_result.control {
    HookControl::Continue => Point::apply(
        &mut operation_state,
        hook_result.output,
    )?,
    HookControl::Block { reason } => return Point::blocked(reason),
}

continue_original_operation(operation_state).await
```

这就是返回参数“进入系统”的唯一入口：

```text
Point::apply(&mut operation_state, validated_output)
```

它不是全局 EventBus 广播，也不是 HookRuntime 随意修改 `ContextManager`、工具或 UI。
每个 Hook Point 的 Owner 手里正好持有那一刻的局部操作状态，所以只有 Owner 知道怎样
安全地应用结果。

### 0.4 注册、触发、执行、消费的完整闭环

```text
一、注册阶段
配置 / Plugin / Rust API
  → HookRegistration {
      point, matcher, executor, order, timeout, failure_policy
    }
  → HookRuntime Registry

二、触发阶段
业务代码运行到固定 Hook Point
  → Point::capture(operation_state)
  → HookRuntime.dispatch(invocation)

三、执行阶段
Registry 按 point + matcher 找到 registrations
  → Executor 执行 Native / Command / HTTP Handler
  → 每个 Handler 得到相同 Invocation Envelope
  → 每个 Handler 返回相同 Response Envelope

四、归一化阶段
Executor 解释 transport 状态
  → 解析 Response
  → 校验版本、invocation_id、schema、大小限制
  → 按 FailurePolicy 处理超时和错误
  → Point::reduce(handler_outputs)

五、消费阶段
HookRuntime 返回 HookPointResult
  → Hook Point Owner 检查 control
  → Point::apply(operation_state, output)
  → 原业务流程继续或结束
```

HookRuntime 不需要知道 `output` 最后如何改变业务；Owner 不需要知道 Handler 是脚本、
HTTP 还是 Rust。这两者通过 `HookPointSpec` 连接。

### 0.5 Command Hook 只是一个 Executor Adapter

外部脚本采用一次请求、一次响应的进程模型：

```text
YourAI                     hook process
   │ spawn(command)             │
   │───────────────────────────►│
   │ stdin: JSON + "\n"         │
   │───────────────────────────►│
   │ close stdin                │
   │───────────────────────────►│
   │            stdout: JSON    │
   │◄───────────────────────────│
   │            stderr: log     │
   │◄───────────────────────────│
   │            exit(code)      │
   │◄───────────────────────────│
```

输入、输出和退出码各自承担不同职责：

- `stdin`：一个完整的事件 JSON，以换行结尾，然后关闭；脚本从 stdin 读取，不把
  大 JSON 放在命令行参数中。
- `stdout`：唯一的机器可读业务响应。结构化能力，例如阻断、权限决策、修改输入和
  添加上下文，都必须通过 JSON 表达。
- `stderr`：只用于诊断信息和错误说明，不能承载成功结果。
- `exit code`：表示进程层状态，不承担完整业务协议。

第一期约定：

| 退出码 | 含义 | Runtime 处理 |
|---:|---|---|
| `0` | Handler 正常完成 | 解析 stdout；空输出等价于 Continue |
| `2` | 兼容 Claude 的快捷阻断 | 将 stderr 转为当前 Hook Point 的 Block/Deny |
| 其他非零值 | Handler 执行失败 | 按该注册项的 `FailurePolicy::Open/Closed` 处理 |

不建议使用 `1 = 拒绝`。脚本崩溃、依赖缺失和普通异常通常也会返回 `1`；如果把它
直接当成拒绝，就无法区分“策略拒绝”和“Hook 坏了”。明确的拒绝应使用 stdout JSON，
`2` 只保留为命令行兼容快捷方式。

下面只是某个具体 Hook Point 的 input 示例：

```json
{
  "session_id": "sess_01",
  "turn_id": "turn_09",
  "cwd": "/workspace/shop",
  "permission_mode": "default",
  "hook_event_name": "PreToolUse",
  "tool_name": "get_order",
  "tool_use_id": "call_42",
  "tool_input": { "order_id": "A10001" }
}
```

脚本要求进入用户确认流程时返回：

```json
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "ask",
    "permissionDecisionReason": "该操作将读取客户订单 A10001"
  }
}
```

脚本退出 `0`。`ask` 是业务决策，不是进程错误，因此不能通过 exit `1` 表示。

### 0.6 HookRuntime 如何处理一次调用

`HookRuntime::dispatch()` 内部执行以下步骤：

1. 根据 `invocation.event.point()` 取得该 Hook Point 的注册项快照；
2. 使用事件专属 matcher 过滤，例如 `PreToolUse` 匹配 `get_order`；
3. 将内部 `HookInvocation` 序列化成 wire JSON；
4. 对 command handler 启动子进程，设置 cwd 和受控环境变量；
5. 向 stdin 写入 JSON 加换行，然后关闭 stdin；
6. 同时读取 stdout/stderr，并等待进程退出；
7. 与 turn cancellation、handler timeout 做竞速，取消时终止整个进程组；
8. 先解释退出码，再对成功响应解析 JSON；
9. 校验 `hookEventName`、字段类型和当前 Hook Point 允许的效果；
10. 聚合多个 Handler 的结果，返回类型化 `HookDispatchResult`。

Runtime 返回的是数据，不产生业务副作用：

```rust
pub struct HookDispatchResult {
    pub outcome: HookPointOutcome,
    pub runs: Vec<HookRun>,
}
```

其中 `runs` 用于日志、耗时和错误展示，`outcome` 才交给 Loop 消费。这样可以避免
stdout、stderr 或某个脚本异常被误当成模型上下文。

### 0.7 示例：UserPromptSubmit 记忆召回

`DefaultLoop` 收到一条用户消息以后、构造模型请求以前，必须显式调用 HookRuntime：

```rust
let text = receive_user_text(&mut tc).await?;

let hook_result = match &tc.snap.hooks {
    Some(hooks) => hooks.dispatch(
        HookInvocation::user_prompt_submit(&tc.meta, text.clone()),
        &tc.cancel,
    ).await?,
    None => HookDispatchResult::continue_user_prompt(),
};

let outcome = hook_result.expect_user_prompt_submit()?;
if let HookControl::Block { reason } = outcome.control {
    return stop_turn(reason);
}

history.add_user_message(&text).await?;
for context in outcome.additional_contexts {
    history.add_hook_context(context).await?;
}

let request = history.build_request_with(tools).await?;
let response = model.stream(request, &tc.cancel).await?;
```

如果 Handler 是记忆服务脚本，则完整的数据流是：

```text
用户输入
  → DefaultLoop 创建 UserPromptSubmit event
  → CommandHookHandler 把 event JSON 写入脚本 stdin
  → 脚本调用外部 memory API
  → 脚本 stdout 返回 additionalContext JSON，exit 0
  → HookRuntime 校验并返回 UserPromptSubmitOutcome
  → DefaultLoop 调 ContextManager::add_hook_context
  → ContextManager 把用户消息和 HookContext 一起构造成模型请求
  → 模型看到本次召回的记忆
```

记忆脚本的输出示例：

```json
{
  "hookSpecificOutput": {
    "hookEventName": "UserPromptSubmit",
    "additionalContext": "用户偏好使用中文；上次讨论的是订单退款流程。"
  }
}
```

关键消费动作不是“把 stdout 拼到 prompt 字符串”，而是由 Loop 调用
`ContextManager::add_hook_context()` 写入一个有来源、信任级别和生命周期的上下文项。
该项随后进入 transcript、恢复和 compact 流程，模型可见的数据不会成为隐藏状态。

### 0.8 示例：PreToolUse 工具审核

模型返回 tool call 以后、真正执行工具以前，Loop 触发 `PreToolUse`：

```rust
let validated_input = tools.validate(&call.name, call.input)?;
let hook_outcome = dispatch_pre_tool_use(
    &tc, &call.id, &call.name, validated_input,
).await?;

if let HookControl::Block { reason } = hook_outcome.control {
    history.add_denied_tool_result(&call.id, reason).await?;
    continue;
}

let input = hook_outcome.updated_input;
let security = security.check_tool_call(&call.name, &input).await?;
let decision = merge_permission(hook_outcome.permission, security);

match decision {
    Deny(reason) => add_denied_tool_result(reason),
    Ask(reason) => {
        // 只有 Loop 拥有 inbox，因此只有 Loop 能完成用户确认。
        tc.outbox.send(Out::Ask { id: call.id.clone(), reason, /* ... */ });
        let reply = wait_matching_reply(&mut tc, &call.id).await?;
        if reply.approved() {
            execute_tool(input).await?;
        } else {
            add_denied_tool_result("user denied");
        }
    }
    Allow => execute_tool(input).await?,
}
```

因此，脚本返回 `ask` 并不意味着脚本自己弹窗或等待用户。脚本已经执行结束；
`HookRuntime` 把 `ask` 变成类型化结果；最后由 `DefaultLoop` 发 `Out::Ask`、等待
`In::Reply`，并决定是否调用工具。

### 0.9 Claude Code 实际怎么设计

Claude Code 的实现也是“调用者触发、通用执行器运行、调用者消费”，代码链条为：

```text
用户消息：
processUserInput
  → executeUserPromptSubmitHooks
  → executeHooks
  → execCommandHook / execHttpHook / callback
  → parseHookOutput + processHookJSONOutput
  → processUserInput 消费 blockingError/additionalContexts

工具调用：
toolExecution
  → runPreToolUseHooks
  → executePreToolHooks
  → executeHooks
  → execCommandHook / execHttpHook / callback
  → 聚合 permissionBehavior/updatedInput/additionalContexts
  → toolExecution 合并普通权限判断
  → ask/deny/allow
  → tool.call(processedInput)
```

Claude Command Hook 的具体协议是：

- 使用 shell 启动 command；
- 把事件对象 JSON 序列化后以 `JSON + "\n"` 写入 stdin；
- 同时收集 stdout、stderr，并受 timeout/abort signal 控制；
- stdout 以 `{` 开头时尝试按 schema 解析 JSON，否则作为普通文本；
- exit `0` 表示成功；exit `2` 表示 blocking error；其他非零值是
  non-blocking error；
- JSON 可返回 `continue`、`stopReason`、`systemMessage`，以及事件专属的
  `permissionDecision`、`updatedInput`、`additionalContext`；
- 多个 Hook 并行执行，权限结果按 `deny > ask > allow` 聚合；
- 最终结果不是由 Hook 执行器自行应用，而是由 `processUserInput` 或
  `toolExecution` 消费。

以记忆为例，Claude 的 `processUserInput` 会把 `additionalContexts` 转换成一个
`hook_additional_context` attachment，追加到本次消息集合中，再把这组消息交给模型。
以工具审核为例，`toolExecution` 先读取 PreToolUse 的 `permissionBehavior` 和
`updatedInput`，然后进入正常的权限解析与用户确认流程。

Claude 的优点是协议成熟、兼容 command/HTTP/callback，并且执行路径集中。我们不应
照搬的地方是它用 `AsyncGenerator` 混合传输进度 UI、诊断消息和业务结果，而且所有
Hook 并行执行。当多个 PreToolUse Hook 都修改 `updatedInput` 时，并行完成顺序不适合
成为输入修改语义。因此 YourAI 的设计是：

| 方面 | Claude Code | YourAI 设计 |
|---|---|---|
| 进程输入 | stdin JSON + 换行 | 相同 |
| 进程输出 | stdout JSON，兼容普通文本 | JSON 为正式协议，普通文本只做有限兼容 |
| 退出码 | `0` 成功、`2` 阻断、其他非阻断错误 | 相同基础语义，再由 `FailurePolicy` 决定失败开闭 |
| 结果传输 | AsyncGenerator 同时 yield 进度和业务结果 | 一次返回 `HookDispatchResult { outcome, runs }` |
| UserPromptSubmit | 调用者追加 additional context attachment | Loop 写入明确的 HookContext conversation item |
| PreToolUse 调度 | 多 Handler 并行，权限按优先级聚合 | 串行 waterfall，使后一个看到前一个的 updated input |
| 用户确认 | 工具流程消费 ask 并调用权限 UI | Loop 消费 Ask，通过统一 `Out::Ask/In::Reply` 完成 |

最重要的边界是：**HookRuntime 负责把外部程序变成可信的类型化结果；Loop 负责让这个
结果在 Agent 流程中真正生效。**

## 1. 目标

Hook 为 YourAI 的稳定执行流程提供可配置扩展点，使用户能够在不修改
`AgentLoop` 主流程的情况下：

- 在用户消息提交前召回记忆、注入项目上下文或执行策略校验；
- 在工具执行前审核、修改输入、允许、拒绝或要求用户确认；
- 在工具执行后记录审计信息或补充模型上下文；
- 在会话、turn 和压缩生命周期中执行通知、清理或观测逻辑；
- 使用 Rust handler、外部命令或 HTTP 服务实现相同的 Hook 语义。

Hook 系统必须完整回答四个问题：

1. **Hook Point**：系统在哪些位置开放 Hook？
2. **Registration**：Handler 如何挂到 Hook Point？
3. **Protocol**：每个 Hook 的输入和输出是什么？
4. **Runtime Integration**：Hook 如何执行、聚合，并由主流程应用结果？

## 2. 核心原则

### 2.1 Hook 返回效果，不直接修改系统

Hook Handler 不直接访问或修改 `ContextManager`、`ToolRegistry`、模型请求和 UI。
它只返回声明式效果，例如：

- 添加上下文；
- 修改工具输入；
- 允许、拒绝或要求用户确认工具调用；
- 阻止当前操作；
- 给用户发送通知。

Hook Point 的拥有者负责验证并应用这些效果。

```text
Hook Handler ──返回效果──► Hook Runtime ──聚合结果──► Hook Point Owner
                                                               │
                                  ┌────────────────────────────┤
                                  ▼                            ▼
                         ContextManager                  Out::Ask / Tool
```

### 2.2 Hook 不拥有 inbox

`AgentLoop` 是 turn inbox 的独占消费者。Hook 不能自己等待 `In::Reply`，也不能
直接向用户提问。

当 `PreToolUse` Hook 返回 `Ask` 时：

1. Hook 执行结束；
2. `DefaultLoop` 发出 `Out::Ask`；
3. `DefaultLoop` 等待匹配的 `In::Reply`；
4. `DefaultLoop` 根据回复执行或拒绝工具。

### 2.3 Hook Point 由掌握控制权的组件触发

- 会话 Hook 由 Session/应用协调层触发；
- prompt、工具、turn Hook 由 `AgentLoop` 触发；
- compact Hook 由发起压缩的 `AgentLoop` 或 `ContextManager` 协调层触发。

`yourai-core` 只定义 Hook 契约，不主动解释或触发业务 Hook，继续保持运输权、
解释权、节奏权的边界。

### 2.4 内部协议与外部 Wire 协议分离

Rust 内部使用类型化事件和结果。Command/HTTP Hook 使用稳定 JSON wire 格式。
外部 JSON 必须先校验并转换成内部结果，不能把任意 `Value` 直接交给主循环。

### 2.5 模型可见即记录

Hook 注入的上下文必须作为明确标记的 conversation item 写入会话记录，不能只在
最后一刻偷偷拼到模型请求中。这样可以保证恢复、审计、压缩和调试看到同一事实。

## 3. Hook Point 契约

一个 Hook Point 不只是事件名，还包括拥有者、触发时机、匹配方式、允许效果、
调度方式、聚合方式和失败策略。

概念模型：

```rust
pub struct HookPointContract {
    pub point: HookPoint,
    pub owner: HookOwner,
    pub matcher: MatcherKind,
    pub dispatch: DispatchStrategy,
    pub allowed_effects: &'static [HookEffectKind],
    pub default_failure_policy: FailurePolicy,
}
```

### 3.1 第一期标准 Hook Point

| Hook Point | 拥有者 | 精确触发时机 | 匹配字段 | 允许效果 | 调度方式 |
|---|---|---|---|---|---|
| `SessionStart` | Session/应用协调层 | 会话加载后、首个 turn 前 | start source | 添加上下文、通知 | 并行收集 |
| `TurnStart` | AgentLoop | turn 建立后、处理首条输入前 | 无 | 观察、通知 | 并行收集 |
| `UserPromptSubmit` | AgentLoop | 收到用户文本后、写历史和调用模型前 | 无 | 阻断、添加上下文、通知 | 并行收集 |
| `PreToolUse` | AgentLoop | tool call 校验后、权限判断和执行前 | tool name | 修改输入、Allow/Ask/Deny、添加上下文 | 串行 waterfall |
| `PostToolUse` | AgentLoop | 工具完成后、结果写历史前 | tool name | 添加上下文、通知 | 并行收集 |
| `PreCompact` | 压缩协调者 | 调用 `compact()` 前 | manual/auto | 阻断、添加压缩指令 | 串行 |
| `PostCompact` | 压缩协调者 | 新摘要生成后 | manual/auto | 观察、添加上下文、通知 | 并行收集 |
| `TurnEnd` | AgentLoop | 最终回答完成后 | 无 | 观察、通知 | 并行、默认尽力而为 |
| `SessionEnd` | Session/应用协调层 | 会话关闭前 | end reason | 观察、清理、通知 | 并行、限时 |

第一期不开放任意 `PreModelRequest` 修改完整模型请求。修改完整 request 会破坏
system prompt、tool schema、历史顺序、token 统计和 Provider 假设。消息前的上下文
扩展统一通过 `UserPromptSubmit` 完成。

### 3.2 标准触发顺序

一次普通 turn 的顺序：

```text
TurnStart
  → UserPromptSubmit
  → 用户消息写入 ContextManager
  → Hook additional context 写入 ContextManager
  → 模型生成
  → PreToolUse
  → SecurityProvider
  → 必要时 Out::Ask / In::Reply
  → 工具执行
  → PostToolUse
  → 工具结果写入 ContextManager
  → 再次模型生成
  → TurnEnd
```

## 4. 挂载模型

### 4.1 注册项

无论 Hook 来自配置、Rust、Plugin 还是临时 session，最终都转换成统一注册项：

```rust
pub struct HookRegistration {
    pub id: String,
    pub point: HookPoint,
    pub matcher: HookMatcher,
    pub handler: Arc<dyn HookHandler>,
    pub order: i32,
    pub timeout: Duration,
    pub failure_policy: FailurePolicy,
    pub source: HookSource,
}
```

```rust
#[non_exhaustive]
pub enum HookSource {
    Managed,
    User,
    Project,
    Plugin,
    Session,
}

#[non_exhaustive]
pub enum FailurePolicy {
    /// Hook 失败、超时或输出非法时记录失败，但主操作继续。
    Open,
    /// Hook 失败、超时或输出非法时阻止主操作。
    Closed,
}
```

普通记忆、通知和观测 Hook 默认 `Open`。承担安全策略职责的 managed Hook 可以
显式配置 `Closed`。

### 4.2 Matcher

Matcher 是 Hook Point 专属语义，不使用一个含义不明的万能字符串：

```rust
#[non_exhaustive]
pub enum HookMatcher {
    Always,
    ToolName(String),
    SessionSource(String),
    CompactTrigger(String),
    SessionEndReason(String),
}
```

工具名 matcher 支持：

- 精确匹配：`get_order`
- 多选：`Edit|Write`
- 正则：`^mcp__memory__.*`
- 全部：`*`

`UserPromptSubmit`、`TurnStart`、`TurnEnd` 第一阶段只使用 `Always`。不要把用户
prompt 当正则匹配输入，复杂内容判断应由 Handler 自己完成。

### 4.3 挂载来源

```text
配置文件 ─┐
Rust API ─┼─► HookLoader / PluginLoader ─► HookRegistration ─► HookRuntime
Plugin ───┤
Session ──┘
```

`yourai-core` 和 `HookRuntime` 不读取配置文件。CLI/应用层负责读取配置、创建具体
Handler 并完成注册。

### 4.4 配置示例

建议兼容 Claude `hooks.json` 的分组形状：

```json
{
  "hooks": {
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "id": "recall-memory",
            "type": "http",
            "url": "https://memory.example.com/recall",
            "timeout": 3,
            "onError": "open"
          }
        ]
      }
    ],
    "PreToolUse": [
      {
        "matcher": "get_order",
        "hooks": [
          {
            "id": "approve-order-access",
            "type": "command",
            "command": "python3 hooks/check_order.py",
            "timeout": 3,
            "onError": "closed"
          }
        ]
      }
    ]
  }
}
```

配置兼容与 wire 协议兼容是两件事。第一期优先保证 wire 协议兼容；配置文件可以
由 CLI 同时支持 TOML 和 `hooks.json`。

## 5. 内部领域协议

### 5.1 Turn 元数据

Hook 输入需要当前 session、turn、cwd、模型和权限模式。当前 `TurnContext` 尚未携带
这些元数据，需要新增：

```rust
pub struct TurnMeta {
    pub turn_id: String,
    pub session_id: Option<SessionId>,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub permission_mode: String,
}
```

```rust
pub struct TurnContext<'a> {
    pub meta: TurnMeta,
    pub snap: ProviderSnapshot,
    pub inbox: &'a mut UnboundedReceiver<In>,
    pub outbox: &'a dyn OutSink,
    pub cancel: &'a CancellationToken,
}
```

### 5.2 Invocation 与事件

```rust
pub struct HookInvocation {
    pub invocation_id: String,
    pub context: HookContext,
    pub event: HookEvent,
}

pub struct HookContext {
    pub session_id: Option<SessionId>,
    pub turn_id: String,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub permission_mode: String,
}
```

```rust
#[non_exhaustive]
pub enum HookEvent {
    SessionStart {
        source: SessionStartSource,
    },
    TurnStart,
    UserPromptSubmit {
        prompt: String,
    },
    PreToolUse {
        call_id: String,
        tool_name: String,
        tool_input: Value,
    },
    PostToolUse {
        call_id: String,
        tool_name: String,
        tool_input: Value,
        tool_output: Value,
        is_error: bool,
    },
    PreCompact {
        trigger: CompactTrigger,
    },
    PostCompact {
        trigger: CompactTrigger,
        summary: String,
    },
    TurnEnd {
        final_text: String,
    },
    SessionEnd {
        reason: SessionEndReason,
    },
}
```

使用 enum 而不是 `HookEventType + Option<T>`，避免产生“事件是 `TurnEnd`，却同时
携带 `tool_input`”之类的非法状态。

### 5.3 事件专属 Outcome

```rust
#[non_exhaustive]
pub enum HookPointOutcome {
    SessionStart(SessionStartOutcome),
    UserPromptSubmit(UserPromptSubmitOutcome),
    PreToolUse(PreToolUseOutcome),
    PostToolUse(PostToolUseOutcome),
    PreCompact(PreCompactOutcome),
    PostCompact(PostCompactOutcome),
    Observe(ObserveOutcome),
}
```

```rust
#[non_exhaustive]
pub enum HookControl {
    Continue,
    Block { reason: String },
}

pub struct UserPromptSubmitOutcome {
    pub control: HookControl,
    pub additional_contexts: Vec<String>,
}

#[non_exhaustive]
pub enum HookPermission {
    Pass,
    Allow { reason: Option<String> },
    Ask { reason: String },
    Deny { reason: String },
}

pub struct PreToolUseOutcome {
    pub control: HookControl,
    pub permission: HookPermission,
    pub final_input: Value,
    pub additional_contexts: Vec<String>,
}

pub struct PostToolUseOutcome {
    pub control: HookControl,
    pub additional_contexts: Vec<String>,
}
```

Outcome 中不放 `stdout`、`stderr` 等执行细节；它们属于 `HookRun`。

### 5.4 执行记录

```rust
pub struct HookDispatchResult {
    pub outcome: HookPointOutcome,
    pub runs: Vec<HookRun>,
}

pub struct HookRun {
    pub hook_id: String,
    pub source: HookSource,
    pub status: HookRunStatus,
    pub started_at: i64,
    pub duration_ms: u64,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
}

#[non_exhaustive]
pub enum HookRunStatus {
    Completed,
    Failed,
    Blocked,
    Cancelled,
    TimedOut,
}
```

策略阻断不是运行错误：

```text
脚本崩溃、超时、非法 JSON → Failed / TimedOut
Handler 返回 Deny 或 Block  → Blocked
父 turn 取消                → Cancelled
```

## 6. 外部 Wire 协议

### 6.1 通用输入字段

Command Hook 从 stdin 读取一个 JSON；HTTP Hook 接收相同 JSON 的 POST body。

```json
{
  "session_id": "session-001",
  "turn_id": "turn-001",
  "cwd": "/project",
  "model": "gpt-5",
  "permission_mode": "default",
  "hook_event_name": "UserPromptSubmit"
}
```

事件专属字段追加在顶层。输入字段采用 `snake_case`，与 Claude/Codex Hook 生态保持
兼容。

### 6.2 通用输出字段

```json
{
  "continue": true,
  "stopReason": null,
  "suppressOutput": false,
  "systemMessage": null,
  "hookSpecificOutput": null
}
```

- `continue: false`：阻止当前操作继续；应同时提供非空 `stopReason`；
- `suppressOutput`：只影响 stdout/stderr 的用户展示，不影响结构化效果；
- `systemMessage`：给用户的警告/通知，不直接作为模型上下文；
- `hookSpecificOutput`：事件专属、模型可见或控制流相关的结果。

输出字段采用 `camelCase`，兼容 Claude Hook 协议。解析器必须拒绝未知字段、错误的
`hookEventName` 和事件不允许的效果。

### 6.3 UserPromptSubmit

输入：

```json
{
  "session_id": "session-001",
  "turn_id": "turn-001",
  "cwd": "/project",
  "model": "gpt-5",
  "permission_mode": "default",
  "hook_event_name": "UserPromptSubmit",
  "prompt": "帮我看看上次那个订单的问题"
}
```

输出：

```json
{
  "hookSpecificOutput": {
    "hookEventName": "UserPromptSubmit",
    "additionalContext": "相关记忆：上次讨论的是订单 ORD-10086。"
  }
}
```

第一期不允许 UserPromptSubmit Hook 替换原始 prompt；它只能阻断或贡献附加上下文。

### 6.4 PreToolUse

输入：

```json
{
  "session_id": "session-001",
  "turn_id": "turn-001",
  "cwd": "/project",
  "model": "gpt-5",
  "permission_mode": "default",
  "hook_event_name": "PreToolUse",
  "tool_use_id": "call-001",
  "tool_name": "get_order",
  "tool_input": {
    "order_id": "ORD-10086"
  }
}
```

输出：

```json
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "ask",
    "permissionDecisionReason": "是否允许读取订单 ORD-10086？",
    "updatedInput": {
      "order_id": "ORD-10086"
    },
    "additionalContext": null
  }
}
```

`permissionDecision` 可为 `allow`、`ask`、`deny`。省略表示 `pass`。

### 6.5 PostToolUse

输入：

```json
{
  "session_id": "session-001",
  "turn_id": "turn-001",
  "cwd": "/project",
  "model": "gpt-5",
  "permission_mode": "default",
  "hook_event_name": "PostToolUse",
  "tool_use_id": "call-001",
  "tool_name": "get_order",
  "tool_input": {
    "order_id": "ORD-10086"
  },
  "tool_response": {
    "status": "shipped"
  },
  "is_error": false
}
```

输出：

```json
{
  "hookSpecificOutput": {
    "hookEventName": "PostToolUse",
    "additionalContext": "订单读取操作已经写入审计日志。"
  }
}
```

第一期不允许任意替换工具输出。后续如果确有需求，应增加事件专属的
`updatedToolOutput`，并把 PostToolUse 调度改成串行 waterfall。

### 6.6 普通文本与退出码兼容

Command Hook 支持 Claude 风格兼容行为：

- exit `0` + 空 stdout：成功，无效果；
- exit `0` + 合法 JSON：解析结构化结果；
- exit `0` + 普通文本：仅在允许贡献上下文的 Hook Point 中将文本作为
  `additionalContext`；
- exit `2`：阻断，stderr 必须包含非空原因；
- 其他非零退出码：Handler 运行失败，按 `failure_policy` 处理；
- 超时或父 turn 取消：终止 handler，分别记为 `TimedOut` 或 `Cancelled`。

HTTP Hook 必须返回合法 JSON；不接受普通文本兼容路径。

## 7. Runtime 与 Handler

### 7.1 对外 Provider

当前 `HookRegistry` 同时表达注册和分发，但名称只强调“注册表”。建议将 Context 中的
Provider 收敛为 `HookRuntime`：

```rust
pub trait HookRuntime: Send + Sync {
    fn register(&self, registration: HookRegistration);
    fn unregister(&self, id: &str);
    fn handler_ids(&self) -> Vec<String>;

    fn dispatch<'a>(
        &'a self,
        invocation: HookInvocation,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<HookDispatchResult, YourAiError>>;
}
```

`HookRuntime` 内部可以包含 registry、matcher、executor、parser 和 aggregator；这些
不是新的 Context Provider。

### 7.2 Handler

```rust
pub trait HookHandler: Send + Sync {
    fn execute<'a>(
        &'a self,
        invocation: &'a HookInvocation,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, HookHandlerResult>;
}
```

具体后端：

```text
NativeHookHandler  → Rust 代码直接返回类型化结果
CommandHookHandler → JSON stdin / stdout + exit code
HttpHookHandler    → HTTP POST JSON / JSON response
未来：PromptHookHandler、AgentHookHandler
```

### 7.3 Runtime 执行流程

```text
1. 根据 Hook Point 取注册项快照
2. 根据事件专属 matcher 过滤
3. 按 source precedence、order、注册顺序稳定排序
4. 按 Hook Point 的 dispatch strategy 执行
5. 为每个 Handler 应用 timeout 和 cancellation
6. 校验并解析 Handler 输出
7. 按 failure_policy 归一化失败
8. 按 Hook Point 的 merge policy 聚合
9. 记录 HookRun 和 observability
10. 返回 HookDispatchResult
```

运行时热注册/卸载采用“事件级快照”：一个 dispatch 使用开始时的 Handler 列表跑完，
注册变化从下一个 Hook 事件生效。

## 8. 调度与聚合

### 8.1 UserPromptSubmit：并行收集

所有匹配 Handler 接收相同原始 prompt，并行执行：

```text
                 ┌─► 长期记忆 API ──────┐
UserPromptSubmit ├─► 项目知识库 API ────┼─► 按配置顺序聚合 context
                 └─► 用户偏好 API ──────┘
```

聚合规则：

1. 任意有效 `Block` 使整体阻断；阻断原因取配置顺序中的第一个；
2. `additionalContext` 按配置顺序追加，而不是按完成顺序；
3. Handler 失败按自己的 `failure_policy` 处理；
4. 一个 Handler 失败不取消其他已经开始的 Handler。

### 8.2 PreToolUse：串行 waterfall

PreToolUse 允许修改输入，因此必须让下一个 Handler 看到前一个 Handler 的输出：

```text
原始 input
  → Hook A updatedInput
  → Hook B 审核更新后的 input
  → ToolRegistry 重新生成 SecurityContext
  → SecurityProvider 检查最终 input
```

聚合规则：

1. `updatedInput` 链式传递；
2. `Deny` 优先级最高；
3. `Ask` 次之；
4. `Allow` 不能覆盖其他 Hook 或 `SecurityProvider` 的 `Ask/Deny`；
5. 有效 `Deny` 可以立即短路后续普通 Handler；managed 审计是否仍需执行由注册策略决定。

最终权限合并：

```text
任意 Hook Deny 或 Security Deny → Deny
否则任意 Hook Ask 或 Security Ask → Ask
否则                               → Allow
```

### 8.3 PostToolUse：并行收集

第一期只允许贡献上下文和通知，因此可以并行执行并按配置顺序聚合。

### 8.4 生命周期 Hook：并行、尽力而为

`TurnStart`、`TurnEnd`、`SessionEnd` 默认不改变主业务数据。它们并行执行，默认
fail-open；`SessionEnd` 使用较短的整体 deadline，避免应用无法退出。

## 9. 与 DefaultLoop 的集成

### 9.1 记忆召回

```text
In::UserText
  → DefaultLoop 触发 UserPromptSubmit
  → HookRuntime 调记忆 Handler
  → 返回 additional_contexts
  → DefaultLoop 写用户消息
  → DefaultLoop 写 HookContext conversation item
  → ContextManager::build_request()
  → ModelProvider
```

建议给 `ContextManager` 增加明确接口，避免 DefaultLoop 用普通 system/user message
伪装 Hook 数据：

```rust
fn add_hook_context<'a>(
    &'a self,
    source: &'a str,
    content: &'a str,
) -> BoxFuture<'a, Result<(), YourAiError>>;
```

写入的模型可见内容应带明确来源和数据边界，例如：

```text
<hook-context source="UserPromptSubmit" trust="external">
相关记忆：上次讨论的是订单 ORD-10086。
</hook-context>
```

外部记忆默认视为不可信数据，不能无边界地冒充 system instruction。

### 9.2 工具确认

```text
模型产生 get_order call
  → DefaultLoop 触发 PreToolUse
  → Hook 返回 Ask
  → SecurityProvider 检查最终 input
  → 合并结果仍为 Ask
  → DefaultLoop 发 Out::Ask
  → DefaultLoop 独占等待匹配 In::Reply
  → 用户允许：ToolRegistry::execute
  → 用户拒绝：错误 ToolResponse 喂回模型
```

`Out::Ask` 示例：

```json
{
  "id": "approval:call-001",
  "payload": {
    "kind": "tool_approval",
    "call_id": "call-001",
    "tool": "get_order",
    "input": {
      "order_id": "ORD-10086"
    },
    "reason": "是否允许读取订单 ORD-10086？"
  }
}
```

前端回复：

```json
{
  "id": "approval:call-001",
  "payload": {
    "decision": "allow"
  }
}
```

HookRuntime 不发送 Ask，也不等待 Reply；这一过程完全属于 DefaultLoop。

## 10. 安全与可靠性

### 10.1 Trust

- 项目和第三方 Plugin 的 Command/HTTP Hook 只有 workspace trust 后才能执行；
- managed Hook 可以由管理员策略预置信任；
- Handler 内容变化后信任 hash 失效，需要重新确认；
- 支持 managed-only 模式，忽略 user/project/session Hook。

### 10.2 Command Hook

- stdin/stdout/stderr 全部 pipe；
- 使用明确 cwd；
- 跟随 turn cancellation；
- 超时后杀掉整个进程组，而不只是直接子进程；
- 限制 stdout/stderr 字节数；
- stdout 只承载结果，普通日志写 stderr；
- 不默认把进程全部环境变量暴露给不可信 Hook。

### 10.3 HTTP Hook

- URL allowlist；
- DNS/IP SSRF 防护；
- 禁止或严格验证 redirect；
- Header 环境变量必须显式 allowlist；
- 清理 CR/LF/NUL，防止 Header 注入；
- secret 不进入 Hook input、日志和 `HookRun`；
- 设置请求体、响应体和超时上限。

### 10.4 输出限制

- 每个模型可见 Hook 输出设置字符或 token 上限；
- 超限内容截断或落临时文件；
- 模型只看到受限预览和恢复路径；
- HookRun 日志也必须限制 stdout/stderr，避免内存和磁盘无限增长。

### 10.5 可观测性

每次运行至少记录：

```text
hook_id / hook_point / source / status
started_at / duration_ms
timeout / failure_policy
是否贡献 context / 是否修改 input / permission decision
```

默认不记录完整 prompt、tool input、tool output 和 secret。详细载荷日志必须显式开启。

## 11. Crate 划分

```text
yourai-core
  └─ HookPoint / HookEvent / HookInvocation / HookOutcome
     HookHandler trait / HookRuntime trait

yourai-hooks-inmemory
  └─ 默认 registry、matcher、排序、调度、聚合、HookRun 记录

yourai-hooks-command
  └─ Command Handler、JSON stdin/stdout、timeout、cancel、wire parser

yourai-hooks-http
  └─ HTTP Handler、allowlist、secret header、SSRF、wire parser

yourai-loop-default
  └─ 在标准 Hook Point 触发 dispatch
     应用 context、permission、block 等结果
     通过 Out::Ask / In::Reply 完成用户确认

yourai-cli
  └─ 读取 TOML/hooks.json、构造 Handler、注册、trust 管理
```

## 12. 对当前接口的调整

当前 `yourai-core/src/hooks.rs` 有以下问题：

1. `HookEventType + HookEvent` 的多个 `Option` 能表达非法状态；
2. `HookOutcome::Modify { new_input: Value }` 无法表达记忆上下文、权限 Ask/Deny、
   事件专属输出和多 Handler 聚合；
3. `dispatch(event)` 缺少 turn/session/cwd/model 元数据；
4. `dispatch(event)` 缺少 cancellation；
5. Registry、执行状态与最终业务 outcome 没有分离；
6. 所有事件固定串行，无法让多个上下文召回并行执行。

落地时应：

1. 用 `HookEvent` enum 代替 optional-field event；
2. 用事件专属 `HookPointOutcome` 代替万能 `Modify(Value)`；
3. 将 Context Provider 从 `HookRegistry` 收敛为 `HookRuntime`；
4. 为 `TurnContext` 增加 `TurnMeta`；
5. 给 dispatch 传入 cancellation；
6. 明确每个 Hook Point 的调度、合并与失败规则；
7. 先实现 Native + Command，再实现 HTTP；
8. 最后在 `DefaultLoop` 的标准位置接入 Hook。

## 13. 第一阶段验收标准

Hook 第一期完成时至少应通过以下端到端场景：

1. `UserPromptSubmit` Command Hook 返回普通文本并注入模型上下文；
2. `UserPromptSubmit` HTTP Hook 返回结构化 `additionalContext`；
3. 多个记忆 Hook 并行执行，但上下文按配置顺序稳定；
4. 记忆 Hook 超时且 `Open` 时，用户消息仍能继续；
5. `PreToolUse(get_order)` 返回 `Ask`，DefaultLoop 发出 `Out::Ask`；
6. 用户回复 allow 后工具执行，回复 deny 后工具不执行；
7. Hook `Allow` 不能覆盖 `SecurityProvider::Deny/Ask`；
8. PreToolUse 的 `updatedInput` 按 waterfall 传给后续 Hook 和 SecurityProvider；
9. `Closed` Hook 崩溃或超时时阻止对应操作；
10. drop `TurnHandle` 或 interrupt 时正在执行的 Hook 被取消；
11. 非法 JSON、错误 `hookEventName` 和未知字段不能产生业务效果；
12. 模型可见 Hook context 进入会话记录并参与恢复和压缩；
13. 未信任 workspace 的项目 Command/HTTP Hook 不执行；
14. Hook 输出超限时被安全截断或落盘；
15. HookRun 能被 ObservabilityProvider 记录但默认不泄露敏感载荷。
