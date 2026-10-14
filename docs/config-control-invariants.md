# 配置和会话控制约束

配置模块只定义五个 struct：Config、ProviderConfig、ModelConfig、PromptSettings、HookSettings。
执行、上下文、图片、额度与超时复用 core 类型。不存在 ConfigData、ConfigSection、ModelsConfig 或额外配置管理类。

Config 私有持有一份 JSON 文档与一把锁。所有对外读写通过有类型的 getter/setter；不提供公开 update 闭包、snapshot、锁守卫或可变字段。Arc<Config> 是唯一共享配置，SessionOptions 复制 Arc，不复制 Config。

- model、variant、compaction_model、主题、安全开关、会话路径直接放在根配置。
- providers 为 ID 到 ProviderConfig 的表。连接参数、请求头、RPM、冷却参数直接属于 ProviderConfig。
- models 为 ID 到 ModelConfig 的表。每个模型通过 provider ID 复用连接；价格与模型参数直接属于 ModelConfig，variants 只是参数覆盖表。
- execution、prompt、hooks 保留明确的数据内容，统一保存在同一个 JSON。
- setter 立即修改共享内存，save 显式原子保存完整 JSON。保存失败保留普通内存修改。
- Config::init 兼容旧分组、provider 内模型表、连接 options、独立 Prompt/Hook 文件；保存只输出当前结构，不写旧文件。

模型模块负责选择、参数合并、价格和客户端构建。运行切换先准备资源，再检查模型及上下文依赖是否变化；提交只修改选择与 effort 字段，保留无关并发修改。保存失败恢复这些字段并保持原运行资源。安全切换以相同方式保护原状态。

controller/state.rs 只保存运行寿命、当前 Harness、通道及准入状态，不持有第二份配置。所有状态改变通过命令或私有转换函数完成；展示快照不决定操作许可。每轮执行参数在开轮时冻结。

验证覆盖 JSON 迁移与 round-trip、字段类型检查、并发 setter、菜单一致性、模型选择失败、持久化失败回滚、共享执行参数及真实 TUI HTTP 请求。
