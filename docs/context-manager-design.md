# 上下文与压缩

## 职责

只保留一个压缩对象 `Compactor`。它在 core 实现 public `exec` 和 private `run`；不再通过计划对象、摘要 Job 或第二个 Compactor Provider 执行。

| 对象 | 负责 |
|---|---|
| `Compactor` | Hook 固定流程、历史选择、摘要生成、预算、校验 |
| `ContextManager` / `DefaultContext` | 当前视图、请求投影、估算、原子应用与恢复 |
| `SessionStore` / `SqliteStore` | SQLite 完整历史、事务、会话恢复与 fork |
| `ModelProvider` / `UsageTracker` | 本次选定模型与统一用量记账 |

Compactor 只保存选定的依赖。候选消息、滚动摘要和游标是一次执行的局部变量。ContextManager 不保存第二份模型、Hook 或计量绑定。

## 入口与流程

手动、接近容量和 provider 明确报告上下文溢出，都经过同一个 `Compactor.exec`。

```text
exec
  ├─ 检查只读请求；低于阈值的自动请求直接返回
  ├─ PreCompact：可阻断，可追加上下文
  ├─ private run
  │    ├─ 取得 Hook 之后的上下文快照
  │    ├─ 选择旧历史，保留当前请求、当前指令与完整工具批次
  │    ├─ 分块更新同一份摘要
  │    ├─ 校验完整请求及节省量
  │    └─ ContextManager.save_summary：核对快照并原子提交
  └─ PostCompact：仅摘要已提交后执行
```

没有安全旧历史时返回不变，不发送 PostCompact 摘要事件。自动请求不需要维护时不执行任何压缩 Hook。PreCompact additionalContext 合并到摘要提示词，PostCompact additionalContext 保存为普通运行时上下文。

任何外部 Hook 和模型请求都不持有 ContextManager 的写锁，也不持有 SQLite 事务。提交时才取得写锁，核对消息身份；若期间有新消息或其他压缩，丢弃过期候选，保持新消息原样。

## 模型看到的内容

```text
冻结的系统提示词
+ 更新后的工作摘要
+ 当前有效的工作区指令原文
+ 最近原始消息、当前用户请求及附件
```

`StoredMessage.context_key` 表示可替换的权威运行时上下文。相同 key 只投影最新一条，当前条目不进入摘要。工作区指令用路径组成 key；旧会话的 `[Instructions: path]` 消息在读取时补出兼容 key，不改写历史正文。普通 Hook、事件和用户消息没有这个 key。

当前用户请求通过存储的 `summary`、`runtime_context` 与 role 判断，不通过用户正文前缀判断。带附件和 recall 的用户消息保留其实际模型内容。工具调用和所有并行结果作为不可拆分的一组；未闭合的组不会进入摘要。长任务可以压缩已闭合的旧工具批次，同时保留触发任务的用户请求。

## 摘要

历史先序列化为文本材料，不向摘要模型提供可执行的原生工具调用。工具关闭。旧图片只给媒体类型、名称和位置占位，不打开位置、不猜测未见内容。

每次请求明确分成 previous-summary 和 conversation。第一块使用上一份已提交摘要，后续块使用本次滚动摘要；完成后只提交一次。上一份摘要的原文不会再次混进 conversation。

提示词固定要求：目标、约束与决策、已完成及验证、当前工作、下一步、文件与命令。保留精确标识，后来的事实覆盖旧事实，删除过时判断，保留未完成目标。`custom_instructions` 可以指定本次摘要重点。

## 预算与计量

容量由本次 `ModelProvider.token_budget()` 提供。正常请求预算包含 system、messages、工具 schema、媒体和输出预留，并扣除 safety_margin。可靠的实际输入用量可以校准匹配的请求前缀；模型、工具或前缀改变时失效。

摘要使用独立的输出预算：目标长度的四倍作为包含推理和格式的余量，且不超过本次模型的普通输出预算。通过同一个 ModelTokenBudget 解析器重算摘要输入容量，不重复预留普通回答的巨大输出空间。

超长历史使用有界滚动分块。provider 报告摘要请求溢出时缩小块预算；不可拆分的组放不下则失败，不丢弃组内消息。请求的共享计数器记录所有尝试，默认最多八次；主循环对这些调用计数，但压缩不消耗业务 step。

每次已付费响应先进入共享用量记录和 UsageTracker，然后才检查摘要或提交；空摘要、截断、事务失败也计量。记账重试复用 UsageEvent 身份，不重新调用模型。用量转换统一使用 core `model::usage`。

## 可靠提交

摘要必须非空、未因输出限额截断，且完整候选请求满足输入预算及最小节省量。保护内容本身太大时明确报错，不宣称压缩成功。

ContextManager.save_summary 取得写锁后先核对快照，再经 SessionStore.save_summary 直接传入来源 ID、摘要和提交进度。源消息 ID 是事务参数：旧消息标为 compacted，摘要作为新行插入，在同一个事务里提交。源集合可以不连续，不用一个粗略序号截断替代。原文不删除，restore 和 fork 使用已提交状态。

Pending / Started / Committed 区分未开始写入、提交状态未知和已确认持久化。取消不能撤销已经启动的 SQLite worker。未知状态下读取被 dirty 屏障阻止，必须恢复后继续。确认提交后恢复或 PostCompact 失败，通过 stop_reason 报告，保留摘要，不能重新执行压缩来冒充回滚。

自动溢出恢复受主循环重试上限约束，而且必须有实际压缩进展。不新增假的用户继续消息，不重放工具，不消费尚未入场的队列消息。

## 工具输出与自动触发

不再提供持久化剪枝策略。普通请求与摘要复用同一份工具输出预览，保留状态、头尾和回看路径；完整原文仍在 SQLite。旧数据库已有的剪枝标记仍按原规则读取，不新增这类标记。prune_enabled、prune_min_savings、prune_growth 配置已移除。

ContextManager 不保存自动维护冷却状态，也不向外暴露 note_maintenance 或维护后的 token 数。一次 Model.exec 的重试共享一个局部自动压缩标记，避免失败后对同一请求重复收费；新请求已经超过容量时仍必须先压缩。独立的显式压缩请求不依赖上一次调用的隐藏状态。

压缩结果用 CompactionResult.compacted 表示是否提交了摘要，不再另设执行动作枚举。提交直接传入来源 ID、摘要和暂存提交进度，不另设提交参数结构。DefaultContext 构造直接传 system、store、policy，不再包装 ContextServices。

## 源码

- `crates/yourai-core/src/compactor.rs`：唯一压缩执行对象及其配置、请求、结果。
- `crates/yourai-core/src/context_manager.rs`：投影、估算、原子应用协议。
- `crates/yourai-harness/src/context/`：视图、输出预览、恢复与原子应用。
- `crates/yourai-harness/src/storage/sqlite.rs`：持久事务。
- `crates/yourai-harness/tests/context.rs`：完整历史、分块、取消、Hook、并发追加、摘要预算与指令恢复。
