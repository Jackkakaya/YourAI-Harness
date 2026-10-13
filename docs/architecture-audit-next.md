# 配置之外的模块职责审查

日期：2026-10-13。此报告记录改造前的审查；其中任务工具协议、任务查询和 UI 控制职责现已迁移，实施结果见 [后端与 UI 分工](backend-ui-separation.md)。基于当前工作区源码，只审查并记录，未修改运行代码；未因本次文档审查重新执行测试。

判断依据：按变化原因、状态所有者、Interface 与 Implementation 的深度，以及删除后复杂度是否消失来判断。不能因为类型名字相似就合并，也不能因为删除一个 struct 就认为模块已经正交。

## 有明确证据的改进点

### 1. 任务与子会话操作混入工具协议适配

Module：`crates/yourai-core/src/tasks.rs` 的 TaskManager；`crates/yourai-core/src/subagent.rs` 的 Subagent。

两个对象都直接实现 ToolProvider，同时包含工具名称、JSON schema、JSON 字段读取、安全描述及领域操作。任务持久化、Hook、关闭结算与工具输入协议有不同变化原因，但目前必须编辑同一个 Module。

建议：领域操作保留在原 Module，把工具 schema、输入解析及结果编码移动到 harness 的具体工具 Adapter。Adapter 调用 create/complete/idle 或 Subagent.exec，不复制 Hook、事务或生命周期逻辑。不新增通用工具工厂和协议框架。

收益：工具协议调整只影响 Adapter；TUI、程序调用和未来服务命令继续使用同一份领域行为，提高 Locality。删除测试表明应移动协议职责，而不是删除 TaskManager 或 Subagent；否则资源结算和领域约束会散落到调用方。

判断：**Strong**。真实协议 Seam 已存在（ToolProvider），不需要再发明接口。拆分可能增加一个薄的具体 Adapter 类型；应评估其隔离协议的价值，不以类型总数作为验收。

### 2. 任务查询的协调逻辑仍在 TUI

Module：`apps/yourai-tui/src/ui/session.rs` 的 Controller::tasks_since；`crates/yourai-core/src/tasks.rs` 的 version/list。

Controller 仍通过 `runtime.h.tasks` 获取 TaskManager，先读取 version，再读取 list，自己判断是否刷新。任务能力的发现路径在 Harness，实际生命周期与初始化又由 SessionManager 负责。调用方需要知道两层对象及缓存更新规则。

建议：由会话提供任务变化查询；TaskManager 负责取得一致的版本与列表，Controller 只消费查询结果。使用现有对象和简单返回值即可，不新增 TaskService 或订阅框架。先核查其他调用点，避免把每个 getter 都变成新的转发接口。

收益：任务状态所有者封装版本协调，TUI 无需访问 Harness 的任务字段；未来第二个客户端可以复用行为。当前分开读取主要可能产生版本与数据不对应及额外刷新，本次没有证据表明它必然丢失任务，不能夸大成已确认的数据丢失 bug。

判断：**Strong** 对接口泄漏；一致快照的实施细节需结合现有更新锁审查。

### 3. 执行生命周期仍由 TUI Runtime 持有

Module：`apps/yourai-tui/src/ui/session/runtime.rs` 的 Runtime，和 core SessionManager。

Runtime 持有 driver、submitter、compact 任务及取消状态；SessionManager 同时负责会话执行和结算。当前单客户端进程内架构可以工作，但界面层仍知道如何启动、重启与关闭执行任务。

建议：作为已记录的共享 Server 里程碑，将唯一执行任务与命令处理归服务侧，客户端只处理命令响应与显示订阅。不能先删除这些 JoinHandle：它们当前承担真实任务所有权和退出清理。

收益：客户端连接生命周期与会话生命周期分离；双客户端不重复驱动执行。

判断：**Strong**，但这是已知事项，不是本次发现的新问题；需要完整迁移和验证，不适合通过几次 getter 改名实现。

## 值得澄清，但暂不合并或删除

| Module / 类型 | 源码证据 | 判断 |
|---|---|---|
| HookJsonOutput、HookSpecificOutput 与 core Hook outcome | wire_output 定义外部 camelCase 协议；runtime/outcomes 集中解释与聚合，core outcome 表达内部执行语义 | 保留。协议适配与内部行为正交；直接复用同一种结构会让 core 依赖外部协议演化 |
| ModelLimits 与 ModelTokenBudget | 前者描述模型容量，后者校验生成上限并计算请求输入预算 | 保留。原始容量与已解析预算不同，不能只按 limits/budget 名称合并 |
| ModelBudget、BudgetSnapshot、RequestControl、Attempt | ModelBudget 共享累计指标与请求时间；RequestControl 管 provider pacing/cooldown 与请求日志；Attempt 结算成功、失败及被丢弃的请求 | 不删除真实结算对象。ModelBudget 名字容易让人误以为它包含费用预算上限；当前 RequestPolicy 只有 rpm 和 cooldown，命名/注释应进一步澄清。Worth exploring |
| Renderer、Presentation、Canvas、TimelineCache | Renderer 负责布局、命中区域、动画；Presentation 节流并提交帧；Canvas 是像素结果；TimelineCache 缓存文本布局 | 保留当前拆分。合并会混合布局变化与终端写入节奏；尚未发现可直接删除的独立包装 |

小的确定冗余：hooks/runtime/outcomes 的 parse_json_value 把 Value 转为字符串，再调用 wire_output 的文本解析器。可以让协议解析 Module 提供 Value 入口，文本入口先解码再复用；这只是去掉重复编解码，不需要增加解析器对象。

## 建议顺序

1. 先核查并收敛任务查询，修复真实的上层 Interface 泄漏。
2. 移动任务/子会话的工具协议 Adapter，保持领域行为和已有验证不变；验证程序调用与工具调用走相同 Hook/事务。
3. 清理 Hook 的重复编解码，澄清模型用量与限流命名；不进行无证据的类型合并。
4. 独立完成共享服务迁移，验证唯一执行、重连、审批和租约 fencing。

当前仍为 219 个 struct、16 个 trait、72 个 enum，共 307 个生产源码类型声明。本次审查没有删除任何类型。


## 2026-10-13 补充审查：会话工厂

发现 runtime::restore 的 10 参数 Interface 重复展开已有 AgentBuilder；private open 又逐项装回 builder。已改为直接传 AgentBuilder，ExecutionOptions 单独传递，详见重构计划第 11 批。没有新增类型；子会话 compaction model 的装配也集中到启动前。

其他审查结论：TaskManager 的领域行为、Table 的 CAS 文档操作、LocalMemory 的记忆查询/管理、LocalSkills 的技能协议均有实际职责，不能因都是存储包装而合并。后续先审查 Hook 的重复校验与转换；controller 与 Server 重构按用户要求暂停。


## 2026-10-13 Hook 补充审查结果

内部事件已经类型化，却又转换字符串传入 outcomes 聚合；已改为直接传 HookEventKind。HandlerConfig 的超时校验与 Duration 转换分离，正且有限的极大数仍会 panic；已统一为验证后返回解析 Duration，注册时只处理一次。去掉独立 timeout getter 与单调用点 parse_json_value 转发，不增加对象。外部协议类型与内部 outcome 继续分离，命令文本兼容和 HTTP JSON 要求不强行统一。实施记录见计划第 12 批。
