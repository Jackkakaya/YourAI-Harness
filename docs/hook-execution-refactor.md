# Hook 与业务执行解耦重构计划

## 目标与边界

业务实现者只写 run，Loop 作者只调度公共业务操作。框架操作包装负责 dispatch、应用修改或阻断、业务执行、提交与收尾。HookRuntime 负责匹配、执行、校验与聚合，不能自行修改业务状态。

保留原 AgentLoop。共享 TurnExecution 是操作能力与调用账本，持有唯一 inbox 消费者、取消、期限、历史与未结工具；DefaultLoop 自己持有默认调度计数和强制收尾策略。运行时工具查找返回私有持有 backend 的 ToolBinding，普通调用使用 exec。Rust 外部实现 trait 的 run 必须公开，这是注册侧协议，不作为正常运行时调用入口。

## 执行步骤与状态

- [x] 从 DefaultLoop 提取工具、授权、交互、输入和清理包装。
- [x] 移除临时 LoopProgram/LoopRun/LoopAction/ProgramLoop 协议，原 AgentLoop 直接创建公共能力。
- [x] 默认步数、强制文本回答和最后一步重试回归 DefaultLoop；共享模型入口只接受明确请求选项。
- [x] 输入、权限、MCP 交互和压缩都有正常公共操作；完成返回 Completed / NeedsMoreWork。
- [x] ToolRegistry 查找返回 ToolBinding；backend 私有，绑定稳定，待处理身份只结算一次。
- [x] ContextManager 拆成业务准备和摘要提交任务，统一 compact 包装覆盖替换实现。
- [x] Session、TaskBoard、Workspace、Subagents 保留各自公共操作与私有业务实现。
- [x] 配置更新捕获下一 turn 的输入策略，不替换 AgentLoop。
- [x] 自定义原始 AgentLoop、替换 ContextManager、直接操作与绑定变化的集成测试。
- [x] 更新接口文档与完整 28 事件对应表。
- [x] 完成 workspace 全量测试、Clippy、格式与 diff 检查，并记录最终结果。

## 28 个事件覆盖

完整逐个入口与效果见 [公共执行文档](./execution.md#28-个-hook-的入口)。归属如下：

| 入口 | 事件数 | 事件 |
|---|---:|---|
| 工具 exec/call | 3 | PreToolUse、PostToolUse、PostToolUseFailure |
| 权限 authorize | 2 | PermissionRequest、PermissionDenied |
| 输入接纳 | 1 | UserPromptSubmit |
| 完成 / 模型执行 | 2 | Stop、StopFailure |
| 会话生命周期 / 提交通知 | 3 | SessionStart、SessionEnd、TurnCompleted |
| 子代理执行 | 2 | SubagentStart、SubagentStop |
| 上下文摘要 | 2 | PreCompact、PostCompact |
| MCP 交互 | 2 | Elicitation、ElicitationResult |
| 任务操作 | 3 | TaskCreated、TaskCompleted、TeammateIdle |
| 工作区操作 / 变化检测 | 8 | Setup、Notification、ConfigChange、InstructionsLoaded、WorktreeCreate、WorktreeRemove、CwdChanged、FileChanged |
| 总计 | 28 | |

## 关键验收

1. 自定义 AgentLoop 使用公共操作，无需引用 Hook 事件或消费 HookDispatchResult；Stop 继续时不重建调度器，也不重放工具。
2. 工具参数修改先于校验和执行；拒绝不运行 backend；审批后的修改仍需通过硬策略。
3. post 阻断或取消保留真实副作用结果；错误收尾保存部分文本、用量和 pending。
4. 原始 inbox 始终只有一个消费者；等待审批、交互和业务操作时正确路由 steer/reply。
5. 替换 ContextManager 自动得到摘要 hooks；未变更、只剪枝没有摘要事件；post 失败不撤销摘要提交。
6. 配置变化保留已有 AgentLoop；注册表更新保留已绑定工具；所有正常操作避免重复触发事件。
7. 原有会话、任务、工作区、子代理和 Hook Runtime 的回归测试继续成立。

## 验证记录

最终验证：`cargo test --workspace` 共 19 个测试套件，477 通过、0 失败、2 忽略；`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check` 和 `git diff --check` 均通过。

新增自定义业务入口验证共 15 个场景（execution 14 个、context 1 个）。测试包括原始 AgentLoop 逆序调度、直接工具执行、Stop 继续、输入拒绝、权限拒绝、MCP 前后修改、最终模型失败、协作取消、配置保留调度器、内置与替换上下文摘要、摘要阻断/提交后停止、只剪枝，以及注册变化中的工具绑定、公开绑定 exec 的生命周期及底层接口拒绝替换 backend。
