# 08 · 验证后台存储与关闭顺序

类型：前置验证。状态：待办。依赖：无。遵守[任务索引](README.md)的全局约束。

**对象**：`Table.read/update`、`SqliteStore.with/run`、`SessionLease` 释放路径。见 [table.rs](../../crates/yourai-harness/src/storage/table.rs)、[sqlite.rs](../../crates/yourai-harness/src/storage/sqlite.rs)、[runtime/mod.rs](../../crates/yourai-core/src/runtime/mod.rs)。

**原因**：异步接口直接调用同步 SQL；简单 spawn_blocking 又可能让取消后的写入脱离关闭语义。

**怎么改**：分类读、维护写和 fenced 写；用 owned 数据试验现有 Table 的后台执行，核对事务与 lease 释放顺序。不能调用 core 的私有 blocking 或给全部扩展补 host。

**验收**：单线程 Tokio 锁竞争时定时任务推进；取消/关闭/owner 替换得到一次提交或明确错误；CAS 不重放。未通过则 09、10 延期。

**保持**：不撤销已开始 SQL，不 downcast Store，不把一次后台 CAS 工作宣称为一个 SQL 事务。
