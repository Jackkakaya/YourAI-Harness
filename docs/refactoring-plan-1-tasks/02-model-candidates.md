# 02 · 统一模型候选规则

类型：改造。状态：待办。依赖：无。遵守[任务索引](README.md)的全局约束。

**对象**：`Config.resolve/effective_effort`、TUI `model_choices`、`ModelChoice`。见 [config.rs](../../crates/yourai-harness/src/config.rs)、[models.rs](../../apps/yourai-tui/src/models.rs)。

**原因**：TUI 自己判断 disabled 和隐式模型，后端 resolve 又判断可用性，规则重复。

**怎么改**：由 Config 提供纯候选查询并复用选择校验；删除 TUI 中的可用性规则，保留 label、排序和选择。

**验收**：disabled、默认/变体、隐式模型、effort 结果一致；候选查询不创建 Client、不发请求；模型测试与 smoke_models 通过。

**保持**：不改变实际 resolve 的最终校验和冻结 controller 的调用。
