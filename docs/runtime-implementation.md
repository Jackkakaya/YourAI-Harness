# Runtime 实现与验收

> 会话记录使用 SQLite；ContextManager 内部完成消息提交、工具清理、摘要 Hook 和模型视图更新。详见 [存储设计](./session-storage-design.md) 与 [ContextManager 设计](./context-manager-design.md)。

`default-loop-flow.md` 的五张图现在统一实现在 `yourai-harness` 中，入口为 `yourai_harness::Harness::open`；`yourai-core` 仅保留协议、Provider 接口和 turn 运输机制，不反向依赖具体实现。

## 五张图对应的代码

| 图 | 实现 | 验证 |
|---|---|---|
| 1 会话宿主 | `crates/yourai-harness/src/runtime/mod.rs`：SessionHost；创建、恢复、队列、串行执行、serve 驱动、关闭、后台唤醒 | runtime 测试：队列恢复、驱动取消、并发关闭、跨会话事件隔离 |
| 2 主循环与 compact | `crates/yourai-harness/src/default_loop/`；`crates/yourai-harness/src/context/` | loop 测试：压缩调度、限制、重试与收尾；runtime/context 测试：真实摘要、完整归档、手动压缩 |
| 3 工具、审批、MCP | `crates/yourai-harness/src/default_loop/{tools,interaction}.rs`；Harness 的 ToolSet 与 PolicySecurity | loop 测试：参数重检、审批、MCP、批次中断；runtime 测试：权限持久化、完整 tasks 调用 |
| 4 扩展 | `crates/yourai-harness/src/{workspace,collaboration,memory,skills}/` | runtime 测试：Git worktree、配置恢复、任务状态、独立子会话 |
| 5 Hook | `crates/yourai-harness/src/hooks/`；`assembly/model_hooks.rs` 模型执行器 | Hook、Loop 与 runtime 测试：注册、匹配、聚合、agent Hook、后台唤醒、关闭进程 |

主要测试文件：[Loop 测试](../crates/yourai-harness/tests/loop_flow.rs)、[Runtime 测试](../crates/yourai-harness/tests/runtime.rs)、[Context 测试](../crates/yourai-harness/tests/context.rs)、[SQLite 测试](../crates/yourai-harness/tests/sqlite.rs)。

## 装配与前端入口

```text
Harness::open(config, model)
  +-- SessionCatalog + MemoryContext + SessionHost
  +-- DefaultLoop + ToolSet + PolicySecurity
  +-- ConcreteHookRuntime + DefaultHookModelExecutor
  +-- MeteredModel / ModelBudget
  +-- LocalUsage
  +-- extensions=true 时：LocalMemory / LocalSkills / Workspace / tasks / subagent

TUI 键盘 / Web 请求 -> host.submit(In)
TUI 渲染 / Web SSE  <- host.serve(limits, sink, cancel)
一次驱动           -> host.run_next(...) / run_until_idle(...)
手动压缩           -> host.compact(...)
中断当前执行       -> host.interrupt()
退出               -> harness.close()，接收剩余输入
```

[终端示例](../crates/yourai-harness/examples/run.rs)：

```sh
cargo run -p yourai-harness --example run -- <model-id> <prompt>
```

模型凭据由 genai 从环境读取。示例输出 JSON 事件，并拒绝交互审批；完整 TUI/Web 界面不在这个库中。前端实现 OutSink，将 Ask 对应的 Reply 交回 submit 即可复用同一条执行路径。

基础装配和子会话共用 `harness::assemble`；子会话显式继承父会话的 ContextPolicy。扩展默认关闭；调用 workspace() / 注册 watch path 也可显式启用工作区能力。

## 持久化与恢复

session_dir/sessions.sqlite3 保存元数据、消息/摘要和用量。宿主队列、配置、权限、任务、记忆、技能仍保存在会话目录；宿主持有 host.lock。MemoryContext 内部先提交数据库再更新活跃视图。

- follow-up 在接纳前落盘；运行中 steer 记入 active。Turn 返回的 pending 转移回宿主队列；失效 Reply 不得进入后续 Turn。
- 恢复时保留未开始的输入，将上次 active 标记为 interrupted。缺少结果的工具调用补中断结果，不自动重放工具或整次失败 Turn。调用方可读取 last_error / interrupted_inputs 后决定继续。
- compact 调用模型生成摘要，成功且候选来源仍 active 才事务提交。模型视图使用系统指令、摘要和后续消息；完整 transcript 保留。工具调用及内部事件身份查询覆盖归档，压缩不会让已完成调用重新执行。
- 手动 compact 与 Turn 互斥，有取消和总截止时间。系统指令保留；显式加载或重载指令才触发 InstructionsLoaded。
- close 取消执行、停止监听、关闭子会话、执行有界 SessionEnd、回收后台 Hook、释放宿主锁。并发关闭不重复交还输入或触发 SessionEnd。应用应显式 close；Drop 只是取消兜底。

## 扩展与后台事件

Workspace 执行指令读取、通知、候选配置验证、工作目录切换、Git worktree 创建/移除和文件监听。ConfigChange 拒绝时不应用候选配置；成功配置用于后续 Turn 并在恢复后加载。目录监听递归检测文件新增、修改、删除，不跟随目录符号链接。默认轮询 250ms。

TaskBoard 持久化任务，完成前允许 TaskCompleted 阻止；队友有未完成任务时不得报告 idle。SubagentTool 创建独立宿主和 Loop，转发输出与交互；SubagentStop 可要求有界继续，取消和关闭传播到子会话。

RuntimeEvents 按 ID 去重，运行中在 Loop 检查点消费。空闲时的 asyncRewake 在宿主允许的情况下生成 follow-up，由 serve 或外层驱动执行。普通通知不自动启动模型；后台结果按 session_id 路由。

## 27 个 Hook 的调用方

| 调用方 | Hook |
|---|---|
| SessionHost | SessionStart、SessionEnd |
| ContextManager | PreCompact、PostCompact（自动/overflow/手动共用） |
| DefaultLoop | UserPromptSubmit、Stop、StopFailure |
| tools / interaction | PreToolUse、PostToolUse、PostToolUseFailure、PermissionRequest、PermissionDenied、Elicitation、ElicitationResult |
| Workspace | Setup、InstructionsLoaded、Notification、ConfigChange、CwdChanged、FileChanged、WorktreeCreate、WorktreeRemove |
| SubagentTool | SubagentStart、SubagentStop |
| TaskBoard | TaskCreated、TaskCompleted、TeammateIdle |

ConcreteHookRuntime 支持 command/http/native/prompt/agent。DefaultHookModelExecutor 提供 prompt 和 agent 的模型执行；评估 Agent 不装配 HookRuntime，避免递归。异步 command 归属于会话，显式关闭时回收；Unix 使用独立进程组终止 shell 及组内子进程。

## 默认策略的保证边界

- Harness 的共享 ModelBudget 对主模型、compact、模型 Hook、子 Agent 统一预留调用次数并累计已知 token。默认只观测、不设置调用次数或 token 上限；只有宿主显式配置 `max_shared_model_calls` / `max_known_tokens` 时才执行跨 turn 限制。共享预算耗尽报告为 `SharedBudgetReached`，不会再伪装成单 turn 的 `LimitReached(ModelCalls)`。token 阈值阻止后续调用，不能精确限制尚未返回用量的并发请求；不是硬费用上限。预算按当前装配生命周期计数，恢复后重新配置额度，历史用量仍保存。
- ModelProvider 只提供 complete / stream_events；genai 原始流转换封装在 GenaiModel。UsageTracker 只通过 record_event 记录原始用量事件。
- PolicySecurity 持久化会话级精确工具规则，支持 addRules/removeRules/replaceRules；不支持的目的地、模式或范围表达式报错，不扩大授权。硬拒绝不能由 Hook 覆盖。
- 外部副作用和文件历史不能提供跨系统 exactly-once。崩溃后的不确定调用不会自动重试。Provider 自行脱离的任务、进程组之外的进程不在取消保证内。
- 默认装配不包含业务 shell/file/MCP 客户端或操作系统沙箱。通过 ToolHandler / SandboxProvider 接入；Loop 已实现共用的权限、取消、交互和结果流程。
- 自动测试使用可控模型，不请求在线模型；GenaiModel 的在线连通性未验证。
