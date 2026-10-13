# 04 · 集中启动配置映射

类型：改造。状态：待办。依赖：无。遵守[任务索引](README.md)的全局约束。

**对象**：`Config`、`ResolvedModel.apply_to`、`SessionOptions`、main 装配段。见 [config.rs](../../crates/yourai-harness/src/config.rs)、[assembly.rs](../../crates/yourai-harness/src/assembly.rs)、[main.rs](../../apps/yourai-tui/src/main.rs)。

**原因**：TUI 逐字段装配后端设置，其他入口需要复制同样的映射。

**怎么改**：用现有 Config/ResolvedModel/SessionOptions 在后端完成共同映射；删除 TUI 的重复赋值，主/压缩模型各解析一次。

**验收**：默认值、CLI 覆盖、yolo OR 配置、steps/input/trust/扩展及恢复行为正确；check-config 和启动/恢复 smoke 通过。

**保持**：主题、argv 和终端判断留 UI；check-config 仍早返回，不打开会话存储或加载启动资源。
