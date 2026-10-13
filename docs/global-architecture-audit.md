# 全量架构审计：职责、依赖与剩余工作

日期：2026-10-13。范围：当前工作区三个 crate 的源码、公开类型清单、实际调用点及相关测试。此次只读审计，未修改运行代码，未运行新测试；此前通过的测试不等于此次所有观察都已验证。

**结果：8 项有明确源码证据的收敛问题，6 项需要先明确语义或验证的观察。没有发现可仅凭当前证据立即删除的 struct/trait。controller、客户端句柄、订阅及 Server 按用户要求暂停，不纳入当前实施队列。**

## 范围与方法

源码范围为 core 46 个、harness 49 个、TUI 36 个 Rust 文件，共 131 个；包括专用测试文件和内联测试，共 43,870 行。扫描文件树、模块入口、声明及依赖，再沿配置、执行、审批、存储和展示调用链核查。审查覆盖全部模块，不代表对每个分支做了运行验证。

生产类型清单仍为：

| crate | struct | trait | enum | 合计 |
|---|---:|---:|---:|---:|
| yourai-core | 96 | 16 | 35 | 147 |
| yourai-harness | 79 | 0 | 22 | 101 |
| yourai-tui | 54 | 0 | 17 | 71 |
| 合计 | 229 | 16 | 74 | 319 |

名称及位置见 [完整类型清单](current-type-inventory.md)。这些是声明数，不是待删除数。确认问题按变化原因、状态所有者、实际依赖、Interface 的深度和删除后的复杂度归属判断，不按类型名字相似程度判断。

此次的 Explore 分工分别核查 core，以及 TUI/context/扩展/存储；主审查核查 harness 装配、模型、Hook、工具、安全和跨模块调用，统一复核结论。没有改动源码。

## 全局依赖与覆盖

当前依赖方向为 TUI → harness → core；TUI 也引用 core 的协议数据。core 没有反向依赖 harness 或 TUI。core 直接复用 genai 的聊天类型与错误，属于已有 SDK 选择，不是本轮确认的过度设计；是否替换 SDK 是另一项产品/兼容性决策。

| Module | 核查内容 | 当前判断 |
|---|---|---|
| Agent / AgentBuilder / ProviderSnapshot | 依赖绑定、热更新、每轮冻结、缺失依赖 | 生命周期差异成立，保留；宿主绑定可见性见 V4 |
| ExecutionOptions / SessionOptions / 配置文件 | 设置归属、默认值、覆盖、生效阶段 | 配置文件→装配映射见 A1；运行与装配生效边界见 V1 |
| Turn* / execution | 输入接纳、账本、取消、deadline、执行限制 | 基本成立；原始冻结设置与已解析限制有不同用途 |
| DefaultLoop | 顺序、重试、压缩调度、工具调用 | 没有第二份运行配置；保留策略与执行模板分工 |
| Tool / ToolProvider / ToolRegistry | 定义绑定、审批、Hook、schema、工具运行 | 固定 schema 重复编译见 A6；Tool 执行包装保留 |
| permission / interaction / SecurityProvider | Hook 更新输入、审批回复、schema、硬限制复查 | 复查有实际安全语义，不能因执行两次就删除 |
| files / shell / web tools | 路径、输入解码、取消、输出截断、进程/HTTP资源 | 业务与资源 guard 保留；审批描述与实际路径复查不同 |
| GenaiModel / MeteredModel / ModelBudget | SDK 适配、容量、超时、计量、准入和日志 | 实际计量对象保留；准入层 SQL 泄漏见 A7 |
| ContextManager / DefaultContext / Compactor | 历史、请求投影、校准、预算、摘要提交 | 投影与模型压缩分工成立；策略与校准见 V1/V2 |
| HookManager / Handler / wire / outcomes | 注册、编译、并行执行、协议解释、聚合 | 前几批问题已收敛；模式编译所有权见 A8 |
| SessionManager / SessionLease / SessionTurn | 活会话、队列、账本、fencing、关闭与恢复 | 不删除拥有实际一致性职责的层；去重容量见 V5 |
| Workspace / backend | 指令、cwd、Git、watch、Hook与事务 | 领域与具体后端分工成立；watch重启语义见 V6 |
| TaskManager / TaskTool | 领域转换、持久化、缓存发布、JSON工具协议 | 当前分工成立；版本与列表快照不下放到UI |
| Subagent / SubagentFactory / SubagentTool | 创建、运行、交互、继续、清理、工具协议 | 仍有 ToolContext 依赖泄漏，见 A5 |
| MemoryManager / LocalMemory / memory hooks | 查询、分类、后台记忆、生命周期映射 | 实际领域操作保留；同步存储调用见 A4 |
| SkillProvider / LocalSkills / Table | 目录、加载、JSON文档与修订CAS | 共享CAS封装成立，异步执行边界见 A4 |
| SessionStore / SqliteStore / SessionCatalog | 租约、历史、状态、输出、任务、迁移 | launcher 接口泄漏见 A3；宽接口见 V3 |
| UsageTracker / LocalUsage / RequestLog | 计量事件去重、有序写入、flush、错误计数 | 不与通用Table合并；SQL归属见 A7 |
| TUI main / launcher / models | CLI、首次装配、选择候选、会话目录 | 后端规则残留见 A1/A2/A3 |
| TUI session / app | 命令发送、事件投影、回复与状态显示 | 会话控制已在后端；controller演进暂停 |
| TUI Draft / Overlay / Editor / selection | 本地输入、选区、剪贴板、外部编辑器 | 界面交互成立，不是应迁往后端的领域控制 |
| TUI Renderer / Presentation / Canvas / cache | 布局、动画、节流、终端提交与缓存 | 改变原因不同，保留；不能仅因类多合并 |

## A：确认的收敛问题（8 项）

“确认”表示职责/重复路径有证据，不表示已经证明线上故障。优先级 P1 表示先处理边界或正确性风险，P2 表示明确但可后置的 Locality/重复工作问题。

### A1 · P1 · 首次启动的后端配置装配仍在 TUI

证据：[main.rs:185](../apps/yourai-tui/src/main.rs#L185) 到首次 Harness::open，手工把 context、steps、memory_search_limit、prompt、extensions、trusted_shell、resume 等转换为 SessionOptions。

问题：CLI读取和主题选择属于UI；如何用这些选择装配会话属于后端应用规则。另一个UI需要复制同样映射，Interface 泄漏装配顺序。

建议：把共同映射与装配规则集中在已有后端 Module；TUI只传解析后的选择及必要覆盖。不要新增一套“统一配置类”，不要为该映射新建管理器。

保留：CLI参数的优先级、cwd来源、主题/终端检测；初次装配与恢复的lease/失败清理不重复实现。

验收：两种调用入口用同一后端映射得到相同会话设置；UI里不再逐字段填写后端运行策略。

### A2 · P1 · 模型候选规则在前后端重复

证据：[models.rs:26](../apps/yourai-tui/src/models.rs#L26) 遍历 provider/models/variants、判断 disabled、补当前模型；[config.rs:351](../crates/yourai-harness/src/config.rs#L351) 的 resolve 又判断 disabled 并拒绝不可用变体。

问题：后端已经保障disabled不可执行；UI再独立解释同一规则，候选查询与实际解析容易演化为两套逻辑。不是当前已证明能执行禁用模型的漏洞。

建议：已有 Config 提供共同候选/选择规则，TUI只投影 label、排序和选择状态。候选数据不必连显示结构一起移到core。

保留：变体覆盖、effort继承、当前隐式模型、实际resolve的最终校验。

验收：候选集合与解析可用性一致，配置规则测试由后端拥有，UI测试只覆盖展示。

### A3 · P1 · launcher 直接依赖具体存储入口

证据：[launcher.rs:22](../apps/yourai-tui/src/launcher.rs#L22) 引入 SessionStore，[launcher.rs:32](../apps/yourai-tui/src/launcher.rs#L32) 调 catalog.store.list_sessions；[storage/mod.rs:16](../crates/yourai-harness/src/storage/mod.rs#L16) 暴露 Arc<SqliteStore>。

问题：UI必须知道catalog背后的数据库及Store协议。目录查询没有形成能隐藏持久化细节的 Interface。

建议：利用已有 SessionCatalog 封装实际目录查询/初始化规则，UI接收SessionMeta。不能仅用无行为getter换名；要让UI不再承担存储选择与初始化知识。

保留：UI列表排序、过滤和行展示；原存储错误与迁移行为。

验收：launcher不导入SessionStore、不访问具体store；查询行为和空列表体验不变。

### A4 · P1 · 异步领域/策略接口直接调用同步 SQLite

证据：[memory/mod.rs:15](../crates/yourai-harness/src/memory/mod.rs#L15) 的async方法与 [skills/mod.rs:10](../crates/yourai-harness/src/skills/mod.rs#L10) 的BoxFuture直接调用同步 [table.rs:21](../crates/yourai-harness/src/storage/table.rs#L21)。[security/mod.rs:113](../crates/yourai-harness/src/security/mod.rs#L113) 的权限更新及check_tool_call也直接读写状态。SQLite使用标准Mutex/同步事务，[sqlite.rs:32](../crates/yourai-harness/src/storage/sqlite.rs#L32) 配置5秒busy timeout；已有blocking-pool入口在 [sqlite.rs:130](../crates/yourai-harness/src/storage/sqlite.rs#L130)。

问题：async外壳不会让同步存储变成非阻塞。实际锁竞争或磁盘等待可能占用Tokio工作线程。确认调用链，尚未测量现实延迟或断言所有请求都会阻塞5秒。

建议：统一规定同步Store访问的执行边界，沿扩展与策略路径复用现有owned/blocking机制。不要只逐个新增async包装，也不要因此立即拆出多个Store trait。

保留：CAS、存储lease、取消后已接受写入的完成/回报、缓存仅在持久化成功后发布。

验收：制造锁竞争时其他会话任务仍能推进；取消/关闭不会留下未监督写入或错误缓存。

### A5 · P1 · 子会话领域入口仍依赖工具执行上下文

证据：[subagent.rs:7](../crates/yourai-core/src/subagent.rs#L7) 的SubagentFactory.create与 [subagent.rs:36](../crates/yourai-core/src/subagent.rs#L36) 的Subagent.exec直接要求ToolContext；内部用其ask/emit_progress；[collaboration/tools.rs:93](../crates/yourai-harness/src/collaboration/tools.rs#L93) 的适配器原样传入。工厂又从tc读取providers/cwd/security/sandbox。

问题：工具JSON协议已移走，但非工具入口仍需要伪造工具身份和进度协议。领域操作与工具交互的Seam尚未完全正交。

建议：先明确领域实际需要的取消、依赖与交互能力，将工具call身份/进度包装留在现有Adapter；不要先发明通用Context类或新的Executor。

保留：父activity、ChildCleanup、父子登记、SubagentStart/Stop、继续上限与取消关闭监督。

验收：工具调用与普通程序调用能使用同一子会话生命周期，不要求普通调用方伪造工具call_id。

### A6 · P2 · 固定工具 schema 被重复编译，读取时克隆整个定义

证据：Tool已在 [tool.rs:108](../crates/yourai-core/src/tool.rs#L108) 缓存不可变definition；[tool.rs:122](../crates/yourai-core/src/tool.rs#L122) 每次克隆完整定义；authorize初始/批准后读取schema并调用 [interaction.rs:96](../crates/yourai-core/src/interaction.rs#L96)，每次jsonschema::validator_for重新编译。

问题：重复的是固定schema的编译和定义克隆，不是重新校验变更后的输入。尚未做性能测量，不承诺具体加速幅度。

建议：在现有Tool内统一拥有固定schema的验证结果/只读访问；动态交互schema独立处理。不为此创建新的验证器包装类。

保留：Hook改写输入后验证、批准后的输入验证、硬策略复查、非法schema的错误语义。

验收：固定schema不重复编译；所有实际输入变化仍被校验，错误与拒绝行为不弱化。

### A7 · P2 · 模型准入对象包含数据库表和SQL编码知识

证据：[model/control.rs:72](../crates/yourai-harness/src/model/control.rs#L72) 的RequestControl.start/finish拼model_requests SQL、rusqlite参数并调用store.with；[request_log.rs:47](../crates/yourai-harness/src/storage/request_log.rs#L47) 只接收任意SQL写入闭包。

问题：准入/冷却状态与请求日志持久化有不同变化原因，修改schema必须编辑模型准入Module。RequestLog目前深度主要在有序队列，持久化语义泄漏给调用者。

建议：把日志SQL、字段编码与持久化结果归到已有RequestLog/storage Module，模型层只报告请求开始/结束。保持现有后台队列，不另建日志Service或通用Repository。

保留：Drop路径非阻塞入队、请求ID、开始/结束顺序、flush确认与错误计数。

验收：模型准入代码不包含SQL或rusqlite参数；故障、取消、flush与既有日志一致。

### A8 · P2 · Hook条件/URL模式的编译与生命周期归属分散

证据：注册项仍保存if_condition字符串，[runtime.rs:113](../crates/yourai-harness/src/hooks/runtime.rs#L113) 只复制；dispatch经 [runtime.rs:447](../crates/yourai-harness/src/hooks/runtime.rs#L447) 解析，再构造正则字符串。[http.rs:268](../crates/yourai-harness/src/hooks/http.rs#L268) 每次重建URL表达式。两者依赖 [matcher.rs:76](../crates/yourai-harness/src/hooks/matcher.rs#L76) 的进程级静态缓存，注册注销后缓存不会随对象释放。

问题：编译后的配置状态没有由配置注册项/handler统一拥有，运行路径还在解释固定设置。未证明当前缓存已经产生可观测内存故障。

建议：在注册/handler创建阶段处理固定模式，实际handler持有必要编译结果，取消/注销跟随其生命周期。不要新建全局缓存管理器。

保留：if工具条件与URL主机通配的不同语义；URL主机通配不能跨DNS label，不能为了复用一个函数弱化规则。

验收：运行路径不重新解释固定配置；注销可释放对应状态；非法条件的处理语义先明确，匹配与HTTP限制回归通过。

## V：先明确语义或验证（6 项）

| ID | 观察及证据 | 为什么暂不改 | 下一步证明/决定 |
|---|---|---|---|
| V1 | ExecutionOptions混合生效阶段；SessionManager.set_config只替换内存（runtime/mod.rs:143）；DefaultContext另存context_policy（context/mod.rs:34），Compactor从context.policy读取（compactor.rs:406）；模型切换已同步两者（assembly.rs:316） | 已明确set_config不重新装配启动资源。正常模型切换无此问题；不能称所有set_config都错误 | 给所有字段列出生效阶段，决定context_policy属装配规则还是可变会话策略；然后确定唯一读取点 |
| V2 | DefaultContext.replace/reload恢复records/system/index，未恢复observation（context/mod.rs:52）；新append才获取观测（:118），估算依赖它（:280） | 可能有意仅信任当前运行观测；现有测试只覆盖append后校准 | 验证重开同一历史的校准行为，确定是否需要持久化/恢复观测 |
| V3 | SessionStore同时含租约、KV、输出、任务、历史、目录等（core/session.rs:61） | 相同lease/fencing与事务边界是真实约束，没有第二实现受宽接口拖累的证据 | 先列每个调用方的能力需求；有真实独立Adapter后再决定是否拆trait |
| V4 | 宿主公开agent()，Agent公开update/set_*；启动时核对身份/存储（runtime/mod.rs:230），之后仍可能直接替换 | 独立Agent必须允许组装/热更新；正常宿主路径已有操作guard，没有复现非法重绑定 | 明确宿主允许替换哪些slot，核查对外承诺，再考虑可见性与一致性约束 |
| V5 | RuntimeEvents.seen与持久化Journal.seen没有明显回收界限（runtime_event.rs:16；runtime/mod.rs:74） | 去重保护重投递与恢复。直接清空会破坏正确性 | 确认事件重放窗口、会话寿命与增长量，再定义回收策略 |
| V6 | Workspace的取消token不可复用，结束task handle可能仍Some（workspace/mod.rs:70、142） | 现有调用为一次启动/关闭，未发现合法重启调用 | 明确watch生命周期是否支持重启；若单次则收窄/说明接口 |

V1的字段阶段已核对：workspace_enabled/instruction_paths主要启动读取；input、附件、重试、执行超时每轮冻结；allow_background_wake、max_followups与宿主cleanup/hook timeout在宿主操作读取当前设置。不要将这些不同时间尺度硬塞成“所有模块随时读取同一个可变Config”。

## 明确保留与低优先级事项

| 类型/函数族 | 保留原因 |
|---|---|
| AgentBuilder / Agent / ProviderSnapshot | 装配草稿、可热替换拥有者、每轮冻结依赖；合并会丢掉真实时间边界 |
| TurnOptions / TurnLimits / TurnInfo / TurnHandle / TurnContext / Turn | 调用选项、约束、身份、监督句柄、借用交互与执行账本不同；不因Turn前缀相同合并 |
| Tool / Model / Compactor执行包装 | 集中Hook、审批、取消、错误恢复、提交；删除会把复杂度重新散落到调度器 |
| SessionLease / SessionTurn / Activity / ChildCleanup /进程guard | 对应durable fencing、结算、关闭监督、子会话和进程清理；不是空壳 |
| Task / TaskManager / TaskTool | 数据、领域转换和JSON Adapter不同；保持一个缓存发布点 |
| LocalMemory / LocalSkills / Table | 两类领域协议复用实际CAS文档机制；薄的Adapter不等于多余 |
| MemoryHookAdapter | 过滤会话、分页消息、排除runtime/summary、映射生命周期，隔离两种协议 |
| LocalUsage / RequestLog / ModelBudget / RequestControl / Attempt | durable计量、有序日志、累计观测、provider pacing、请求结算分别不同；A7只调整SQL归属 |
| ModelLimits / ModelTokenBudget / ContextPolicy | 原始容量、解析后的请求预算、维护策略不同；不能统一成一个万能limits/config |
| ApprovalDecision / PolicyDecision | 工具级可询问审批与命令/文件级硬限制不同 |
| 外部Hook wire / HandlerConfig / core outcome | 配置、外部输出协议和内部执行语义分别演化；保留适配 |
| 请求/快照/序列化数据struct | 数据结构不自动构成行为抽象层；不为减数字强行合并 |
| Renderer / Presentation / Canvas / TimelineCache | 布局、提交节奏、帧数据和缓存不同变化原因 |
| Draft / Overlay / Picker / Selection /本地UI jobs | 操作交互和展示；不能把UI所有分支都搬到后端 |

额外低优先级清理：ModelBudget名称/部分“hard limits”注释与当前观测/限流能力不完全匹配；公开便利函数如ToolRegistry.has/count和CompiledMatcher.compile缺少生产调用，但没有据此删除外部API。execution/workspace/runtime再导出路径需要与公共兼容承诺一起考虑。prompt.prepare的8参数虽长，当前都是单次启动提示的真实输入，暂不新增builder。

Table的CAS冲突不会自动重试，这是明确拒绝覆盖的语义，不是已确认bug。审批后再次校验输入、DNS/路径在实际执行前复核、取消后清理宽限也各有不变量，不计入待删除清单。

## 后续建议顺序与完成标准

此次仅提出候选，不授权或启动实施。controller/Server留在 [独立计划](server-plan.md)，不与下面工作混在一起。

1. 先明确V1配置字段生效阶段，避免后续把真实生命周期差异误合并。
2. 处理A1/A2/A3，把UI中的后端规则与存储知识收敛到已有Module；不新增统一配置或管理器。
3. 单独处理A4同步存储执行边界，先制定取消/关闭/持久化验证，再迁移。
4. 处理A6与A7两处明确的重复工作/持久化泄漏，分别验证工具授权和日志结算。
5. A5需要先明确最小领域交互语义再实施；A8处理固定配置编译所有权。两项不以预建框架为目标。
6. V2—V6只有在证据/语义明确后才转成实施项；最后清理低优先级注释、API与文档。

当前可量化的是8项确认候选，不是8个提交或8个必删类型。6项观察可能最终判为保留，不能提前算成确定修改量。完成标准是上述清单逐项有保留/调整的依据、唯一状态归属和对应验证，随后做相关整体验证；类型总数只记录结果，不作为验收目标。
