# 09 · 迁移技能与记忆存储调用

类型：改造。状态：待办。依赖：08 通过。遵守[任务索引](README.md)的全局约束。

**对象**：`LocalSkills`、`LocalMemory`、`Table`。见 [skills/mod.rs](../../crates/yourai-harness/src/skills/mod.rs)、[memory/mod.rs](../../crates/yourai-harness/src/memory/mod.rs)、[table.rs](../../crates/yourai-harness/src/storage/table.rs)。

**原因**：async/BoxFuture 外壳内直接同步读写，会在数据库等待时占用 Tokio 工作线程。

**怎么改**：先迁移 LocalSkills.register 一个写路径；验证后推广 Table 读写、其余技能和记忆操作，复用同一已验证执行边界。

**验收**：技能加载/目录、记忆查询/管理、peer CAS、取消及旧 owner 拒绝正确；锁等待不占用 Tokio 工作线程。

**保持**：保留排序、分类、时间戳和搜索限制；Table 无缓存，不新增缓存或执行管理器。
