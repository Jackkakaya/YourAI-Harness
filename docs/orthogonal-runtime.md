# 配置与运行职责收敛

本次先完成现有进程内运行架构的收敛，共享 Server 是独立后续里程碑。保持三个 crate，不新增通用 Executor、Factory、配置管理器或事件总线框架。

后端控制迁移已完成，见 [后端与 UI 分工](backend-ui-separation.md)。TUI 不再驱动会话或管理执行任务；共享网络 Server 尚未实现。

## 状态与操作的归属

| 对象 | 唯一职责 |
|---|---|
| Harness | 装配以及模型、权限切换 |
| SessionManager | 会话生命周期、输入队列、子会话、关闭 |
| AgentLoop | 操作的调度顺序；DefaultLoop 无配置、无会话状态 |
| Turn | 单轮输入路由、调用账本、部分输出、完成状态 |
| Model / Tool / Compactor | 各自完整执行流程和对应 Hook |
| ContextManager | 活动上下文、投影、并发更新协调 |
| SessionStore | 持久化、事务、租约、版本与身份校验 |
| Workspace | 指令、工作目录、监听、Git worktree |
| TaskManager / Subagent | 任务转换 / 子会话操作 |
| TUI View / Renderer / Presentation | 显示投影 / 布局 / 帧提交 |

请求、结果、事件和快照是数据结构，不按数量削减。取消、租约、原子提交等资源保障不因减少类型而移除。

## 已实施

- `Config` 是唯一可编辑配置；`ExecutionOptions` 为 execution 类别数据和每轮冻结副本，替代 `LoopConfig`、`ExecutionConfig`、`SessionConfig`。
- 输入字段合回执行数据；InputSettings 已删除，不再独立管理或按会话保存。
- `SessionOptions` 表达创建/恢复选择及适配器装配，配置通过 `config: Config` 传入；替代含义混杂的 `HarnessConfig` 名称。
- 默认值只定义一次。`Turn::open(context)` 只读取宿主传入 TurnContext 的冻结配置，调用 `ExecutionOptions::resolve` 一次。
- 配置的步数和操作超时是上限，本次 `TurnLimits` 只能收紧；绝对 deadline 保持为运行控制信息，不因重试或压缩重置。
- `DefaultLoop` 为无状态调度策略。独立运行通过 `Agent::run_with(input, config, options)` 显式传入配置，不另传循环配置。
- `ToolOutputStore` 是装配依赖，位于 provider 快照；不放进配置。每轮绑定固定实现，模型与程序调用共享同一工具账本。
- 应用共享 Config；SessionManager 每轮读取并冻结执行数据。Agent、AgentBuilder 与 ProviderSnapshot 都不保存可编辑配置。模型切换在会话操作锁内更新模型/上下文绑定，不改写启动 context_policy。
- `Workspace.change_config/notify/config` 移除。配置修改属于共享 Config 的 getter/setter 与 save；SessionManager 只读取每轮执行数据，输入提交通知仍属于宿主。
- 创建、恢复、子会话共享 `prepare_session → assemble → start_session`；适配器差异留在装配阶段，生命周期和初始化失败清理只实现一次。
- 子会话继承父会话执行设置，不自动激活父会话的技能、记忆或指令路径；ContextPolicy 来自冻结的上下文选择；显式模型、安全和沙箱继承保持原语义。
- `SessionStore` 的租约、状态、原文操作成为必须实现的方法，不再静默继承 `unsupported` 默认实现。
- 无生产实现的 `ObservabilityProvider/Span` 移除，实际失败和 Hook 耗时通过 `tracing` 记录。
- 标题生成和旧会话标题恢复迁入会话侧，TUI 查询标题不写元数据。
- TUI 的 App 不再取得 Harness 访问运行内部状态；经 Controller 查询环境、队列和任务投影。测试仍可读取底层对象进行断言。
- 权限模式从 Harness 当前安全绑定取得，Controller 不另维护 `yolo` 布尔值。

## 第二批接口收敛

- 删除 Providers 包装，Agent 直接拥有依赖绑定、原子更新与快照。调用从 agent.ctx().set_model(...) 收敛为 agent.set_model(...)，从 agent.ctx().snapshot() 收敛为 agent.snapshot()。
- 删除 Agent::ctx 与 SessionManager::history 转发；Agent::bindings 和会话 watch_paths 限为 crate 内部。
- 删除未使用的 Turn::limits / check_control 重复入口和 TurnInfo.started_at 字段。Turn::config 返回已解析配置；取消和绝对 deadline 继续由执行机制保证。
- Agent 通过 Arc 共享同一绑定所有者，Builder 和 ProviderSnapshot 继续分别表达装配参数和单轮冻结选择。

## 当前配置契约

用户后续要求采用简单的文件与共享内存方案，替代此前分散的会话设置管理。完整清单见 [配置契约](core-contracts.md#配置读写与持久化契约2026-10-13)。

- 一份共享 Config；yourai.json 保存全部配置。只在初始化 init 时读取，旧分拆文件仅用于迁移，不自动监听。
- 嵌套 setter 直接修改同一份 Config 并内部加锁，save 原子写入完整主 JSON；普通保存失败保留内存修改，模型和安全切换失败回滚对应分组。
- 模型/effort、YOLO、主题都保存主配置；不恢复 SQLite 中独立的会话配置。
- 应用配置只使用主 JSON；SQLite 仅保存会话等领域数据。
- 每轮从共享配置的 execution 类别复制 core 执行数据；已经运行的轮次冻结。Prompt 用于新会话，已存在的会话保留冻结历史。
- 旧会话偏好与独立 InputSettings 已删除；模型数据只保留 ProviderConfig 与 ModelConfig；选择字段直接属于 Config。没有新增 ConfigManager 或配置服务。

独立执行示例 [session_config.rs](../crates/yourai-harness/examples/session_config.rs) 仍演示显式执行数据与单轮限制，不是第二份应用配置。

## 验收

运行 workspace 测试以及 TUI 离线 PTY smoke。检查配置覆盖、恢复、取消、Hook、工具原文、模型切换与子会话继承；新增配置集成测试覆盖自定义 Loop、限制解析、共享配置读取与文件保存、恢复与服务侧标题。

启动子会话的测试等待包含多次 SQLite 初始化与异步 I/O，首个进度等待改为 10 秒并在 worker 提前退出时立即报告原因；关闭仍要求在原有 2 秒期限内完成。

## 后续独立里程碑

共享网络 Server 尚未实现。driver、submitter、compact 以及切换/关闭流程已移至 harness 后端，独立于 UI 事件消费。后续需要独立服务进程、带游标的网络订阅，并分离会话关闭与客户端断开。

实施 Server 前不引入抽象 Transport 或多协议支持。需单独验证双客户端唯一执行、审批只生效一次、快照订阅不丢事件、Server 重启恢复、数据库租约 fencing。

配置现按 models/execution/security/ui/session/prompt/hooks 分类，类别只是数据分组，各组都由 Config::init、嵌套 setter 和 Config::save 统一加载/修改/保存。JSON 与 Rust 路径一致，旧平铺格式可读、保存后迁移。详见 [配置契约](core-contracts.md#配置读写与持久化契约2026-10-13)。
