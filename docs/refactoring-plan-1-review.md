# 重构计划 1 审查意见

> 配置后续调整：用户要求全面梳理读写边界并改进接口，原文保留 set_config 的计划已被 [任务 01](refactoring-plan-1-tasks/01-config-contract.md) 与 [当前配置契约](core-contracts.md#配置读写与持久化契约2026-10-13) 替代。

日期：2026-10-13。审查范围：原计划、全量审计和当前源码调用链。本次只审查和重写文档，不实施代码。controller / Server继续暂停。

结论：原计划的方向正确，但更接近审计清单，尚不足以直接指导实现。以下问题已在新版 [重构计划1](refactoring-plan-1.md) 修正。

| 级别 | 原计划问题 | 源码/实施依据 | 新计划处理 |
|---|---|---|---|
| P1 | 把245个类型和全部公开函数矩阵设为统一开工前置，缺少结束边界 | 已确认的schema缓存或日志归属不依赖其他所有公开API的结论 | 保留全量索引与逐类型分类；详细矩阵按涉及类型/函数完成，只阻塞依赖该决策的批次 |
| P1 | 存储批写“复用已有blocking/owned机制”，没有可调用入口及收尾归属 | core SessionManager::blocking 是pub(crate)；harness Table只有store/id/key，没有host/activity；Table也没有缓存 | 先验证读写/fenced store/lease释放顺序。优先在现有Table内用owned数据执行后台操作；关闭行为先通过gate测试，失败则阻止该批推广，不能给所有扩展补host |
| P1 | schema缓存可能隐含把执行错误提前为注册错误 | Tool::new、ToolRegistry::register不返回Result；非法schema当前在PreToolUse之后的authorize阶段报错 | 明确保留错误阶段，在现有Tool中共享惰性编译结果及错误；动态交互schema不共用该缓存 |
| P1 | 子会话只写“去掉ToolContext”，缺少Ask回复、断线和继承的契约 | Subagent把child Ask映射为parent工具提问；Factory读取冻结providers/cwd/security/sandbox；配置在创建时读取parent当前值 | 先写消费表，选择原生事件＋可选问答能力；Adapter拥有call_id和工具JSON。明确配置与provider的冻结时机，先验证普通调用和审批回路 |
| P2 | context_policy仍留作抽象的“确定归属”，没有默认目标 | set_config仅换默认值；DefaultContext.policy为当前实例策略；已有模型切换同时发布配置与provider | 推荐保留现有时机，明确默认值与有效实例快照的差异，不新增隐式热更新或配置管理器 |
| P2 | UI三项改造捆绑，候选查询可能每项构造ModelProvider | Config.resolve会构造SDK Client，UI枚举只需身份与选项 | 拆成候选、目录、启动装配三个批次；候选复用纯选择校验，不构造Client、不做网络请求 |
| P2 | 要求目录查询“不允许转发”可能制造假深度 | launcher通过Catalog创建目录，泄漏点是.store与SessionStore；不是自己手写SQLite初始化 | 认可已有Catalog的目录公开契约；不为接口增加虚构排序/业务，不立即私有化controller仍用的store字段 |
| P1 | 目录导入迁移可能引入循环前置 | SessionCatalog::new检测未导入旧JSON会拒绝，导入不能依赖先构造正常Catalog | 关联函数或storage公开入口直接复用导入实现；查询仍通过正常Catalog |
| P2 | 日志与Hook模式同批，验收范围太宽 | 两项状态、风险及调用链独立 | 分开实现、分开验证，日志不改变flush/取消语义；模式不合并URL与工具通配 |
| P2 | V项像必须实施的后续工程 | 去重、宽Store、单次watch生命周期均可能合理 | 每项输出保留/调整/延期结论，未确认问题不强制变成代码改动 |
| P2 | 每批验收混合模块测试和整仓全部检查 | 小范围纯映射与公共trait变化的影响不同 | 分局部、公开API、TUI和最终整体验收；明确现有测试入口与新增行为场景 |

额外修正：CLI的 `--import-json-sessions` 仍直接引用storage::sqlite和SqliteStore.path，是目录公开契约审查的相关入口；新版将其纳入同一范围，避免宣称启动层已经完全不知数据库。

保留的原则：不以删类型数量验收；不为接口隔离预建trait/Manager/通用Context；不删除审批后复查、快照、CAS、fencing和资源guard；不顺手重做已完成批次。

新版的“可实施”指每批具有文件、步骤、语义选择、前置门槛和验证；存储与子会话仍须先完成各自限定的行为验证，不能把文档方案当成已经通过测试的实现。

修订稿复核：core与UI/存储两路只读复核均完成。发现导入循环前置后已补充约束；同时明确check-config早返回、schema缓存只保存错误描述。复核确认的是计划一致性与实施门槛，不是运行行为已通过测试。
