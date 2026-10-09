# 代码质量整改：策略归属与持久化

本次保留 core / harness / TUI 三个包，以及 SessionHost / DefaultLoop /
DefaultContext / SQLite 的分工。整理的单位是职责和不变量，不按文件行数新增公共 trait。

## 模型执行策略

- `ModelProvider::timeouts()` 是所选模型的默认响应头/读取时限，默认各 300 秒。
- `GenaiModel` 在传输端执行时限；`ConfiguredModel` 统一持有所选模型的容量、输出预算和显式时限，
  `MeteredModel`、`SourceModel` 透传默认值。LoopConfig 不再持有另一份模型默认超时。
- 主循环优先级：TurnLimits.model_timeout > 请求 ChatOptions > 模型默认值。
  complete、compact、模型 Hook 和子 Agent 共享所持模型的默认值。
  Hook evaluation 自身的总时限仍独立存在。
- 模型与上下文管理器通过同一次 Providers.update 发布。Hook 和新建子任务使用调用时冻结的依赖快照，默认继承模型；显式固定模型的子任务保持原身份。
- TUI 的 `Config::resolve` 一次返回 ResolvedModel，包含已配置的模型和运行设置；容量不再存入上下文策略；
  启动、切换、创建会话共用，不在不同入口重新解析超时和请求策略。

## 错误与 Hook 契约

流式失败保留 HTTP status、body、headers，沿 SDK 的结构化错误链传递。
harness 的 `model/failure.rs` 只在一个位置解开 `YourAiError` 中的 SDK 嵌套；重试提示、错误分类和日志共用该解释。
不从显示字符串猜测状态码。流式与 collected 请求都覆盖真实 HTTP 429 / Retry-After 测试。

`ModelProvider::classify_error()` 是分类的插件接口，返回 core 定义的中立 `ModelErrorClass`。
core 不解释厂商错误码，默认返回 Unclassified；默认 recovery 只映射模型提供的分类。
GenaiModel 在私有 `model/failure.rs` 中集中解释 OpenAI 兼容错误码和 HTTP 状态，
已知额度/上下文错误优先于状态码。其他 provider 可按自己的协议分类，甚至完全不使用 HTTP。
SDK 的 WebStream、WebModelCall、WebAdapterCall 经过 harness 的统一原始数据提取路径，
提取状态/body/headers 不等于决定恢复策略。

| 位置 | 管理内容 |
| --- | --- |
| core `ModelProvider` / `ModelErrorClass` | 中立分类和恢复接口，不含厂商协议判断 |
| GenaiModel / `model/failure.rs` | 具体厂商错误解释；当前为 OpenAI 兼容字段与 HTTP 状态回退 |
| harness `model/retry.rs` | Genai 的 Retry-After 解析、默认退避/抖动、统一时限上限 |
| MeteredModel | 调用内部 provider 的分类与提示，协调一次失败的计数和冷却通知 |
| `model/control.rs` | 接收分类和提示，维护共享准入与冷却，不解析原始错误 |
| `metrics::Attempt` | 只记账；首次失败返回 true，防止重复通知 |

ConfiguredModel、SourceModel、MeteredModel 均透传 classify_error 和已有 recovery override。
Loop 仍调用 provider 的 recovery/retry_after；它不会用厂商解析代替插件的决定。
只有 RateLimited 表示应触发共享冷却，使用内部 provider 的 retry_after，缺少提示时使用
配置的 cooldown_seconds。其他分类的提示只影响本次重试。额度耗尽、上下文溢出、服务端
故障的 Genai 分类都不会触发共享限流冷却。`rate_limited` 指标继续统计实际 HTTP 429，
不把非 HTTP provider 的分类伪装成 HTTP 状态码。

MeteredModel 对同一请求仅在首次失败时通知冷却；重复 Err、End 后 Err、无终止事件的 EOF
及 Drop 取消都不会额外延长冷却。对外 retry_after 仍将 provider 提示与剩余共享冷却取较长者；
两者均不存在时返回 None，不能用 Some(0) 跳过正常退避。

Loop 用初始等待与无响应头上限构造 Backoff；retry 模块不依赖 LoopConfig。
响应头存在但无有效提示时沿用 OpenCode 的全局上限分支，该兼容规则只在默认 retry 实现中。
解析器、退避和共享冷却统一使用 `i32::MAX` 毫秒上限；零秒提示有效，不附加抖动。

### 自定义模型迁移

以前自定义 ModelProvider 未覆盖 recovery 时，会隐式继承 core 对 genai 429/5xx 的解释。
现在默认分类为 Unclassified、默认恢复为 Fatal，避免把 Genai 策略强加到其他插件。
需要自动恢复时实现 classify_error；只有明确返回 RateLimited 才启用共享冷却。
已有 recovery/retry_after override 保留，但 recovery override 本身不授予共享冷却语义。
无需引入新的插件注册体系、配置工厂或独立策略 trait。

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
事务提交统一拒绝 Closed 状态；关闭释放文件锁与发布 Closed 也在该事务锁内完成，
旧 Host 引用或延迟持久化任务无法覆盖重新打开的会话。Closing 仍允许必要的关闭和恢复
事务，但 watch_path、set_cwd 等外部修改会拒绝。获取 operation 锁后再次检查关闭状态，
压缩发布 Compacting 时在 live 锁内复查，避免关闭竞争使状态倒退。
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

文件 write/edit 的实际 blocking 任务持有 owned 文件锁，直到读改写和原子替换结束。异步等待者超时或取消不会提前释放锁；取消检查保留在提交前。

命令 Hook 的首行探测与后续前台 stdout 共用 1 MiB 字节预算，stderr 独立限制为 1 MiB。超限返回失败，按既有 failure policy 处理，不解析截断后的结构化回复；读取后续输出时仍排空管道，避免子进程被满管道卡住。

- 主循环、子 Agent、collected 调用继承同一模型时限；显式 turn override 仍生效。
- 实际流式 HTTP 错误保留 Retry-After，不能只测试人工构造错误。
- 写事务等待时状态查询和取消保持响应；提交等待者被取消后磁盘与内存一致。
- 取消启动再关闭会话，未开始输入可归还；模型 Drop 日志能经 flush 确认。
- Host 对错误 Hook outcome 的拒绝与 Loop / compact 一致。
- 原有工作区测试、SDK 测试、静态检查和 TUI smoke 继续通过。
