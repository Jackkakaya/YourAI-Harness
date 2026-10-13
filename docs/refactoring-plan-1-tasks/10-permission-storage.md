# 10 · 迁移权限的同步存储路径

类型：改造。状态：待办。依赖：09 通过。遵守[任务索引](README.md)的全局约束。

**对象**：`PolicySecurity.check_tool_call/update_permissions/remember_tool_approval`、内部 `Policy`。见 [security/mod.rs](../../crates/yourai-harness/src/security/mod.rs)。

**原因**：异步策略直接执行同步状态读写；持久规则、临时批准和缓存发布需要明确结算顺序。

**怎么改**：分别处理持久规则、session_allow、hard_deny；复用验证后的存储边界，CAS 成功再发布缓存，避免标准 Mutex 跨不受控 await。

**验收**：硬 deny、记住批准、scope 拒绝、CAS 失败不改缓存、再次硬策略检查及锁竞争/关闭正确。

**保持**：不改变 YOLO 和审批语义，不迁移整个 core journal，不强行套通用 PolicyService。
