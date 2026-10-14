# 01 · 简化配置文件与共享读取

类型：配置与接口改进。状态：已实现，整体验证通过。

当前契约：全部配置集中一个 JSON，应用与会话共享同一个 Arc<Config>，启动加载一次。getter/setter 内部加锁；setter 即时更新内存，save 显式保存主 JSON，外部编辑下次初始化生效。

**实现**：Config 统一 JSON 初始化与保存；各数据 struct 描述 JSON，共享字段通过 Config 直接 getter/setter，内部加锁；嵌套字段按 ID 修改，无通用访问对象。没有 snapshot、Clone、通用 update 或额外配置管理类。旧平铺字段、Prompt/Hook 分拆文件仅用于迁移读取。模型构建及校验保留在模型模块；执行类别用于每轮参数。

**不变量**：普通保存失败保留内存修改，模型和安全切换保存失败回滚对应分组；模型准备失败不保存、正在执行的轮次不变、既有提示词冻结、审批/CAS/fencing 与关闭清理继续保留。历史和任务等领域数据仍在会话数据库。

字段、API 和验证结果见 [当前配置契约](../core-contracts.md#配置读写与持久化契约2026-10-13)。配置代码见 [config.rs](../../crates/yourai-harness/src/config.rs)。本次仅迁移 controller 的配置读写，不实施 Server 或订阅改造。
