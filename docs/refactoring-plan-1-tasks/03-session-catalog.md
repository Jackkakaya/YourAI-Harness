# 03 · 隐藏目录存储细节

类型：改造。状态：待办。依赖：无。遵守[任务索引](README.md)的全局约束。

**对象**：`SessionCatalog`、launcher `pick`、main 的 JSON 导入分支。见 [storage/mod.rs](../../crates/yourai-harness/src/storage/mod.rs)、[launcher.rs](../../apps/yourai-tui/src/launcher.rs)、[main.rs](../../apps/yourai-tui/src/main.rs)。

**原因**：UI 访问 catalog.store、SessionStore 和具体 SQLite 导入/路径接口。

**怎么改**：通过已有 Catalog/存储公开入口查询和导入；UI 只消费结果。导入直接复用后端实现，不能先构造会拒绝未导入旧 JSON 的 Catalog。

**验收**：空/已有列表、导入错误和备份保留正确；launcher 操作不变；生产 UI 不引用具体 SQLite 接口。

**保持**：不改 controller，不强制私有化所有 store 字段，不把列表排序搬入后端。
