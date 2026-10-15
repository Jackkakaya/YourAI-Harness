# 02 · 模型候选与展示边界

类型：改造。状态：已实现。依赖：无。遵守[任务索引](README.md)的全局约束。

**职责**：Config 提供配置数据；TUI 组织模型菜单、标签、排序和当前隐式条目；model 处理模型/provider 绑定、参数继承、variant 可用性、effort 和客户端构建。

**改动**：模型模块提供 list_models(config, available_only)，返回模型、variant、effort、available。删除 Config::model_data 和跨模块的原始 JSON 查询接口；绑定与参数解析保留为内部实现。TUI 只负责标签与排序。

**边界**：查询内部一次持锁读取，不复制整块配置；可用性仅指有效 provider 关联和启用 variant，不验证凭据或网络。全部模式包含不可用条目，可用模式过滤。菜单不构建客户端，没有额外配置状态或管理类。

**验证**：菜单测试迁入 TUI，覆盖排序、默认/变体、隐式模型、provider 缺失、空 ID、effort 和并发一致性。后端选择、构建、配置与 controller 回归保留；严格 Clippy、格式与 smoke_models 验证。

**保持**：controller 和运行资源提交保护未改动，客户端构建继续校验认证、地址、额度和生成参数。

模型配置接口收敛为 list_models/select/resolve。列表携带生效 effort 与价格；删除 effective_effort/pricing/resolve_compaction 公共函数。模型额度与覆盖超时直接放入 ModelConfig，旧 limit 只用于输入迁移。
