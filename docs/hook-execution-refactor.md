# Hook 简化实施记录

最终设计已实现，见 [接口与 28 个入口](execution.md)、[对象划分](hook-template-design.md)、[问题修复](hook-design-review.md)。本次改造基于 main 中已合并的 PR10 / PR11，统一提交简化实现与审查修复。

## 已完成

- Tool、Model、Compactor 直接拥有完整公共 exec 和私有业务步骤；删除 Turn 中旧工具 / 模型执行入口。
- Turn 的调用阶段、固定工具、真实结果和稳定结果身份合为一条记录；部分响应和延迟上下文成功保存后才移除。
- 授权、MCP 交互是实际普通函数，没有额外 Executor。
- LoopConfig 只包含 steps 与 ExecutionConfig，默认值定义一次。
- TaskManager 直接实现 ToolProvider，复用 SQLite；删除独立 TaskTool / TaskStore / JsonTasks。
- Subagent 直接实现 ToolProvider；SessionHost 唯一 children 集合；SubagentFactory 只装配 child。
- Workspace / TaskManager 每个 host 唯一；初始化、关闭和 owned 写入维持资源所有权。
- Hook 注册即编译为 handler，删除重复注册来源分支；once 在执行前原子认领。
- 原审查发现的重复执行、取消提交、无效设置、动态指令和候选清理问题同步修复。
- 当前设计、类型清单、迁移示例和架构接口同步更新。
- 最后一轮审查的 14 项修复完成：输入 / 调用的不确定提交、终端响应和用量控制、cwd 和 child 能力快照、动态指令、Git 登记、Task 迁移、交互期限、压缩提交状态及大输入 Hook 管道。逐项见 [审查记录](hook-design-review.md#最后一轮审查的-14-项修复)。

## 最终验证

2026-10-09，在最终代码上完成：

- `cargo test --workspace`：22 个套件，544 通过，0 失败，2 忽略（含 3 个 doctest）。
- `cargo test --workspace --tests -- --test-threads=1`：20 个套件，541 通过，0 失败，2 忽略。
- `cargo clippy --workspace --all-targets -- -D warnings`：通过。
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`：通过。
- `cargo fmt --all -- --check`、`git diff --check`：通过。
- `cargo build -p yourai-tui`：通过。
- TUI 的 smoke、launcher、models、ui、frames 五项离线冒烟：全部通过。使用短临时 cwd 避免工作树长路径使会话标题被截断。

重点新增验证：多个调用保存失败仍保留各自真实结果、禁止重放、Completed 终态、dropped model future 的可见响应、失败 checkpoint 的上下文、固定定义、once 并发认领、启动结果无人接收、关闭清理失败、动态指令进入真实请求、真实 Git 效果与登记失败重试、恢复取消不消费输入，以及数据库提交确认早于视图恢复。

最后一轮增加 22 项回归，覆盖真实 SQLite 中的输入丢回执与恢复、原附件冻结、Hook 上下文原子保存、身份冲突、工具拒绝与模型保存失败后的结算、用量 / 维护恢复取消、普通安全和沙箱继承、startup 热替换、当前 cwd 的审批与执行、A→B→A 指令、linked worktree HEAD、跨仓库丢目录清理、旧登记兼容、Task 迁移和标记失败、MCP 绝对期限、提交后 reload 失败，以及 262144 字节 Hook 输入和异步交接。SQLite fork 的既有回归同时验证新输入身份同步。

Hook 业务对象仍为 8 个；生产类型声明为 231 个 struct、22 个 trait、70 个 enum，包含 TUI、配置、协议数据及内部清理对象。此轮仅增加一个私有 WorktreeRegistration 数据枚举兼容新旧登记格式，没有增加业务执行类。

以下记录保留前一阶段当时的接口和完成状态；其中“未实施”描述当时，最终状态以本节和最终设计为准。

## #10 历史验证记录

独立 PR 工作树验证：`cargo test --workspace` 共 19 个测试套件，473 通过、0 失败、2 忽略；`cargo test --workspace --tests -- --test-threads=1` 共 17 个测试套件，470 通过、0 失败、2 忽略；`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check` 和 `git diff --check` 均通过。`cargo build -p yourai-tui` 与 CI 使用的 5 个离线冒烟脚本（smoke、launcher、models、ui、frames）均通过；本地使用短的临时工作目录，避免长工作树路径使页脚隐藏脚本等待的完整会话标题。

新增自定义业务入口验证共 15 个场景（execution 14 个、context 1 个）。测试包括原始 AgentLoop 逆序调度、直接工具执行、Stop 继续、输入拒绝、权限拒绝、MCP 前后修改、最终模型失败、协作取消、配置保留调度器、内置与替换上下文摘要、摘要阻断/提交后停止、只剪枝，以及注册变化中的工具绑定、公开绑定 exec 的生命周期及底层接口拒绝替换 backend。

## 前一阶段简化验证

在独立分支 `refactor/simplify-hook-design` 验证：`cargo test --workspace` 488 通过、0 失败、2 忽略；CI 单线程模式 485 通过、0 失败、2 忽略。Clippy（`-D warnings`）、格式和 diff 检查通过。TUI 构建及 smoke、launcher、models、ui、frames 五项离线冒烟通过。新增验证覆盖任务并发提交、子会话 Start 拒绝不创建持久化会话、跨续跑的事件及持久化身份一致性。

## 任务SQLite收敛验证（2026-10-09）

TaskBoard、TaskTool与JSON存储已收敛为一个core TaskManager，删除TaskStore和JsonTasks。TaskManager直接实现现有ToolHandler；工具和TUI共享同一Arc，持久化复用SessionManager与现有sessions.sqlite3。schema升级v5，原子导入旧tasks.json，原文件保留。

`cargo test --workspace`：19个套件，495通过、0失败、2忽略（含3个doctest）。迁移前5个使用假TaskStore的core测试已移到Harness真实SQLite场景；runtime与SQLite两个套件共57项通过。覆盖共享实例、重启和顺序、并发32项写入、hook拒绝/无效结果/效果保存失败、SQLite提交失败、完成幂等、迁移及事务回滚、会话隔离和级联删除、等待方取消后的提交/缓存一致，以及长hook期间关闭并重开后禁止旧host写入。

严格Clippy（`--workspace --all-targets -- -D warnings`）、格式与diff检查、TUI构建均通过；TUI任务离线smoke验证了实际工具调用、Todo显示、压缩后恢复历史/任务和退出。其余最终设计调整及R7/R8之外的审查问题仍未实施；这里的通过结果只说明当前任务改造及已有回归通过。
