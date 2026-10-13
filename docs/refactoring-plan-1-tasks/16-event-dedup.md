# 16 · 核对事件去重容量

类型：条件性检查。状态：待办。依赖：无。遵守[任务索引](README.md)的全局约束。

**对象**：`RuntimeEvents.seen`、运行 `Journal.seen`。见 [runtime_event.rs](../../crates/yourai-core/src/runtime_event.rs)、[runtime/mod.rs](../../crates/yourai-core/src/runtime/mod.rs)。

**原因**：seen 没有明显回收界限，长会话可能增长；同时它保护重投递和恢复，不能直接清空。

**怎么改**：明确事件重放窗口并观察增长，决定保留或有限回收；需要 Server 语义的部分明确延期。

**验收**：结论和适用条件明确；若回收，重复事件、未确认事件、恢复和旧 ID 重投递不破坏既定保证。

**保持**：不删去重保护，不继续 controller、订阅或 Server 改造。
