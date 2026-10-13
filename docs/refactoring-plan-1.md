# 重构计划 1（审查修订版）

> 配置后续调整：用户要求全面梳理读写边界并改进接口，原文保留 set_config 的计划已被 [任务 01](refactoring-plan-1-tasks/01-config-contract.md) 与 [当前配置契约](core-contracts.md#配置读写与持久化契约2026-10-13) 替代。

日期：2026-10-13。状态：计划已重新制定，尚未实施。审查结论见 [计划审查意见](refactoring-plan-1-review.md)，事实依据见 [全量审计](global-architecture-audit.md) 与 [类型清单](current-type-inventory.md)。

执行入口：[18 个简短任务文档](refactoring-plan-1-tasks/README.md)。本文件保留总体设计和完整验收要求；每份任务只列对象、原因、修改方式和验收，进度在任务索引记录。

本计划包含总体目标、目标职责、设计决策、依赖顺序、具体实现步骤、分批及整体验收。不是再次无期限扫描，也不是按struct数量减法改造。

## 一、总体规划

### 1. 目标与范围

| 用户要求 | 本轮具体目标 | 完成证据 |
|---|---|---|
| 所有类型职责正交 | 解释同概念多类型的真实状态/时间/协议差异，合并无独立职责的包装 | 全量类型分类＋重点族职责矩阵＋实际处置记录 |
| 公开函数正交 | 同一规则只有一个实现；调用者不拼内部顺序；局部case留在对应Adapter | 涉及公开API的消费者/副作用/调用顺序表，旧路径删除或明确保留 |
| 高内聚低耦合 | UI只展示和操作；SQL、配置解释、领域执行各有归属 | 依赖/调用点检查与行为测试，变化集中在其所属Module |

范围为core、harness、TUI，当前基线229 struct、16 trait、74 enum。已有重构继续保留；全量审计A1—A8是实施主线，V1—V6是决策/验证项。

**冻结范围：SessionController、SessionClient、SessionSubscription、controller运行流程及网络Server。不能修改它们以完成本计划。** 其他Module的公共接口改变仍需保证冻结代码编译；需要修改冻结代码的部分明确延期，不静默扩大范围。

不在本轮实现远程协议、事件恢复、全局万能Config、StorageExecutor、通用Context、通用Repository，或为抽象整齐而拆多个Store trait。

### 2. 目标职责与依赖

| 既有Module | 目标职责 | 不应知道的细节 |
|---|---|---|
| TUI main / launcher / models | CLI输入、选择、显示、操作发送 | 共同配置映射、变体可用性规则、SQLite表/具体模块 |
| harness Config / assembly | 解释文件设置、查询选择、装配依赖 | 终端事件、View、UI布局 |
| SessionManager / Agent | 会话默认设置与生命周期 / 依赖绑定和执行入口 | 具体SQL、终端显示 |
| DefaultContext / Compactor | 历史投影与有效策略快照 / 压缩操作 | UI状态、独立可变配置管理器 |
| Tool / permission | 固定定义与完整执行 / 审批与最终输入校验 | 每个工具的字段名和具体文件/命令算法 |
| Subagent / SubagentFactory / SubagentTool | 子会话流程 / 具体装配 / 工具交互适配 | 领域流程不要求工具call身份，Factory不承担运行与清理 |
| Table / LocalMemory / LocalSkills | CAS文档读写 / 记忆领域 / 技能领域 | Tokio线程被同步数据库长时间占住、UI |
| SqliteStore / SessionCatalog / RequestLog | fenced持久化 / 目录公开能力 / 有序请求日志 | 模型重试策略、显示规则 |
| RequestControl / Attempt | provider准入 / 请求结算 | SQL表名与字段编码 |
| Hook注册与handler | 配置编译、执行、结果解释 | 不属于自己的全局缓存生命周期 |

依赖保持 TUI → harness → core；协议数据可由TUI引用core。各Module通过已有Interface/Adapter协作，不再添加一层无状态Manager。

### 3. 本计划选定的默认决策

这些是拟实施契约，尚未经过本计划测试；如果验证与现有行为冲突，只阻止相关批次。

1. **配置所有权继续在SessionManager。** AgentBuilder只表达依赖，Agent不持有第二份配置。每轮已冻结设置不受后续更新影响。
2. **保留set_config当前时机。** 它替换内存默认值，不重装workspace、指令或上下文。ExecutionOptions.context_policy是装配/显式重绑定的输入；ContextManager.policy是当前已发布实例的有效快照。默认值和有效快照不得在文档中都称为“当前生效策略”。有效实例只由装配或已有模型重绑定流程发布，本轮不新增策略热更新操作，不把set_config暗中变成模型切换。
3. **固定schema缓存保留执行错误阶段。** Tool创建/注册仍可完成；第一次授权校验惰性编译并缓存结果，错误也缓存。非法schema仍在当前执行阶段报Config错误，保持ToolStarted/PreToolUse顺序。
4. **候选查询为纯查询。** 返回身份/变体/有效effort等必要数据，使用已有纯选择校验；不为每个候选创建Client/ModelProvider。disabled规则不能由UI再解释。
5. **扩展CAS不自动重试。** 一次更新的读取、转换和CAS在同一后台工作中执行；这不是声称它们已经是同一SQL事务。冲突返回错误，取消不自动撤销或重放已开始工作。
6. **子会话先保持继承语义。** provider优先用调用该操作的冻结快照；显式model/tools覆盖继续优先。配置在子会话创建时复制父当前默认值，context策略取所用历史实例，子input/指令/workspace按现有规则独立。此批不顺便把配置继承改成父turn快照。
7. **目录Interface可以很小。** 在既有SessionCatalog封装目录公开能力，即使列表最终委托Store，也有隔离持久化的价值；不人为制造业务来让它显得“深”。本轮不要求私有化所有store字段。

### 4. 类型与函数处置方式

全量索引须覆盖所有生产struct/trait、相关enum与公开操作。普通数据、标准实现和宏生成访问器允许按明确类别引用共同理由；高耦合族逐项深查。每批开工前补齐该批涉及类型、函数及直接调用者的详表，**全仓详表不是无关批次的开工门槛**。

| 类型族 | 当前目标 |
|---|---|
| AgentBuilder / Agent / ProviderSnapshot | 保留装配、拥有者和冻结快照的时间差异；核对公开slot约束 |
| TurnOptions / Limits / Info / Handle / Context / Turn | 逐项记录数据/资源/状态职责；重点核查字段链与转发，不承诺一律合并 |
| Config / ResolvedModel / ModelSettings / SessionOptions / ExecutionOptions | 用现有类型收敛转换；不建立第二条配置流水线 |
| Task / Manager / Tool；Subagent / Factory / Tool | 保留领域与Adapter；子会话移除ToolContext依赖 |
| Budget / Snapshot / Metrics / Control / Attempt | 数据、观测、准入、结算区分；移动SQL，核对命名 |
| Hook配置 / 注册项 / 运行项 / wire / outcome | 保留协议差异；删除无实际用途的重复状态、解释和缓存 |
| Lease / SessionTurn / Activity / cleanup guards | 保留有实际一致性及清理职责的资源对象 |
| Memory / Skills / Table / Usage / RequestLog | 保留不同领域和持久化契约；共享执行边界不等于万能存储 |
| Renderer / Presentation / Canvas / cache；Draft / Overlay等 | 布局、帧提交、数据与交互分别评价，不因UI类多而合并 |
| controller族 | 只记录现状，冻结 |

处置矩阵固定记录：概念、唯一职责、拥有状态/生命周期、消费者、同族区别、删除测试、保留/合并/删除/收窄/延期结论。公开函数补充前置条件、副作用、调用顺序、case来源和相关行为测试。

没有生产调用不能自动删除公开API；新增类型必须说明现有类型/普通函数为何不足。最终报告同时给出实际增删数量和消失的复杂度，不设数量目标。

公开函数的case审查先覆盖四个具体族：

| 位置 | 要区分的case | 处置规则 |
|---|---|---|
| hooks/runtime.rs 的 condition_matches | command/file_path/path/pattern/query字段名单 | 证明是外部兼容协议还是为新工具不断补字段；前者归协议Adapter，后者收敛输入投影。H1的编译优化不能当作这个问题已解决 |
| permission.rs 的输入更新/批准/拒绝 | Hook改写、人工批准、硬策略复查 | 保留不同安全时刻的验证；只删除重复编译/搬运，不消灭业务差异 |
| execution/attachment.rs 的媒体分支 | 文本、图片、音频/文件、尺寸限制 | 具体媒体策略归附件Module，不为每个MIME预建类，不把校验放入UI |
| model错误分类/重试 | provider错误、恢复、server delay、cooldown | provider语义和调度恢复各有归属；不得按错误字符串到处补重试分支 |

每项在相关批次/R0给出保留、移动、调整或延期结论。需要新Interface的补丁式case先明确两个真实消费者与最小数据，不为了消灭match语句新增框架。

## 二、实施路线与依赖

| 编号 | 工作包 | 对应审计 | 前置条件 | 风险 |
|---|---|---|---|---|
| C0 | 字段生效契约与索引分类 | V1、类型/函数要求 | 无；与小批并行记录 | 文档及契约 |
| U1 | 模型候选规则收敛 | A2 | 该批消费者表 | 低 |
| U2 | 目录/维护公开入口 | A3 | 该批调用点表 | 低 |
| U3 | 首次启动配置映射 | A1 | 当前覆盖顺序确认；不等待策略热更新决策 | 低 |
| T1 | 固定工具schema与定义访问 | A6 | 选定错误阶段，相关API表 | 中 |
| L1 | 日志SQL归属 | A7 | 既有日志顺序/flush语义确认 | 中 |
| H1 | Hook固定模式编译所有权 | A8 | 条件错误与通配语义确认 | 中 |
| D0→D1→D2 | 存储并发验证→扩展→权限 | A4 | D0关闭/fencing gate通过 | 高 |
| G0→G1 | 子会话交互验证→分离 | A5 | G0普通调用/审批协议通过 | 高 |
| R0 | V项结论、余下API审查与收尾 | V2—V6 | 涉及批次完成或明确延期 | 按结论决定 |

推荐先U1→U2→U3，再T1、L1、H1；C0同步记录。D/G先完成各自有限的设计与行为验证，不因编号靠前而先改高风险路径。D0若无法在冻结范围内成立，延期D批，继续不依赖它的工作。L1和H1分开实现/验收。

每个代码包保留单独检查点和改动记录。无需自动创建worktree、commit或PR；有版本管理动作时另按用户授权执行。

## 三、具体实现计划

### C0：配置契约与必要性记录

**文件**：core/config.rs、runtime/mod.rs、runtime/lifecycle.rs、execution各读取点、harness/assembly.rs、context/mod.rs、collaboration/subagents/backend.rs；文档职责矩阵。

**步骤**：

1. 为ExecutionOptions逐字段列默认来源、生效时刻、消费函数、持久化路径和更新入口。
2. 明确启动字段、每轮冻结字段、宿主操作即时读取字段；给context_policy区分默认输入与有效实例快照。
3. 核查config() / set_config / change_config / 模型重绑定，文档说明差异；不擅改set_config返回类型、持久化范围或热更新行为。
4. 全量类型索引增加类别与处置状态；涉及修改的公开操作单独列详表，未决项不机械标“保留”。

**验收**：字段无遗漏；当前正常模型切换不被误报为缺陷；覆盖更新后下一轮、已开始turn、manual compact、model switch、child inheritance五种场景的契约。行为未变时先复用现有configuration/context/runtime测试，仅对缺少的契约补行为验证。

### U1：模型候选规则

**文件**：harness/config.rs；TUI/models.rs；相关config/model_configuration与模型picker测试。

**步骤**：

1. 提取resolve与候选共同需要的纯身份/变体检查，仍由Config拥有，避免再写一套disabled判断。
2. 用现有类型/简单返回数据提供候选身份与有效effort；不创建候选Provider，不新增候选Manager。
3. TUI删除provider遍历中的可用性规则和隐式当前项兜底规则；保留label、筛选、顺序与选择状态。
4. 保持既有默认项、变体项、当前隐式模型和effort继承语义。候选不代表网络可达或凭据有效，不承诺resolve所有传输步骤都必定成功。

**验收**：默认项、disabled、隐式当前模型、缺失provider/变体、effort继承有后端测试；同一身份的disabled判定与resolve一致。查询不构造Client、不发请求。TUI模型测试及smoke_models通过，冻结controller可照常使用旧的model_choices接口。

### U2：目录查询与CLI维护入口

**文件**：harness/storage/mod.rs（既有SessionCatalog）、storage/sqlite.rs的导入公开边界；TUI/launcher.rs、main.rs维护分支、sessions.rs展示投影。

**步骤**：

1. 盘点launcher和 `--import-json-sessions`：前者用.store/SessionStore，后者直接引用storage::sqlite和SqliteStore.path。
2. 在既有目录/存储Module公开目录查询与导入结果能力；具体SQLite路径知识留在后端。导入使用storage公开函数或Catalog关联函数，直接复用现有导入实现，不能先调用SessionCatalog::new：正常构造会拒绝尚未导入的旧JSON，先构造会产生循环前置。后端返回数量及目标位置，列表不附加虚构排序/权限流程。
3. launcher消费SessionMeta并保留排序/筛选；main仅选择是否执行导入及展示后端结果。
4. 不立即私有化SessionCatalog.store或迁移controller现有调用；记录剩余嵌入访问用途。

**验收**：空目录、已有会话、旧JSON导入、错误与备份保留有后端验证；launcher New/Resume/Esc/Quit和空列表行为不变。生产launcher不导入SessionStore，TUI不直接引用SQLite具体实现；测试fixture访问数据库不算生产依赖。

### U3：启动配置映射

**文件**：harness/config.rs、assembly.rs必要的既有入口；TUI/main.rs；configuration/model_configuration与启动smoke。

**步骤**：

1. 固定既有覆盖表：model/variant由CLI选择，yolo为CLI OR配置；root/cwd/resume由入口确定；steps、memory_search_limit、extensions、trusted_shell、prompt等来自文件设置。
2. 用Config、ResolvedModel、SessionOptions完成一次有实质行为的共同映射，复用ResolvedModel::apply_to；不新增ResolvedConfig/StartupConfig/Overrides链。
3. CLI继续解析argv、主题及终端要求，创建最小启动选择，删除共同逐字段映射；不重复解析主/压缩模型。
4. 不改变恢复input设置的既有持久化覆盖规则，不顺手给context_policy做热更新。保持 `--check-config` 的早返回位置，不因共同装配提前检查终端、打开目录/存储、读取提示或cwd启动资源。

**验收**：默认值、CLI覆盖、主/压缩模型request policy、context/steps/input、扩展和trust/yolo映射有后端测试。`--check-config`仍无需终端且无模型请求；启动/恢复smoke通过。只搬共同规则，controller的模板与prepare流程不改。

### T1：固定schema工作归Tool

**文件**：core/tool.rs、permission.rs、interaction.rs；ToolRegistry调用点；execution/loop_flow相关测试。

**步骤**：

1. 在现有Tool中共享惰性编译结果，缓存成功和失败；Tool clone共用结果，definition绑定身份不变。缓存拥有失败描述，在授权时构造同类别Config错误，不要求YourAiError支持Clone。
2. 提供普通输入验证操作或只读schema访问，authorize不再为读schema克隆完整definition。
3. schema第一次使用仍在现有authorize位置；保持注册函数的无Result接口和错误阶段，不能偷偷改为注册拒绝。
4. 无schema不编译；动态elicitation schema继续按自身请求处理，不误用固定工具缓存。
5. 只删除已被替代的编译/克隆路径，不把审批后的再次业务校验删掉。

**验收**：验证无schema、合法空schema、非法schema错误类别与ToolStarted/PreToolUse顺序；Hook改写输入、PermissionRequest批准后改写、硬策略Deny、clone绑定身份及动态交互。固定schema编译复用由可观察测试/限定测试插桩证明，不用实现镜像测试或未经测量的加速数字。

### L1：RequestLog拥有持久化编码

**文件**：harness/model/control.rs、model/metrics.rs、storage/request_log.rs及日志相关测试。

**步骤**：

1. 将model_requests INSERT/UPDATE及字段编码移入已有RequestLog/storage Module。
2. RequestControl传请求身份、来源、状态、耗时等必要数据，保留准入/cooldown；不直接包含rusqlite参数和SQL。
3. 内部仍用既有有序后台队列，start/finish共用请求ID；flush及错误传播不变。
4. 删除被替代的任意SQL调用入口或收窄其可见性，按实际剩余消费者决定。

**验收**：从SQLite读取请求行验证成功、失败、取消、开始/结束顺序、状态、flush和日志故障计数；Drop入队保持非阻塞。模型控制Module不包含SQL编码，现有pacing/retry测试保持通过。

### H1：固定Hook模式随注册/handler拥有

**文件**：harness/hooks/runtime.rs、matcher.rs、http.rs、config.rs、handler.rs必要调用点；Hook单元与hook_fix_flow测试。

**步骤**：

1. 将if条件的固定工具/模式解析放在注册编译阶段；URL允许模式在handler创建时处理。
2. 工具条件与URL匹配保持各自语义，不强行共用一个“万能通配器”。DNS label通配不能跨点、端口和路径。
3. 拟保持当前条件语义：原来不匹配的条件仍不匹配；若要把非法条件升级为注册错误，单列行为变更，不随优化暗改。
4. 运行项/handler拥有编译结果；注销后资源随实际引用生命周期释放，不承诺正在执行handler立即释放。
5. 迁移所有消费者后才删除对应进程级缓存；若还有实际用途，明确保留并记录，不清除仍在用的状态。

**验收**：有/无if、通配、无效条件、不适用事件、legacy工具名、URL主机边界、环境变量规则；注册去重、once、注销时执行中handler、注册顺序聚合与HTTP限制保持正确。dispatch不重新解释固定模式。

### D0：先证明存储执行与关闭契约

**文件**：harness/storage/table.rs、sqlite.rs、memory/mod.rs、skills/mod.rs；core/session.rs及runtime关闭路径只核查，必要修改另列；cloud_storage/sqlite/memory_lifecycle测试。

**步骤和推荐方案**：

1. 分类读、维护写、fenced会话写，逐调用核对store来源；明确未受租约约束的维护能力不能被当作运行中安全写入口。
2. 不调用core的pub(crate) SessionManager::blocking；不把host/activity塞进所有Table，不downcast为SqliteStore。
3. 优先在现有Table内使用owned store/id/key/输入数据执行后台读或整个CAS工作，调用方await结果；同步Store协议暂不改。需要捕获的字符串/数据在入队前转换为owned值。
4. 保持“读取→转换→CAS”作为一个后台工作，但仍承认底层是分别受保护的事务；不重试用户闭包，不假设可取消SQL。
5. 推荐关闭契约：fenced store必须在事务边界与lease释放排序；已进入持久化事务的工作先结算，释放后尚未进入事务的工作被fencing拒绝。取消等待者不保证任务被撤销，也不保证已排队工作必定提交。维护store单独验证。
6. 用gate测试验证现有SQLite连接/事务/fencing与core关闭能实现第5条。若不能，先给出实际资源拥有者上的限定settlement修改及API表，再改代码；该gate未通过不得推广，更不能仅加spawn_blocking就宣称完成。

**通过门槛**：单线程Tokio＋独立SQLite连接持Immediate锁时，其他定时任务仍推进；明确区分取消前未开始、读完未写、写事务已开始、提交后等待者取消、关闭/owner替换五种位置。结果是一次提交或明确错误，close返回后旧owner不能写入；CAS冲突不重放。测试用可控gate/通知与超时兜底，避免只靠睡眠碰运气。

### D1：扩展先试点，再推广

**前置**：D0通过。**文件**：Table、LocalSkills、LocalMemory及其直接调用者。

1. 先迁移LocalSkills.register一个写路径，验证创建、冲突、取消、关闭与重新读取。
2. 推广Table读/写和LocalSkills其余操作，再迁移LocalMemory；保留BTreeMap顺序、分类、时间戳、词匹配和限制。
3. Table没有缓存，不增加缓存发布流程。若其他涉及对象有缓存，只在持久化成功后发布。
4. 比对入口签名及错误语义；不新增每类扩展的执行管理对象。

**验收**：skills加载/目录、memory store/retrieve/search/list/delete/clear、peer连接CAS、旧owner拒绝和D0取消场景均通过；工作线程不因SQL锁等待被占住。现有memory_lifecycle/cloud_storage复用，新增只覆盖本次实际行为。

### D2：权限单独迁移

**前置**：D1通过且权限缓存结算契约明确。**文件**：harness/security/mod.rs与必要的既有存储辅助入口。

1. 分类持久规则、session_allow和hard_deny；逐步迁移读/更新，不与core运行journal一口气改造。
2. 更新工作保持CAS，guard不持有跨不受控异步等待的标准Mutex；成功后发布对应缓存，失败不发布候选。
3. 复用D批已证明的执行边界；若权限缓存需要不同收尾，不硬套Table、不创建统一PolicyService。

**验收**：记住审批、硬deny、规则更新、scope拒绝、CAS失败不改缓存、输入更新后二次硬策略检查；锁竞争及关闭场景。不能把避免阻塞换成放宽审批或改变YOLO语义。

### G0：明确子会话最小交互并做有限验证

**文件**：core/subagent.rs、interaction.rs/tool.rs实际能力定义；harness/collaboration/tools.rs、subagents/backend.rs；runtime/configuration测试。

**步骤和目标Interface**：

1. 列出exec/run/factory实际消费字段：父会话、冻结providers、有效cwd/security/sandbox、cancel、子事件、问答；call_id不属于领域。
2. 优先用已有SessionId、ProviderSnapshot、Out、Value和cancel表达数据；领域进度是带子标识的原生事件，工具Adapter才包装ToolProgress JSON。
3. 问答是独立可选能力：收到child Ask时保留child request id，取得答案后交回child；工具入口继续映射parent独立request id与Question；普通程序入口能回答，未提供能力时明确报错而非永久等待。
4. 使用明确参数与借用回调/已有Seam实现最小调用；不把ToolContext换名成通用SubagentContext。此批在实际Rust借用/生命周期下确定签名及全部调用点表，不以新框架规避问题。
5. 保持总体决策中的配置/provider继承时机、显式覆盖及child独立输入设置；消费者返回关闭/失败触发取消与已有清理。

**通过门槛**：普通程序入口无需伪造call_id，工具入口仍收到相同子进度和审批回路；问答缺失、父取消、子事件消费者断开有确定结果。若不能复用既有能力，先记录唯一缺失语义与最小数据/Interface，不能先建Context/Executor系列。

### G1：迁移完整子生命周期

**前置**：G0确定签名和门槛通过。

1. SubagentFactory不再接收ToolContext，只取得装配必需的明确数据。
2. exec/run移除工具身份与工具JSON包装，Adapter负责对应协议；不复制create/run/continue/close。
3. 迁移所有工具与普通程序调用，删除旧签名和单纯搬运层；ChildCleanup和父子登记继续由core拥有。

**验收**：普通调用、工具调用、子审批、父取消、消费者断开、Factory错误身份、SubagentStart阻断、Stop要求继续、父关闭期间创建、子配置独立与冻结provider继承。关闭后无遗留子会话/未监督任务，不能只验证返回文本。

### R0：条件项结论与最终收尾

| 观察 | 本计划的默认处置 | 改代码的触发条件 |
|---|---|---|
| V2 校准恢复 | 先记录当前观测是否仅当前运行有效，补缺失的恢复场景 | 有明确恢复契约及正确性证据，才变更持久化/估算 |
| V3 宽SessionStore | 保留共享事务/lease接口 | 实际第二Adapter或消费者证明拆分能减少必要知识，而不是增加转换层 |
| V4 宿主Agent slot公开 | 记录独立Agent与宿主使用约束 | 证实当前公共承诺允许非法identity/store重绑定，再收窄或受控更新；不影响独立Agent |
| V5 seen增长 | 保留去重与恢复保护，记录容量风险 | 定义重放窗口并测得增长需求后才回收；若涉及Server则延期 |
| V6 watch重启 | 按当前单次生命周期说明接口 | 有合法重启需求/调用才实现重启；不凭猜测增加状态机 |

补齐全量类型分类和公开操作索引；重点族给出逐项必要性，未决项必须有原因和处理条件。清理实际被替代的旧路径、无行为转发和过期注释，更新数量。每个A项标为已实现或明确延期，每个V项标为保留/调整/延期；不能以“以后再看”结束。

## 四、验收标准与验证执行

### 1. 分批检查层级

| 改动层级 | 必须验证 |
|---|---|
| 文档/契约记录 | 链接、源码依据、范围一致；不声称运行验证 |
| 局部行为 | 相关单元及受影响集成测试、相关编译、fmt、diff |
| 公共签名/trait变化 | workspace all-targets编译及Clippy，迁移examples/tests/冻结调用者 |
| TUI调用变化 | TUI单元、相关启动/恢复及模型smoke |
| 本计划最终收尾 | workspace测试、全目标Clippy、fmt/diff、两组离线PTY smoke |

对应命令入口：

```sh
cargo check --workspace --all-targets
cargo test -p yourai-harness --lib config::
cargo test -p yourai-harness --lib hooks::
cargo test -p yourai-harness --test configuration --test model_configuration
cargo test -p yourai-harness --test execution --test loop_flow --test execution_repairs
cargo test -p yourai-harness --test cloud_storage --test sqlite --test memory_lifecycle
cargo test -p yourai-harness --test runtime --test lifecycle_regressions --test lifecycle_repairs
cargo test -p yourai-tui
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
git diff --check
```

这些是选择入口，不要求每个小批全部重跑。最终用 `cargo test --workspace --no-fail-fast`，构建实际测试二进制后运行 `apps/yourai-tui/tests/smoke.py` 与 `smoke_models.py`；按脚本当前参数/环境执行，不以过期二进制验证源码。不请求外部真实模型，不通过放宽测试断言掩盖回归。

### 2. 三个架构目标的最终判据

**类型正交**：所有生产类型被分类；重点同概念族逐项给出状态/时间/协议差异；无依据的多层包装有明确处置。不可变快照不是第二个可变权威。没有为了每个case新增一个对象或为未来扩展预建trait。

**函数正交**：涉及公开API都有消费者、前置条件、副作用和case来源记录；共同配置映射、候选规则、固定schema编译、日志SQL、固定模式解释各有一个实质实现。普通子调用不要求工具身份；没有把普通操作改成调用方拼步骤。必要的最终输入/硬策略复查保留。

**高内聚低耦合**：生产TUI不复制后端规则或直接知道SQLite；领域子会话不依赖工具身份协议；SQL在storage，准入在model，模式状态随其实际拥有者。现有Interface提供行为深度，变更Locality可以从调用点/改动文件证明，而不是仅画依赖箭头。

源搜索用于核查上述旧路径和依赖，不能只靠字符串不存在证明行为正确。冻结controller如仍有公开嵌入访问明确标注，不宣称未来Server已解决。

### 3. 交付与停止条件

每批记录：精确文件/函数/类型、实际增删/合并、为什么、调用方少知道什么、不变量、测试结果、延期范围和具体下一步。新增类型要说明现有类型/普通函数不足的真实原因；没有删除类型就写0。

完成交付：审查意见、新版计划、类型/公开操作处置记录、A/V状态表、配置生效表、实际变更清单和最终验证结果。所有确认问题有完成或明确延期结论，冻结范围保持不变。相关高风险gate失败时停止该批并修复/延期，独立批次继续；不为消灭待办强行扩范围或删除保护层。
