# DefaultLoop 实现

> 会话存储与统一提交路径已经落地，当前行为以 [会话存储设计](./session-storage-design.md) 和源码为准。

`yourai_harness::DefaultLoop` 只实现默认调度。流程图 2、3 的执行契约现归独立 `execution` 模块，默认与自定义 Loop 共用。自定义接口和迁移说明见 [公共执行层](./execution.md)。实现依赖 `yourai-core`，core 不反向依赖 Loop。图 1、4 及具体 Provider 也统一位于 `yourai-harness`，见 [Runtime 实现与验收](./runtime-implementation.md)。

## 入口和模块

```text
Agent.start / run
    -> DefaultLoop.run_turn
       -> TurnExecution 公共操作
       -> 首条输入 + UserPromptSubmit
       -> checkpoint：取消、steer、延迟上下文
       -> compact（需要时）
       -> 模型请求与事件流
       -> 完整 assistant 历史
       -> 串行工具批次 / Stop
       -> 下一轮或统一收尾
    -> TurnOutput / TurnFailure
    -> Core 关闭 inbox，回收残留输入
```

| 文件 | 职责 |
|---|---|
| `default_loop/mod.rs` | DefaultLoop：调用公共模型和工具入口，提交候选完成 |
| `execution/mod.rs` | TurnExecution、共享操作状态、业务能力与完成操作 |
| `execution/config.rs` | ExecutionConfig 业务操作策略；default_loop/config.rs 保留原 LoopConfig 字段和 steps |
| `execution/control.rs` | 可取消等待、唯一 inbox 消费、历史、用量与清理 |
| `execution/admission.rs` | 输入 Hook、附件准备、拒绝与提交 |
| `execution/model.rs` | 请求装配、工具绑定、流式事件、完整消息与工具 ID 校验 |
| `execution/tools.rs` | 工具 Hook、校验、审批、执行与结果提交 |
| `execution/interaction.rs` | 工具提问、回复路由、MCP Hook 与回复校验 |
| `execution/hooks.rs` | 私有 Hook 调用和结果应用 |

公共 ToolExecutor/ModelExecutor/ContextExecutor/InputExecutor/PermissionExecutor/InteractionExecutor 隐藏内部状态和 Hook 协议。DefaultLoop 不再直接 dispatch Hook。

## 装配

```rust
use std::sync::Arc;
use yourai_core::prelude::*;
use yourai_harness::default_loop::{DefaultLoop, LoopConfig};

fn assemble(model: Arc<dyn ModelProvider>, history: Arc<dyn ContextManager>) -> Arc<Agent> {
    let config = LoopConfig {
        steps: Some(20),
        ..Default::default()
    };
    Agent::builder()
        .agent_loop(Arc::new(DefaultLoop::new(config)))
        .model(model)
        .context_manager(history)
        .build()
}
```

必需 ModelProvider、ContextManager。工具、安全、Hook、记忆、技能、用量及可观测性均按需装配。默认不检索记忆、不自动激活所有技能；配置 `memory_search_limit` 和 `skill_ids` 后才使用相应能力。

模型通过 `ModelProvider::stream_events` 获取事件流。测试和其他适配器可以直接构造事件流。`ModelProvider::recovery` 默认映射 provider 的 `classify_error`；未分类错误默认不可恢复。GenaiModel 自行分类结构化 HTTP 错误，其他适配器可独立实现分类和恢复策略，不能靠字符串猜测后无限重试。

## 确定的执行语义

- **steer**：在下一主循环检查点接纳，不强行中止正在进行的模型请求或工具批次。仍经过 UserPromptSubmit。Hook 拒绝的输入发 Notice，不写入历史。
- **follow-up**：始终留在 pending，由宿主驱动下一 Turn。运行中消费过的未处理输入在成功或失败时都交还。
- **工具**：模型请求前绑定 handler；模型看到的 schema、审批描述和执行来自同一实例。Hook 更新参数后进行 JSON Schema 校验；远程 schema 引用解析未启用。
- **审批**：Security Deny、PreToolUse Deny 均不能被 Allow 覆盖。PermissionRequest Hook 可给出最终参数范围；修改后重新校验 schema 和 Security。用户回复只接受 `{"behavior":"allow"}` 或 `{"behavior":"deny"}`，不允许借回复改写工具输入。
- **权限变更**：Hook 的 updated_permissions 调用 SecurityProvider::update_permissions；PolicySecurity 原子持久化会话级精确工具规则，不支持的变更报错。未包含权限变更的 Allow 仅授权当前调用。PermissionDenied.retry 受次数限制，每次重新检查权限。
- **工具结果**：普通失败形成 tool result；取消、断开、总截止时间、次数限制、Hook 全局停止终止 Turn。工具单次超时是普通工具失败。
- **历史顺序**：先提交完整 assistant（含 tool calls、reasoning、签名），再提交每个 tool result。工具 Hook 附加上下文延迟到整批工具结果之后写入。
- **流式结果**：Chunk/Reasoning 是增量；Message 是这一条 assistant 的完整文本；TurnOutput.text 是最后一条 assistant 或失败时的部分文本。
- **用量**：Out::Usage 是每次调用的增量；TurnOutput.usage 是当前 Turn 已知用量之和，包括 compact 返回的用量。
- **重试**：仅在尚无可见正文/推理输出且未处理 End 时，按 provider 分类有限重试。截断、过滤或没有终止事件的流不能执行不完整工具。重复 call_id（包括当前上下文里已有 ID）会被拒绝。
- **Stop**：blocking feedback 写入上下文并继续，但受独立次数上限约束；prevent_continuation 表示直接停止。StopFailure 仅用于最终模型故障，不能覆盖原始错误。

## 超时、预算和收尾

agentic iteration 默认不限（steps=None）；达到第 N 步时要求模型仅文本总结、不再提供工具。steps=0 是配置错误。默认 5 次请求重试、1 次连续溢出恢复、3 次 Stop 继续、1 次权限重审。重试时间由 `model/retry.rs` 集中管理：初始 2 秒逐次倍增，附加至多 25% 抖动；无响应头时默认封顶 30 秒，有响应头时使用全局安全上限（`i32::MAX` 毫秒）。provider 返回的重试提示优先且不追加抖动；GenaiModel 从 Retry-After 解析提示，MeteredModel 与剩余共享冷却取较长者。

总截止时间不因重试、审批或压缩重置。模型默认响应头等待与原始数据读取时限各 300 秒，不含整段响应总时长；心跳和未完整事件的原始数据会重置读取等待。GenaiModel 在传输层执行这两个时限，Loop 不重复添加事件空闲计时。普通操作、工具、审批、Hook 和收尾时限默认 None，可显式配置；工具单次时限覆盖执行及内部提问。

模型请求前按完整请求预算触发 compact；provider 明确报告 overflow 时有界重试。ContextManager 内部清理/摘要和提交，Loop 只重建请求。分批摘要按实际尝试调用计数，包括失败和取消；只清理不消耗模型额度。ContextManager 记账，Loop 汇总 Turn 用量。Harness 的共享 MeteredModel 另统一限制主模型、摘要、Hook 模型及子 Agent。

取消时，共享执行包装请求工具子 token 取消，给有界宽限保存真实结果，再丢弃未完成 future。Provider 必须 cancellation-safe，用 RAII 或自身有界清理释放进程和任务。执行层随后以独立时限记录部分 assistant、已完成工具的实际结果以及剩余调用的 `interrupted_or_not_executed` 结果。它不能强杀实现方私自脱离的任务。

ContextManager 的工具结果提交必须按 call_id 幂等，以处理提交时中断后的收尾重试。持久化失败或清理超时会输出明确错误 Notice，并记录指标；宿主不能自动重放副作用。进程崩溃后的事务恢复仍属于宿主和存储实现。

OutSink 新增默认 `closed()` 等待接口；core 的 channel sink 实现关闭通知，因此前端关闭后，即使模型没有新输出，也能结束等待。自定义 sink 若无关闭通知，仍通过 send(false) 报告断开。

## 工具交互

工具调用 `ToolContext.ask`，内部桥将请求交给公共执行层。公共工具执行入口等待工具时同时服务交互通道，使用独立 request_id 关联 In::Reply。过期/错误 ID 的回复不保存、不转给后续提问；工具取消等待时关闭 oneshot，执行层注销当前请求。

MCP 请求依次经过 Elicitation、用户回复（或 Hook 答复）、ElicitationResult。最终回复只允许 accept/decline/cancel；accept 的 content 按 requested_schema 校验。普通提问不触发 MCP Hook。非交互 Agent.run 遇 Ask 立即取消并返回 Config。

## Hook 职责划分

公共 execution 模块接入 10 个操作 Hook，DefaultLoop 和自定义 AgentLoop 无需触发或消费：UserPromptSubmit、Stop、StopFailure、PreToolUse、PostToolUse、PostToolUseFailure、PermissionRequest、PermissionDenied、Elicitation、ElicitationResult。

SessionStart/SessionEnd、工作区、配置、指令文件、子 Agent、协作任务等事件仍归会话宿主或对应扩展。ConcreteHookRuntime 自主管理后台 Hook；宿主订阅其完成事件与唤醒策略不属于 DefaultLoop。Loop 的内部 Notice 不递归触发 Notification。

`yourai-harness` 还提供 SessionHost、GenaiModel、SQLite 持久化、模型摘要、宿主后台事件订阅和扩展。它们与 DefaultLoop 在模块层分离，完整装配入口是 `Harness::open`。

## 验证

`crates/yourai-harness/tests/loop_flow.rs` 使用可控制事件流和内存历史验证完整主链，覆盖审批、Hook 参数修改、硬拒绝、JSON Schema、普通/MCP 交互、压缩、有限重试、Stop、steer/follow-up、取消、断开、超时、预算以及批次收尾；包含真实 ConcreteHookRuntime 注册与效果消费测试。不依赖在线模型或 API Key。


## 工具输出存储与重复调用保护

Harness 装配为同一 SessionCatalog 的会话共享不可变的 ToolOutputStore，目录为
`<root>/tool-output`。每次落盘使用独立 UUID 文件并排他创建，不依赖模型 call_id；
不同 Harness root 不共享可变配置。后台每小时清理超过七天的受管理普通文件。

DefaultLoop 在 PostToolUse（含 MCP 输出改写）之后统一处理超长结果：默认 2000 行、
50 KiB，完整 JSON 落盘，头尾预览带 `output_paths`。MemoryContext 仍独立执行请求的
`tool_output_chars` 限额，但缩减及 prune 都保留文件路径和状态，不裁掉回看入口。
过期文件不再保证可读；无需额外的 read_tool_result 工具。

已装配的 Shell 为 stdout/stderr 分别流式保存完整字节，每路仅保留固定开头和滚动结尾，返回有界头尾预览。
输出量本身不终止命令，超时、取消和进程组清理仍生效。`capture_complete` 表示两路采集到 EOF，
`output_complete` 表示返回文本也未截断；完整内容分别由 stdout_path/stderr_path 指向。
直接创建 Shell::new（未提供存储）保留原 8 MiB 采集上限，可用 Shell::with_output 显式启用存储。
Read 保留行分隔符，裁剪超长行，并按 50 KiB 分页；超过 16 MiB 的文件仍使用 shell 定向检索。

连续三个同名同参数工具调用会要求审批；后续相同调用仍需逐次批准。复用现有 PermissionRequest、
参数改写后的 schema/硬策略复查、PermissionDenied 与 TUI y/n 协议；YOLO 使用已有跳过审批语义。
用户新输入、assistant 非空文本及不同工具/参数会打断连续计数。计数在本 Turn 内，
这是适配当前逐步 assistant 消息结构的策略，不声称与 OpenCode 的消息 part 扫描完全相同。
默认 steps=None 仍无限制，默认模型重试仍为五次。

参考 OpenCode `2fa3363c924c5c3e367b84a87ae478296a0ed59b`：
`packages/core/src/tool-output-store.ts` 的唯一文件/头尾预览/清理，
`packages/opencode/src/tool/shell.ts` 的流式落盘，及 session/processor.ts 的权限门控。
