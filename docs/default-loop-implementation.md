# DefaultLoop 实现

> 当前实现的历史写入接口将按 [会话存储设计](./session-storage-design.md) 迁移为统一提交路径。该设计尚未落地，本文仍描述迁移前行为。

`yourai-loop::DefaultLoop` 对应流程图 2、3，复用图 5 的 HookRuntime。实现依赖 `yourai-core`，core 不反向依赖 Loop。图 1、4 及具体 Provider 已在 yourai-runtime 实现，见 [Runtime 实现与验收](./runtime-implementation.md)。

## 入口和模块

```text
Agent.start / run
    -> DefaultLoop.run_turn
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
| `yourai-loop/src/lib.rs` | DefaultLoop、LoopConfig、TurnState、主流程、compact、技能与记忆准备 |
| `control.rs` | 可取消等待、超时、输入队列、历史、用量与失败收尾 |
| `model.rs` | 请求装配、工具绑定、流式事件、完整消息、截断与工具 ID 校验 |
| `tools.rs` | 参数校验、Hook、安全审批、串行执行、结果提交 |
| `interaction.rs` | 工具请求通道、oneshot 回复、Ask/Reply 路由、MCP 回复校验 |
| `hooks.rs` | Hook 调用、通用效果、展示与可观测性 |

这些是内部模块，没有再增加 ToolExecutor、队列或调度器 Provider。

## 装配

```rust
use std::sync::Arc;
use yourai_core::prelude::*;
use yourai_loop::{DefaultLoop, LoopConfig};

fn assemble(model: Arc<dyn ModelProvider>, history: Arc<dyn ContextManager>) -> Arc<Agent> {
    let config = LoopConfig {
        system_prompt: Some("你是一个编程助手。".into()),
        compact_threshold: Some(80_000), // 按所选模型容量设置；默认不启用阈值压缩
        compact_target: Some(40_000),
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

模型默认走新增的 `ModelProvider::stream_events`，其默认实现桥接已有 `stream`，原适配器无需改动。测试和其他适配器可以直接构造事件流。`ModelProvider::recovery` 默认保守分类结构化 HTTP 错误；适配器可覆盖，不能靠字符串猜测后无限重试。

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

默认最多 64 次模型调用额度、256 次工具执行、2 次请求重试、1 次连续溢出恢复、3 次 Stop 继续、1 次权限重审。配置可以调整；TurnLimits 的次数与配置取较小值。

总截止时间不因重试、审批或压缩重置。模型单次时限覆盖建立流与整个读取过程；工具单次时限覆盖执行及内部提问。默认普通操作 120 秒、审批 300 秒、Hook 30 秒、一次收尾等待 5 秒，可配置。

模型请求前按完整请求预算触发 compact；provider 明确报告 overflow 时有界重试。ContextManager 内部清理/摘要和提交，Loop 只重建请求。分批摘要按实际尝试调用计数，包括失败和取消；只清理不消耗模型额度。ContextManager 记账，Loop 汇总 Turn 用量。Harness 的共享 MeteredModel 另统一限制主模型、摘要、Hook 模型及子 Agent。

取消时，Loop 请求工具子 token 取消、丢弃操作 future。Provider 必须 cancellation-safe，用 RAII 或自身有界清理释放进程和任务。Loop 随后以独立时限记录部分 assistant、已完成工具的实际结果以及剩余调用的 `interrupted_or_not_executed` 结果。它不能强杀实现方私自脱离的任务。

ContextManager 的工具结果提交必须按 call_id 幂等，以处理提交时中断后的收尾重试。持久化失败或清理超时会输出明确错误 Notice，并记录指标；宿主不能自动重放副作用。进程崩溃后的事务恢复仍属于宿主和存储实现。

OutSink 新增默认 `closed()` 等待接口；core 的 channel sink 实现关闭通知，因此前端关闭后，即使模型没有新输出，也能结束等待。自定义 sink 若无关闭通知，仍通过 send(false) 报告断开。

## 工具交互

工具调用 `ToolContext.ask`，内部桥将请求交给 Loop。Loop 等待工具时同时服务交互通道，使用独立 request_id 关联 In::Reply。过期/错误 ID 的回复不保存、不转给后续提问；工具取消等待时关闭 oneshot，Loop 注销当前请求。

MCP 请求依次经过 Elicitation、用户回复（或 Hook 答复）、ElicitationResult。最终回复只允许 accept/decline/cancel；accept 的 content 按 requested_schema 校验。普通提问不触发 MCP Hook。非交互 Agent.run 遇 Ask 立即取消并返回 Config。

## Hook 职责划分

本 crate 接入 10 个 Loop 所属 Hook：UserPromptSubmit、Stop、StopFailure、PreToolUse、PostToolUse、PostToolUseFailure、PermissionRequest、PermissionDenied、Elicitation、ElicitationResult。

SessionStart/SessionEnd、工作区、配置、指令文件、子 Agent、协作任务等事件仍归会话宿主或对应扩展。ConcreteHookRuntime 自主管理后台 Hook；宿主订阅其完成事件与唤醒策略不属于 DefaultLoop。Loop 的内部 Notice 不递归触发 Notification。

yourai-runtime 已提供 SessionHost、GenaiModel、文件持久化、模型摘要、宿主后台事件订阅和扩展。它们与 DefaultLoop 分层实现；完整装配入口是 Harness::open。

## 验证

`yourai-loop/tests/flow.rs` 使用可控制事件流和内存历史验证完整主链，覆盖审批、Hook 参数修改、硬拒绝、JSON Schema、普通/MCP 交互、压缩、有限重试、Stop、steer/follow-up、取消、断开、超时、预算以及批次收尾；包含真实 ConcreteHookRuntime 注册与效果消费测试。不依赖在线模型或 API Key。
