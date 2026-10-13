# 05 · 缓存固定工具 schema

类型：改造。状态：待办。依赖：无。遵守[任务索引](README.md)的全局约束。

**对象**：`Tool`、`Tool.definition`、`permission::authorize`、`interaction::validate_schema`。见 [tool.rs](../../crates/yourai-core/src/tool.rs)、[permission.rs](../../crates/yourai-core/src/permission.rs)、[interaction.rs](../../crates/yourai-core/src/interaction.rs)。

**原因**：固定 schema 每次验证重新编译，读取 schema 还克隆完整工具定义。

**怎么改**：在现有 Tool 中共享惰性编译结果和错误描述；authorize 使用缓存验证，避免完整定义克隆。

**验收**：clone 共用编译结果；非法 schema 的错误类别、ToolStarted/PreToolUse 顺序不变；无 schema、输入改写、批准后修改和动态交互正确。

**保持**：不改成注册失败；不删除输入变化后的校验与硬策略复查。
