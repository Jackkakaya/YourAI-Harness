# Core 组件接口契约


运行时补充接口：`TurnOptions.events` 绑定内部事件队列；ContextManager 内部协调持久化并维护归档身份索引；`SecurityProvider::update_permissions` 消费权限变更；`HookRuntime::subscribe_background / shutdown_session` 提供按会话订阅和后台回收。`TurnHandle::abort` 用于协作取消超时后的任务回收，宿主必须记录被强制中止执行的不确定状态。

对应 [完整流程图](./default-loop-flow.md)。本文定义公共边界和运输机制。DefaultLoop、会话宿主、compact 摘要和业务扩展现已统一实现在 `yourai-harness`，见 [实现文档](./default-loop-implementation.md) 与 [Runtime 文档](./runtime-implementation.md)。下文出现的 `yourai-loop` / `yourai-runtime` 是合并前的历史包名。

## 图与代码的对应

| 流程 | 已定义的接口/类型 | 实现边界 |
|---|---|---|
| 图 1：会话宿主 | `SessionRuntime`、`SessionContext`、`SessionStatus`、`SessionTurn`、`InputRejected` | trait 在 core，具体宿主在外部实现；不是 Context 的新 Provider 插槽 |
| 图 2：主循环 | `AgentLoop`、`TurnResult`、`TurnFailure`、`TurnInfo`、`TurnOptions`、`TurnLimits` | core 传递环境与结果；业务状态机、计数、超时和重试策略由 Loop 实现 |
| 图 2：compact | `CompactionRequest`、`CompactionResult`、`CompactionTrigger`；更新 `ContextManager::compact` | ContextManager 内部触发摘要 Hook、计算并提交变更，返回执行状态 |
| 图 3：工具 | `ToolRegistry::resolve`；`ToolInteraction`、`InteractionRequest`、`InteractionKind`；`ToolContext::ask` | ToolExecutor 为 Loop 内部模块，不新增 trait；交互桥已在 yourai-loop 实现 |
| 图 4：可选扩展 | 复用 ToolHandler、HookRuntime、SessionRuntime | 不预先增加工作区、多 Agent、协作任务的公共 Provider |
| 图 5：Hook | 复用现有 HookRuntime / HookRegistry / HookHandler | 此轮不修改 Hook wire 协议，不自动触发业务 Hook |

## 1. 会话宿主

`SessionRuntime` 定义对象安全的会话控制接口。创建/恢复由具体实现的构造入口负责，完成历史绑定、Agent 装配及 SessionStart 后才暴露实例。

```text
submit(In)
    +-- Idle：接纳用户输入到宿主队列
    +-- Running：按 InputMode 投递当前输入；Reply 仅对应当前交互
    +-- 拒绝：InputRejected 归还原始输入

run_next(limits, outbox, cancel)
    +-- 无待执行输入：Ok(None)
    +-- 忙 / 关闭 / 启动失败：Err，保留未开始的输入
    +-- 已执行：Ok(Some(SessionTurn { turn_id, result }))
            +-- result = Ok(TurnOutput)
            +-- result = Err(TurnFailure)
```

外层驱动再次调用 run_next 处理后续任务，每个调用的 OutSink 只对应一个 Turn。宿主持有 TurnHandle、转发事件、保证同会话执行互斥。它在返回前将成功或失败报告里的 pending **移动**回宿主队列，报告的 pending 清空；失效 Reply 不得启动下一 Turn。

这不是“一个 submit 就自动启动后台常驻 actor”的接口。宿主可以提供更高层自动驱动入口，但 core 不要求特定调度架构。

其他操作：

- `context()` / `status()`：会话环境和展示用状态；不能先读状态再假设操作不会竞争。
- `interrupt()`：请求取消当前 Turn，不清空后续输入，也不代表清理已经完成。
- `compact(request, cancel)`：手动压缩，与 Turn 写历史互斥；调用方负责前后 Hook。
- `close(timeout)`：拒绝新输入、清理当前执行、SessionEnd、释放资源，成功时归还剩余输入。

`SessionManager` 继续负责元数据；`ContextManager` 负责历史；SessionRuntime 不通过监听 Out 重复保存历史。状态机、互斥、后台事件与恢复策略是未来具体宿主的实现责任。

## 2. Turn 身份、环境和限制

```rust
let mut options = TurnOptions::default();
options.session = Some(Arc::new(SessionContext::new(session_id, cwd)));
options.limits.steps = Some(32);
options.limits.deadline = Some(Instant::now() + Duration::from_secs(120));

let handle = agent.start_with(In::user_text("任务"), options)?;
// handle.info.id 与 Loop 中 tc.info.id 对应同一次执行。
```

- `Agent::start_with` / `run_with` 接收 TurnOptions；原 `start` / `run` 使用默认参数。
- `TurnInfo` 包含 TurnId、开始时间及本次选项；通过 TurnContext.info 传给 Loop。
- 有 SessionContext 且装配了 ContextManager 时，Core 校验其 session_id 一致，避免写入其他会话的历史。
- 无会话的独立 Loop 仍可运行；会话宿主应始终传递会话绑定。
- TurnLimits 定义模型/工具调用次数、绝对截止时间、模型/工具/审批/Hook 单次超时。None 表示未指定，次数为 0 表示不允许调用。
- `tc.check_control()` 检查取消和总截止时间。Loop 必须在异步等待时监听取消/超时，并在调用边界维护计数；core 不自动强杀不合作的 Provider。
- `AbortReason::DeadlineExceeded` / `LimitReached(TurnLimit)` 表达可识别的结束原因。单次工具超时与总截止时间的业务处理由调用阶段区分。

此处的限制不等于已经实现默认预算管理器。压缩、模型 Hook、子 Agent 的统一用量预算仍需调用方接入。

## 3. 成功与失败都能交还状态

```rust
pub type TurnResult = Result<TurnOutput, TurnFailure>;

pub struct TurnFailure {
    pub error: Box<YourAiError>,
    pub output: TurnOutput, // 部分文本、usage、pending
}
```

AgentLoop::run_turn、Agent::run/run_with、TurnHandle::join 使用 TurnResult。

```rust
// Loop 已有部分输出或暂存输入时，显式带回它们。
return Err(TurnFailure::new(AbortReason::Cancelled, output));

// 尚无本地累计状态时，可通过 From 转换使用 ? 或 .into()。
tc.check_control()?;
```

Core 在 Loop 返回后，无论 Ok/Err，都先关闭 inbox，再把未接收消息追加到输出的 pending。Loop 已经取走但尚未处理的消息仍需自己放进 pending。次序为 Loop 暂存输入在前，通道残留在后。

- 非交互 run 遇 Ask 仍返回 Config，同时保留 Loop 已返回的部分结果和通道残留。
- 非交互 run 在装配校验失败时也归还首条输入。
- start/start_with 在启动前失败仍返回 YourAiError；调用方应在提交前保留首条输入，供装配失败时恢复。
- panic 或强制终止导致 Loop 未返回时，不能恢复其局部文本、用量和已消费输入；join 返回故障，不承诺虚构部分结果。
- Drop/interrupt 发出协作取消信号，不等同于已经完成资源清理。

## 4. ContextManager

```text
restore()                         恢复已提交视图
append(Vec<StoredMessage>)         保存消息，成功后更新内存
build_request(system, tools, execution)      返回 ChatRequest、估算、可用输入预算及维护标志
compact(CompactionRequest, execution, cancel) 返回已完成的 CompactionResult
```

ContextExecution 由当前执行快照生成，携带 model/hooks/usage/Hook 基础信息；ContextManager 不持有第二份运行时依赖。ModelProvider 统一使用 complete / stream_events，UsageTracker 统一使用 record_event。

保留 session_id、records/messages、归档身份等只读查询；无公开 reset/prune/候选提交接口。system 只能经 build_request 传入。

CompactionRequest 包含 Threshold/Overflow/Manual、请求环境、摘要要求、截止时间、剩余调用额度。共享调用计数和已知 usage 在失败/取消后仍可结算。CompactionResult.action 为 Unchanged / Pruned / Summarized，包含前后估算、原因与提交后 Hook 状态，不返回待提交候选。

```text
Loop / Host → ContextManager.compact
                → 清理
                → 必要时 PreCompact → 模型摘要
                → SessionManager.save_context（事务）→ 更新内存
                → PostCompact
            ← 已提交结果
```

ContextManager 内部记录 UsageTracker；Loop 只汇总 Turn 用量，不重复写账。PostCompact 的失败/停止不回滚已提交摘要。完整流程见 [ContextManager 设计](./context-manager-design.md)。

## 5. 工具绑定与执行期交互

`ToolRegistry::resolve(name)` 返回 Arc<dyn ToolHandler>。一次调用解析一次，Loop 用同一个 handler 描述权限并执行。现有 security_context/execute 保留为便捷方法，默认委托 resolve；分开调用便捷方法不保证跨调用目标相同。

ToolContext 新增可选的 `interaction: Option<&dyn ToolInteraction>`。

```rust
let answer = tc.ask(
    InteractionKind::Question,
    serde_json::json!({ "question": "选择输出格式" }),
).await?;
```

ask 自动生成独立 request_id 并关联 call_id，缺少交互实现立即报 Config，等待时响应取消。InteractionKind::McpElicitation 携带服务端和 elicitation 身份，完整表单等仍由 payload 表达。

```text
ToolContext.ask
    -> ToolInteraction.request
    -> Loop 的内部请求通道（yourai-loop）
    -> 对 MCP 调用 Elicitation Hook
    -> Out::Ask / In::Reply
    -> 对 MCP 调用 ElicitationResult Hook
    -> 校验并经专属回复通道返回工具
```

只有 Loop 读取 inbox。交互桥处理时间限制、非交互运行、请求注销与迟到回复；Loop 等待工具时同时服务桥接通道。该流程已在 yourai-loop 实现。

第二层 Security 检查仍仅 Allow/Deny；MCP 或普通业务提问不能变成绕过硬性权限的另一个审批入口。

## 6. 输入模式与迁移

In 仍是两种变体；UserText 增加 InputMode：

```rust
In::user_text("当前追加指令") // mode = Steer
In::follow_up("稍后执行")    // mode = FollowUp
```

作为首条消息时，两种模式均启动本次输入；区别只作用于运行中追加的消息。旧 JSON 缺少 mode 时默认 Steer，新的 mode 值使用 snake_case。

本轮包含 Rust API 变更：

1. Loop 实现改为返回 TurnResult；构造 YourAiError 后用 `.into()` 转为 TurnFailure，或显式提供部分输出。
2. 错误消费者通过 `failure.error` 检查原因，通过 `failure.output` 读取部分结果。
3. UserText 模式匹配使用 `{ text, .. }` 或显式处理 mode；构造优先使用 helper。
4. ContextManager 实现新增 session_id，更新 compact 签名与返回值。
5. ToolRegistry 实现新增 resolve；手工构造 ToolContext 需提供 interaction。
6. 手工构造 TurnContext 需传入 info；正常使用 Agent 启动时由 core 完成装配。

core 没有新增 DefaultLoop、ToolExecutor、工作区或协作任务 Provider。yourai-loop 已接入这些契约；SessionHost 已实现具体会话宿主。

## DefaultLoop 接入时的补充

- ModelProvider::stream_events 默认桥接原 stream；ModelRecovery 和 recovery() 提供保守的恢复分类。
- OutSink::closed 默认保持 pending，core 通道实现提供真实关闭信号。
- AbortReason::HookStopped 表示 Hook 通用停止请求。
- ContextManager/SessionManager 工具结果提交按 call_id 防重复；ToolHandler 执行 future 须 cancellation-safe。
