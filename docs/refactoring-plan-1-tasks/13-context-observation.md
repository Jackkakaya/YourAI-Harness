# 13 · 核对上下文校准恢复

类型：条件性检查。状态：待办。依赖：无。遵守[任务索引](README.md)的全局约束。

**对象**：`DefaultContext.replace/reload/estimate`、`View.observation`。见 [context/mod.rs](../../crates/yourai-harness/src/context/mod.rs)。

**原因**：恢复历史不恢复 observation，重开后校准可能不同；尚未证明这是缺陷。

**怎么改**：验证同一历史重开后的估算，明确观测是当前运行数据还是持久契约；仅在确有恢复要求时调整。

**验收**：给出保留/调整/延期及证据；若改动，覆盖模型或工具变化时旧校准失效，不能用不匹配观测。

**保持**：不为统一字段强行新增持久化数据，不顺手重新加载历史来更新配置。
