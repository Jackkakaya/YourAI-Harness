# 01 · 简化配置文件与共享读取

类型：配置与接口改进。状态：已实现，整体验证通过。

用户后续确定：常规配置集中一个 JSON，仅 prompt 独立。应用共享一份 Config，启动加载一次；应用内修改保存对应源再更新内存，外部编辑下次启动生效。不采用多层设置服务。

**实现**：唯一共享 Config 按 models/execution/security/ui/session/prompt/hooks 分类，分类字段在 Rust 和 JSON 中一致；分组只是数据，读写仍只有 config::update。Prompt 独立，Hook 支持内嵌或独立 JSON；JSON/SQLite 保存相同分类文档；旧平铺及内嵌 Prompt 无损迁移，重复和未知字段报错。模型、effort、YOLO、主题接入运行更新，执行类别用于每轮冻结参数。

**不变量**：保存失败不发布、模型准备失败不保存、正在执行的轮次不变、既有提示词冻结、审批/CAS/fencing 与关闭清理继续保留。历史和任务等领域数据仍在会话数据库。

字段、API 和验证结果见 [当前配置契约](../core-contracts.md#配置读写与持久化契约2026-10-13)。配置代码见 [config.rs](../../crates/yourai-harness/src/config.rs)。本次仅迁移 controller 的配置读写，不实施 Server 或订阅改造。
