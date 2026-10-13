> 配置与启动接口已收敛，当前迁移说明见 [配置与运行职责收敛](orthogonal-runtime.md)。

# Core 组件接口契约

> 当前存储已改为 SQLite schema v6：队列、扩展状态和工具原文使用 SessionStore；SessionLease 使用数据库租约。旧章节中的状态 JSON、输出文件和文件锁描述已被 [云端存储实现](cloud-storage.md) 替代。

普通自定义调度使用 core 的 [AgentLoop 与公共执行层](./execution.md)。Core AgentLoop 保留为底层运输接口。


运行时补充接口：`TurnOptions.events` 绑定内部事件队列；ContextManager 内部协调持久化并维护归档身份索引；`SecurityProvider::update_permissions` 消费权限变更；`HookManager::subscribe_background / shutdown_session` 提供按会话订阅和后台回收。`TurnHandle::abort` 用于协作取消超时后的任务回收，宿主必须记录被强制中止执行的不确定状态。

Core 的 deadline watchdog（截止时间后 60 秒宽限）强制丢弃 Loop，或 Loop 在构造/执行 future 时 panic，统一返回 `ErrorKind::LoopTerminated` 并取消 turn token。它表示执行状态未知，与正常协作收尾的 `Aborted(DeadlineExceeded)` 区分。宿主必须隔离会话并保留活动输入供检查；不得自动重放。Core 仍回收 inbox 中未消费的输入，但无法恢复 Loop 已消费的局部状态。

对应 [完整流程图](./default-loop-flow.md)。本文定义公共边界和运输机制。DefaultLoop、具体模型 Provider 和业务扩展在 `yourai-harness`；公共执行模板、会话宿主与工作区操作在 `yourai-core`，见 [实现文档](./default-loop-implementation.md) 与 [Runtime 文档](./runtime-implementation.md)。下文出现的 `yourai-loop` / `yourai-runtime` 是合并前的历史包名。

## 图与代码的对应

| 流程 | 已定义的接口/类型 | 实现边界 |
|---|---|---|
| 图 1：会话宿主 | `SessionManager`、`SessionEnvironment`、`SessionStatus`、`SessionTurn`、`InputRejected` | 具体宿主 SessionManager 在 core；不是 Context 的新 Provider 插槽 |
| 图 2：主循环 | `AgentLoop`、`TurnResult`、`TurnFailure`、`TurnInfo`、`TurnOptions`、`TurnLimits` | core 传递环境与结果；AgentLoop 自主管理调度；Tool / Model / Compactor 管理各自执行流程 |
| 图 2：compact | `CompactionRequest`、`CompactionResult`、`CompactionTrigger`；`ContextManager::project / estimate / apply` | Compactor::exec 固定 Hook；private run 生成摘要；ContextManager 核对快照并应用；SessionStore 原子提交 |
| 图 3：工具 | `ToolRegistry::resolve`；`ToolInteraction`、`InteractionRequest`、`InteractionKind`；`ToolContext::ask` | Tool::exec 位于 core tool 模块；AgentLoop 调用固定公共操作，交互桥和回复路由归公共执行层 |
| 图 4：可选扩展 | 复用 ToolProvider、HookManager、SessionManager | 不预先增加工作区、多 Agent、协作任务的公共 Provider |
| 图 5：Hook | 复用现有 HookManager / HookHandler | 保持 Hook wire 协议；公共业务入口自动触发并消费 Hook，Core 底层运输接口不代替业务包装 |

## 1. 会话宿主

`SessionManager` 直接实现会话控制流程。创建/恢复由具体实现的构造入口负责，完成历史绑定、Agent 装配及 SessionStart 后才暴露实例。

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

`submit` 成功表示已入队，不代表输入已通过附件解析或写入历史。默认 Loop 的输入接纳发生在执行期：

- 明确拒绝（无效附件、UserPromptSubmit 阻止）：发布 `Out::InputRejected { rejection: InputRejected { input, reason } }`，并记录到 `TurnOutput.rejected`；原输入不写历史、不调用模型，也不进入 `pending` 自动重试。`InputRejected` 统一定义于 protocol，原 `session_runtime::InputRejected` 路径继续重导出。
- 暂时执行失败（存储失败、暂时 I/O 故障、超时或取消）：保留原输入到 `TurnFailure.output.pending`，沿用已有重试约定。
- UserText 的可选 id 对旧 wire 保持兼容，入口在转移所有权前补齐。重试必须复用 pending 中的身份；StoredMessage.input 保存原输入，恢复已确认提交后不重复追加或准备附件。用户消息与该次 Hook 附加上下文须由 ContextManager.append 原子保存。
- `Out::InputRejected` 与 `TurnOutput.rejected` 是同一拒绝的实时通知与最终报告；消费者选择一种恢复渠道，不能当成两次拒绝。非流式 `Agent::run` 调用方需要检查 `rejected`，成功完成接纳判定不等于输入已提交给模型。
- 附件读取和解码在阻塞工作线程准备；主循环继续响应取消与 deadline。取消后的只读工作可以自行结束，但不提交历史。相对 FileRef 路径基于 `SessionEnvironment.cwd` 解析。

详细状态归属与恢复流程见 [草稿与输入接纳设计](./draft-admission-design.md)。

外层驱动再次调用 run_next 处理后续任务，每个调用的 OutSink 只对应一个 Turn。宿主持有 TurnHandle、转发事件、保证同会话执行互斥。它在返回前将成功或失败报告里的 pending **移动**回宿主队列，报告的 pending 清空；失效 Reply 不得启动下一 Turn。

这不是“一个 submit 就自动启动后台常驻 actor”的接口。宿主可以提供更高层自动驱动入口，但 core 不要求特定调度架构。

其他操作：

- `context()` / `status()`：会话环境和展示用状态；不能先读状态再假设操作不会竞争。
- `interrupt()`：请求取消当前 Turn，不清空后续输入，也不代表清理已经完成。
- `compact(request, cancel)`：手动压缩，与 Turn 写历史互斥；ContextManager 的摘要生命周期包装负责前后 Hook。
- `close(timeout: Option<Duration>)`：拒绝新输入、清理当前执行、SessionEnd、释放资源，成功时归还剩余输入；None 等待清理完成，Some 显式限制整个关闭过程。

`SessionStore` 继续负责元数据；`ContextManager` 负责历史；SessionManager 不通过监听 Out 重复保存历史。状态机、互斥、后台事件与恢复策略由 core SessionManager 实现。

## 2. Turn 身份、环境和限制

```rust
let mut options = TurnOptions::default();
options.session = Some(Arc::new(SessionEnvironment::new(session_id, cwd)));
options.limits.max_model_calls = Some(32);
options.limits.deadline = Some(Instant::now() + Duration::from_secs(120));

let handle = agent.start_with(In::user_text("任务"), ExecutionOptions::default(), options)?;
// handle.info.id 与 Loop 中 tc.info.id 对应同一次执行。
```

- `Agent::start_with` / `run_with` 接收 TurnOptions；原 `start` / `run` 使用默认参数。
- `TurnInfo` 包含 TurnId、开始时间及本次选项；通过 TurnContext.info 传给 Loop。
- 有 SessionEnvironment 且装配了 ContextManager 时，Core 校验其 session_id 一致，避免写入其他会话的历史。
- `SessionManager.environment()` 读取会话运行环境；聊天历史由 Agent 的 `context_manager` 提供。两者不共用 `context()` 这个名称。
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
restore()                              同步已开始写入并恢复已提交视图
append(Vec<StoredMessage>)              保存消息，成功后更新内存
build_request(tools, model)             只读构造模型请求和用量估算
project(records, tools)                 与主请求一致的候选视图
estimate(request, model)                模型容量与媒体估算
apply(expected, change)                 核对快照后原子应用
```

Compactor 持有本次选定的 context / model / hooks / usage，公共 exec 固定执行 PreCompact → private run → 已提交摘要的 PostCompact。选区、分块、预算与校验都在 private run；ContextManager 不生成摘要，也不持有 HookManager。它保留冻结的 system prompt，并投影当前有效的工作区指令原文。

Hook 和模型请求不持有上下文写锁。apply 在提交前核对快照，避免覆盖并发追加。取消区分未提交、未知提交和已确认提交；未知状态必须 restore，不重放。已提交后的恢复或 PostCompact 失败返回 stop_reason，不撤销摘要。没有安全旧历史时返回不变；只有提交摘要才发送 PostCompact。完整流程见 [上下文设计](context-manager-design.md)。

## 5. 工具与交互

ToolProvider 提供 definition、security_context、run；没有第二份 name。ToolRegistry 注册时构造 Tool 并固定定义，resolve 返回持有私有 backend 的 Tool。调用者使用 Tool.exec；ToolContext 由其私有 run 装配。

```rust
let call = turn.enqueue("my_tool", serde_json::json!({})).await?;
turn.tool(&call.call_id)?.exec(&mut turn, &call.call_id).await?;
```

一次模型请求的 schema 和对应待执行 Tool 来自同一 snapshot。执行前认领身份，真实结果进入本调用记录后再执行 post。开始后不能重放，保存失败时稳定身份用于结算。

```rust
let answer = tc.ask(
    InteractionKind::Question,
    serde_json::json!({ "question": "选择输出格式" }),
).await?;
```

ToolContext.ask 生成独立 request_id，关联 call_id；未配置交互立即报 Config。Tool 私有 run 等待业务时同时服务交互通道，interaction::elicit 负责 MCP 前后事件与答案校验。Turn 负责唯一 inbox 和 Ask / Reply 路由，普通提问不触发 MCP Hook。

第二层 Security 仍仅 Allow / Deny；提问不能绕过硬策略。

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
4. ContextManager 实现 session_id、restore、append、build_request 和原子 apply；需要定制投影或校准时覆盖 project / estimate；压缩经唯一 Compactor 执行。
5. ToolRegistry 实现 register / unregister / snapshot；其余查询复用默认实现；ToolProvider 以 definition 作为唯一名字。
6. 手工构造 TurnContext 需传入 info；正常使用 Agent 启动时由 core 完成装配。

core 直接实现执行、会话、工作区、任务和子会话的固定流程；Harness 提供默认 Loop、业务 Provider、存储及装配。

## DefaultLoop 接入时的补充

- ModelProvider::stream_events 提供事件流；ModelRecovery 和 recovery() 提供保守的恢复策略。
- ModelProvider::classify_error() 提供中立错误分类，默认 Unclassified；core 不解析厂商错误码。默认 recovery() 映射模型分类，Adapter 可独立覆盖。RateLimited 显式启用共享冷却，使用模型的 retry_after 或配置回退；其他分类不会触发共享冷却。GenaiModel 自行解释结构化 HTTP 错误。
- OutSink::closed 默认保持 pending，core 通道实现提供真实关闭信号。
- AbortReason::HookStopped 表示 Hook 通用停止请求。
- ContextManager/SessionStore 工具结果提交按 call_id 防重复；ToolProvider 执行 future 须 cancellation-safe。

### 模型传输层超时

`ModelProvider::uses_transport_timeouts()` 默认 false。返回 true 的实现必须将
`ChatOptions.stream_header_timeout` 用于 HTTP 响应头等待，将
`stream_read_timeout` 用于每次原始响应体读取；这两个值只用于本地传输，不能发入模型请求 JSON。
Loop 对这类 Provider 只在外层监听总 deadline、取消和断连；否则仍使用模型事件等待超时。
所有包装 Provider 必须透传该能力，避免把心跳或未解析完成的数据误判为无进展。


### 模型默认值和宿主异步提交

模型默认时限由 `ModelProvider::timeouts()` 提供；模型传输默认时限不放进调度策略。
包装 Provider 必须透传该值。主循环依次采用显式 TurnLimits、请求选项、模型默认值。

默认 SessionManager 保留同步 `submit` 的持久化确认，同时提供 `submit_async`。
异步入口等待 owned blocking transaction，取消等待不代表撤销已开始的提交。
宿主状态查询和取消不持有 journal 写事务锁。详细设计见
[代码质量整改](./architecture-quality-repairs.md)。

## 配置读写与持久化契约（2026-10-13）

配置入口统一为一个共享 `Config`。它按类别组织数据；各类别不读写文件、不拥有独立全局实例，全部通过 config::update 修改。没有 ConfigManager、设置服务或额外 Repository。

| 类别/字段 | 数据 struct | 用途和来源 |
|---|---|---|
| models | ModelSettings | selected、compaction、variant、effort_overrides、providers；主配置 |
| execution | ExecutionOptions | steps、超时、重试、context、附件、技能/记忆执行数据；主配置 |
| security | SecuritySettings | yolo、trusted_shell；主配置 |
| ui | UiSettings | theme；主配置 |
| session | SessionSettings | directory、extensions；主配置 |
| prompt | PromptSettings | system_prompt、soul_file、memory_file、profile_file、append_system_prompt、memory_file_max_chars、profile_file_max_chars、provider_max_chars；prompts.json |
| hooks | HookSettings | events、可选 file；内嵌或独立 Hook 文件 |

JSON 和 Rust 的类别路径一致。execution.context 在 Rust 中为 execution.context_policy；ContextPolicy、ImagePolicy 是执行类别的嵌套策略。RequestPolicy 和 ModelLimits 从 models.providers 的 JSON 数据解析用于资源装配；不各自维护配置源。

### 文件与兼容

主配置 JSON 或 SQLite 保存 models/execution/security/ui/session/hooks。Prompt 单独保存在同目录 prompts.json，文件内容就是 PromptSettings 的字段，不重复包装。Hook 少时使用 hooks.events；多时主配置使用 `{"hooks":{"file":"hooks.json"}}`，独立 JSON 为 `{"hooks":{"events":{...}}}`。路径相对于主配置。非空内嵌事件与外部引用同时存在时报错，避免重复注册。旧独立 Hook 的 `{"hooks":{事件名:[...]}}` 格式仍可读取。

旧平铺主配置由一个 normalize_document 函数转换成分类字段；首次普通保存迁移为分类 JSON。未知字段和同一字段的重复新旧定义都拒绝。旧内嵌 Prompt 保存时先保留到独立文件，不丢失内容。已有独立 Prompt 文件时，普通保存不改写它。

SQLite 的 app_config(id=1,config_json) 保存相同分类文档。空库从邻近 yourai.json 导入一次，随后只写所选源，不双写主 JSON。没有新增存储接口类。

### 读取和修改

```rust,ignore
let config = Arc::new(Mutex::new(Config::load(path)?));
let model = config.lock().unwrap().models.selected.clone();
let steps = config.lock().unwrap().execution.steps;
config::update(&config, |value| {
    value.execution.steps = Some(20);
    value.security.yolo = true;
    value.ui.theme = "nord".into();
    Ok(())
})?;
config::update(&config, |value| {
    value.prompt.system_prompt = Some("编程助手".into());
    Ok(())
})?;
```

Get 只读共享字段；Set 修改候选数据，校验并同步保存变化的来源，然后发布共享内存。保存时不重新加载配置；外部文件编辑只在下次启动读取。修改 hooks.file 可将缓存定义保存到新来源，无须重新读取文件。一次跨文件更新可能涉及多个独立文件提交，并非跨文件数据库事务；失败时共享内存不发布。

模型、YOLO、主题的应用操作经 SessionClient.model/set_yolo/set_theme，内部调用同一个 config::update；模型与权限还同步更新实际资源。不能只改 models.selected 就声称已切换当前模型客户端。

ExecutionOptions 同时是 execution 类别数据和 core 的每轮冻结参数。SessionManager 绑定读取共享配置的函数，下一轮取得 execution 的新值，已有轮次不被中途改写。SessionOptions 只持有一份 Config 和启动/依赖参数；ResolvedModel 是创建后的客户端和准入数据。Prompt 与 Hook 的会话资源在新建会话时装配；连接、会话目录与扩展等资源设置需要相应的重新装配操作。

历史、队列、任务、用量、权限规则、临时批准与记忆内容是领域状态，不放入应用 Config；旧会话 config/model_preferences 文档不作为全局配置来源。

### 验证

配置测试覆盖：两文件独立保存、外部修改只在 reload 生效、旧内嵌 prompt 无损分离、JSON 保存失败不发布、SQLite 同格式与写失败回滚、模型选择全局保存及资源失败保持旧绑定、下一轮读取共享配置、旧会话设置不覆盖全局值。继续运行 workspace、Clippy、fmt/diff 及两组离线 TUI smoke。

分类改造验证记录见本次工作区测试与配置集成测试；包含旧格式迁移、类别未知字段/重复字段校验、JSON/SQLite 同格式、保存失败保留内存、下一轮共享执行参数与 TUI 配置落盘检查。

本次分类改造验证：workspace 648 项通过、2 项既有 ignored；配置集成测试 15 项通过；Clippy（全目标，-D warnings）、fmt/diff 检查及两组离线 TUI smoke 均通过。

全局同步检查：中英文 README、架构/设计文档、TUI 启动与模型/会话测试均使用现行分类路径；明确的旧格式兼容用例保留。hooks.file 在读取或保存前拒绝与主配置、prompts.json 的路径别名（含符号链接），避免配置源互相覆盖。额外启动器 smoke 通过，现行配置文档的本仓库链接已核对。
