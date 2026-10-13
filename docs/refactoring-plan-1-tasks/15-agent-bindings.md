# 15 · 核对宿主依赖更新权限

类型：条件性检查。状态：待办。依赖：无。遵守[任务索引](README.md)的全局约束。

**对象**：`SessionManager.agent`、`Agent.update/set_*`、`AgentBuilder`、`ProviderSnapshot`。见 [context.rs](../../crates/yourai-core/src/context.rs)、[runtime/mod.rs](../../crates/yourai-core/src/runtime/mod.rs)。

**原因**：独立 Agent 需要热更新，但宿主的 identity/store 替换可能绕过一致性约束；尚未复现正常路径错误。

**怎么改**：区分独立组装与宿主允许替换的 slot，核对消费者；有证据才收窄公开范围或限定更新。

**验收**：明确允许/禁止更新及其时机；若调整，独立 Agent、受控模型切换和每轮快照测试保持通过。

**保持**：不改 controller，不把所有依赖都变成配置，不取消独立 Agent 的组装能力。
