# 11 · 明确子会话交互接口

类型：前置验证。状态：待办。依赖：无。遵守[任务索引](README.md)的全局约束。

**对象**：`Subagent.exec/run`、`SubagentFactory.create`、`ToolContext`、`SubagentTool`。见 [subagent.rs](../../crates/yourai-core/src/subagent.rs)、[tools.rs](../../crates/yourai-harness/src/collaboration/tools.rs)、[backend.rs](../../crates/yourai-harness/src/collaboration/subagents/backend.rs)。

**原因**：领域入口要求 ToolContext，普通程序调用必须伪造工具身份；直接去掉它又会遗漏子审批和回复路由。

**怎么改**：列最小数据/能力，确定普通事件、可选问答和取消的签名；call_id/ToolProgress JSON 留 Adapter，验证真实 Rust 生命周期。

**验收**：普通调用无需 call_id；子 Ask 能得到回复，缺少交互能力明确报错；父取消/消费者断开确定收尾。未通过则 12 延期。

**保持**：保留冻结 providers、创建时父配置、有效 history 策略及显式覆盖，不把 ToolContext 换名成通用 Context。
