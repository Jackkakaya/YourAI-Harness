# 14 · 核对 SessionStore 是否需要拆分

类型：条件性检查。状态：待办。依赖：无。遵守[任务索引](README.md)的全局约束。

**对象**：`SessionStore`、`SqliteStore` 及任务/扩展/历史调用者。见 [session.rs](../../crates/yourai-core/src/session.rs)。

**原因**：接口覆盖多种存储能力，可能偏宽；但共同事务和租约有实际作用，不能仅按方法多判过度设计。

**怎么改**：列实际消费者需要的能力和共享事务；默认保留，只有真实独立实现/消费者证明收益才提出拆分。

**验收**：有明确结论；若拆分，调用方必要知识减少且 CAS/fencing/事务不弱化。保留也算完成。

**保持**：不预建多个 Store trait 或 Adapter，不把 SQLite 细节带入 core。
