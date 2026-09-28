# 代码质量整改：策略归属与持久化

本次保留 core / harness / TUI 三个包，以及 SessionHost / DefaultLoop /
MemoryContext / SQLite 的分工。整理的单位是职责和不变量，不按文件行数新增公共 trait。

## 模型执行策略

- `ModelProvider::timeouts()` 是所选模型的默认响应头/读取时限，默认各 300 秒。
- `GenaiModel` 在传输端执行时限；`ConfiguredModel` 将 Harness 的显式配置应用到任意模型，
  `MeteredModel`、`SourceModel` 透传默认值。LoopConfig 不再持有另一份模型默认超时。
- 主循环优先级：TurnLimits.model_timeout > 请求 ChatOptions > 模型默认值。
  complete、compact、模型 Hook 和子 Agent 共享所持模型的默认值。
  Hook evaluation 自身的总时限仍独立存在。
- 模型切换仍只替换主模型；已经配置的 Hook / 子 Agent 模型保持原身份，这是既定策略。
- TUI 的 `Config::resolve` 一次返回 ResolvedModel，包含模型、上下文和运行设置；
  启动、切换、创建会话共用，不在不同入口重新解析超时和请求策略。

## 错误与 Hook 契约

流式失败保留 HTTP status、body、headers，沿 SDK 的结构化错误链传递。
`YourAiError` 只在一个位置解开 SDK 嵌套；重试提示、错误分类和日志共用该解释。
不从显示字符串猜测状态码。流式与 collected 请求都覆盖真实 HTTP 429 / Retry-After 测试。

`HookDispatchResult` 集中检查 event 与 outcome 类型，并提供尊重 suppress_output 的可见消息。
Loop、compact、Host 各自保留停止、取消和消息路由策略。
Hook runtime 的纯解析/聚合放在私有 `outcomes` Module，调度和后台生命周期留在 runtime。

## 宿主持久化

私有 journal 事务遵守以下顺序：

1. 获取独立的 journal 写事务锁。
2. 短暂读取 live 状态，复制候选 journal。
3. 在不持有 live 锁的情况下写临时文件、fsync、rename、同步目录。
4. 短暂获取 live 锁，发布已提交的 journal；完成相关运行状态变更。

所有 durable journal 写入及 inbox 的启动/结束切换遵守同一事务锁。
状态查询、取消和 Reply 不等待文件落盘。输入只在持久化成功后投递给运行中的 Loop。

同步 `submit` / `post_event` 等入口继续提供同步持久化确认，本身仍是阻塞调用。
异步调用方使用 `submit_async` / `post_event_async` 等入口；整个事务在 owned blocking task
内执行。取消等待者不取消已经开始的事务，因此丢失确认时不能盲目重投同一输入。
Turn 启动与收尾由拥有 operation guard 的监督任务负责，避免等待者离开后遗留 Running 状态。

TUI 通过有序提交队列等待落盘，绘制和按键循环不等待该确认。
落盘失败通过既有 InputRejected 事件提示，已提交草稿仍可经输入历史编辑，不覆盖新草稿；
切换会话前检查待提交输入，关闭时排空提交队列。

## 模型请求日志

请求日志使用一个有序 writer；开始、完成和取消按顺序入队。
Attempt::Drop 只入队，不写数据库、不等待锁。不同 provider budget 共用该 writer。
日志写入错误累积到 journal_errors，flush 返回持久化错误。

`ModelBudget::flush` 是落盘确认点；`Harness::close` 在 Host、子任务和 Hook 收尾后调用它。
关闭的 durable cleanup 与 pending 输入领取分开；关闭等待取消后可重试领取。
诊断日志 flush 失败记录到 host.last_error 和 journal_errors，不吞掉 pending 输入。
会话收尾提交失败时，已接管的输入保留在内存恢复队列，宿主进入 Closing，禁止继续执行。
日志 flush 成功时，正常关闭不会丢弃取消记录；进程崩溃仍可能留下未刷新的诊断日志，不能作为计费级账本。
会话输入 journal 继续保留每次操作的持久化确认，不与诊断日志混用。

## 验证重点

- 主循环、子 Agent、collected 调用继承同一模型时限；显式 turn override 仍生效。
- 实际流式 HTTP 错误保留 Retry-After，不能只测试人工构造错误。
- 写事务等待时状态查询和取消保持响应；提交等待者被取消后磁盘与内存一致。
- 取消启动再关闭会话，未开始输入可归还；模型 Drop 日志能经 flush 确认。
- Host 对错误 Hook outcome 的拒绝与 Loop / compact 一致。
- 原有工作区测试、SDK 测试、静态检查和 TUI smoke 继续通过。
