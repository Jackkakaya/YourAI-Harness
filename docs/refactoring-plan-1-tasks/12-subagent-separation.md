# 12 · 移除领域的工具上下文依赖

类型：改造。状态：待办。依赖：11 通过。遵守[任务索引](README.md)的全局约束。

**对象**：`Subagent`、`SubagentFactory`、`SubagentTool`、`ChildFactory`。见 [subagent.rs](../../crates/yourai-core/src/subagent.rs)、[tools.rs](../../crates/yourai-harness/src/collaboration/tools.rs)、[backend.rs](../../crates/yourai-harness/src/collaboration/subagents/backend.rs)。

**原因**：工具 Adapter 虽已独立，领域运行/装配仍读取工具身份和交互协议，分离不完整。

**怎么改**：按 11 的最小接口迁移 Factory/exec/run；工具包装和问答适配放 SubagentTool，删除旧签名和无行为搬运层。

**验收**：普通/工具调用、子审批、错误身份、Start 阻断、Stop 继续、父关闭期间创建及取消清理正确，无遗留子会话。

**保持**：不复制运行/继续/关闭流程；保留 ChildCleanup、activity 和父子登记。
