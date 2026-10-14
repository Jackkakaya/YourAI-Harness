# 全局职责收敛与重构计划

> 本文保留历史重构计划。配置部分已于 2026-10-15 更新为单 JSON、Arc<Config>、内部加锁的嵌套 getter/setter 与显式 save；旧 update、分拆存储和 SQLite 配置计划已废止。当前实现以 [配置契约](core-contracts.md#配置读写与持久化契约2026-10-13) 为准。

更新日期：2026-10-13。对应分支：`codex/orthogonal-runtime`。状态：第一批进程内重构已完成，第二批接口收敛与配置所有权已收敛到共享 Config，SessionManager 读取每轮冻结数据，后端控制迁移已完成，独立网络 Server 仍未实施。

## 1. 当前有多少类型

统计范围是三个 crate 的生产 Rust 源码声明，排除测试文件、内联测试模块、第三方 re-export；包含条件编译及宏中固定名称的声明。Rust 没有 `class`，这里以 `struct` 对应对象和数据结构。相同名字在不同模块分别计数；声明数量不是运行实例数量，也不是抽象层数。

| 包 | struct（类/数据结构） | trait（接口） | enum | 总计 |
|---|---:|---:|---:|---:|
| yourai-core | 96 | 16 | 35 | 147 |
| yourai-harness | 79 | 0 | 22 | 101 |
| yourai-tui | 54 | 0 | 17 | 71 |
| **合计** | **229** | **16** | **74** | **319** |

全部名称、可见性与源码位置见 [当前类型清单](current-type-inventory.md)。当前清单包含 319 个声明，已重新扫描源码位置。枚举事件、请求/结果、序列化数据、资源 guard 和有状态对象都包含在统计内，不能把 229 个 struct 等同于 229 层设计。

## 2. 判断标准与最终职责

每个 Module 应有唯一状态所有者和清晰 Interface。一个变更应尽量集中在同一处（Locality），小的 Interface 应封装完整行为（Depth），而不是要求调用方拼接步骤。

判定重复职责需要实际证据：同一状态保存多份、同一默认值多处定义、同一生命周期流程多处实现、转发层要求调用者了解下层、无实际 Adapter 的扩展 Seam。删除一个 Module 后，复杂度消失才适合删除；若复杂度散落到多个调用方，则它可能有必要保留。

| 对象/Module | 唯一职责 | 不应承担的职责 |
|---|---|---|
| Harness | 装配 Adapter，切换模型和权限绑定 | 输入队列、执行账本、界面状态 |
| SessionManager | 读取 Config.execution 的冻结副本、活会话队列、执行串行化、子会话与关闭 | SQL 实现、终端绘制 |
| Agent | 执行入口、依赖绑定与一致快照 | 持久化会话队列 |
| AgentLoop / DefaultLoop | 决定执行顺序 | 保存第二套配置、第二份会话状态 |
| Turn | 单轮操作账本、输入路由与完成状态 | 会话恢复、跨轮队列 |
| Model / Tool / Compactor | 对应操作的完整执行及 Hook | 宿主生命周期 |
| ContextManager / DefaultContext | 活动历史、模型请求投影与上下文更新 | 模型调用、SQL 细节 |
| SessionStore / SqliteStore | 持久化、原子事务、租约与版本校验 | 调度执行 |
| Workspace | 工作目录、指令、文件监听与 worktree | 通用会话配置更新 |
| TaskManager / Subagent | 任务转换 / 子会话操作 | 重复拥有会话关闭流程 |
| 后端 SessionController / TUI View / Renderer / Presentation | 会话控制 / 显示投影 / 布局 / 帧提交 | 服务侧数据写入与会话执行所有权 |

函数遵循同样标准：一个行为只有一个实质实现；上层只选择参数，不再复制初始化、失败清理、限制解析或状态结算顺序。不新增通用 Manager、Executor、Factory、Transport 或事件总线来包装现有流程。

## 3. 已确认的问题与已经完成的重构

下列是已实施事项，不是待办。具体调用方式见 [配置与运行职责收敛](orthogonal-runtime.md)。

| 原对象/流程 | 重复职责或过度设计证据 | 当前改动 | 状态 |
|---|---|---|---|
| SessionConfig、ExecutionConfig、LoopConfig | 一次执行需要拼接多套设置，循环另持有默认值 | 合并为 ExecutionOptions；DefaultLoop 改成无状态结构 | 已完成 |
| InputOptions、RuntimeConfig | 初始输入和持久化更新重复表达相同字段 | 字段并入 Config.execution，独立输入设置结构已移除 | 已完成 |
| HarnessConfig 与运行配置 | 启动装配选择和执行设置混杂；部分字段多处出现 | 改为 SessionOptions，分类配置通过 config: Config 传入，execution 类别用于每轮冻结；指令路径只定义一处 | 已完成 |
| Model/Tool/审批/Hook 的限制选择 | 各操作分别判断覆盖值和默认值，容易产生不同语义 | ExecutionOptions::resolve 统一解析；TurnLimits 只收紧上限 | 已完成 |
| 配置中的 ToolOutputStore | 配置数据夹带有状态依赖 | 移到依赖快照，固定本轮使用的实现 | 已完成 |
| 创建、恢复、子会话装配 | 初始化和失败清理容易分叉 | 共用 prepare_session → assemble → start_session | 已完成 |
| Workspace.change_config / notify / config | 文件工作区承担会话配置和输入通知 | 配置更新归 config::update，输入通知归 SessionManager；删除 Workspace 对应入口 | 已完成 |
| ObservabilityProvider、Span | 无生产实现，增加 provider 插槽和调用 Interface | 删除两项 trait 与插槽，真实诊断使用 tracing | 已完成 |
| SessionStore 的默认 unsupported 方法 | 必需能力在 Interface 上伪装成可选，错误延迟到运行时 | 租约、状态、原文相关八个方法必须实现 | 已完成 |
| TUI 标题生成/恢复 | 显示查询同时修改持久化元数据 | 标题归会话侧；TUI 只读取 | 已完成 |
| Controller.yolo | 布尔状态与实际安全绑定两份真相 | 从 Harness 当前绑定推导，删除重复布尔值 | 已完成 |
| App 取得 Harness 后访问内部状态 | 上层 Interface 泄漏运行实现 | 改用 Controller 的环境、队列、任务查询 | 已完成 |

第一批减少了五个类型声明：三套配置变为一套（净减 2），两套输入设置变为一套（净减 1），删除两个观测 trait（净减 2）。HarnessConfig → SessionOptions 是改名和职责收窄，不算删除。更多收益来自删除重复字段、函数和状态，不会体现在类型数量上。

改动也修正了子会话误继承父会话技能激活、启动覆盖显式 workspace 设置，以及标题回填失败阻止恢复的问题。子会话继承执行设置，输入激活独立；恢复和关闭继续保留原子提交、租约 fencing 与取消结算保障。

## 4. Session* 与 Turn*：哪些保留，哪些仍需收敛

| 当前类型 | 实际职责 | 判断与行动 |
|---|---|---|
| SessionManager | 一个活会话的行为和生命周期所有者 | 保留；后续收窄对外访问，不能变成统管所有 Module 的大对象 |
| SessionLease | 取得数据库执行租约；作用域结束释放，限定所有持久化操作的 owner | 保留；资源所有权与会话调度正交，直接删除会把释放与 fencing 分散到调用方 |
| SessionEnvironment | 身份、cwd、transcript 路径和加载指令的冻结数据 | 保留数据结构；不新增行为或另一套配置 |
| SessionTurn | turn_id 与已结束 TurnResult 的报告 | 保留报告语义；是否可与现有输出报告合并列入 P1 审计，尚无安全删除结论 |
| Turn | 活跃执行账本、调用状态、部分输出与完成状态 | 保留为单轮执行核心 |
| TurnContext | Loop 借用的通道、取消信号、身份和依赖快照 | 有独立借用生命周期；保留 transport 职责，P1 收窄可见性与访问链 |
| TurnHandle | 外部持有的运行任务、输入输出通道和取消/等待权 | 保留；与执行内部状态不同，Drop 取消和 join 需要明确所有权 |
| TurnOptions | 发起一次执行的参数：来源、会话快照、限制、事件等 | 保留参数职责；已删除 config 覆盖字段；避免成为调用方手工装配内部状态的入口，P1 审计 |
| TurnLimits | 单次执行收紧限制和绝对 deadline | 保留；配置默认值与本轮 deadline/收紧限制不同。配置来源已统一为共享 Config |
| TurnInfo | Core 生成的身份及冻结参数 | 保留元数据；P1 检查是否必须整个公开 |
| ExecutionOptions | 会话与执行默认设置 | 已统一；不能再出现第二套相同运行设置 |
| Config 的七类字段、ContextPolicy、ImagePolicy | 具有独立语义的配置分组 | 保留嵌套数据；统一配置入口不要求所有字段摊成一个大结构 |
| SessionOptions | 创建/恢复和 Adapter 装配选择 | 保留启动参数；不复制 ExecutionOptions 字段 |

Turn、TurnContext、TurnHandle **尚未合并**。第一批主要处理重复配置与错误归属，没有把所有同前缀的类型删掉。这里的保留是基于所有权、生命周期和数据/行为差异，不是默认认可所有公开 Interface 都合理。

## 后端控制迁移（已实施）

详细职责与共用调用见 [后端与 UI 分工](backend-ui-separation.md)。

- [x] 删除 TUI Runtime、InputAction 与 Effect 的生产定义；任务控制迁入 harness。
- [x] 后端独立后台循环拥有执行、提交、压缩、续跑、操作互斥和切换清理，不依赖 UI poll。
- [x] TUI 只发送操作、接收结果并更新显示；移除基于 View 的领域操作判定。
- [x] 模型配置解析、effort 选择及切换流程迁入后端；TUI 移除 genai 依赖。
- [x] 任务版本/列表查询归状态所有者；JSON 工具协议归 TaskTool / SubagentTool。
- [x] Hook 解析删除重复编解码；ModelSelection 数据复用。
- [x] 增加无 UI 消费、真实会话退休和失败切换的后端集成测试，迁移已有模型设置测试。

验证：631 项 workspace 测试、两组离线 TUI smoke、fmt 与 Clippy 全部通过。

后端控制迁移完成时有 317 个声明。数量增加来自实际操作协议、历史/快照数据、后台任务所有者和具体工具 Adapter；没有增加通用 Transport、配置管理器或事件总线。完整清单重新扫描当前 src，旧统计只用于记录先前批次。

## 5. 当前仍有的问题与候选

配置之外的后续审查见 [模块职责审查](architecture-audit-next.md)：其中任务/子会话工具协议、任务查询泄漏和 TUI 执行控制现已改造；Hook 协议结构、模型容量/预算和渲染职责暂保留。

| 优先级 | Module / 源码 | 当前证据与问题 | 计划动作 | 判断强度 |
|---|---|---|---|---|
| P0 | 共享网络 Server | 进程内后端控制已迁移；网络订阅和客户端断开尚未分离 | 后续实现独立服务、事件游标及多个订阅客户端 | 部分完成；不再由 TUI 驱动执行 |
| P1 | TurnContext、TurnInfo、TurnOptions、ProviderSnapshot / core/context.rs、turn.rs | Loop 需要理解多层访问链；底层 transport 和内部绑定广泛公开 | 列出生产调用方实际依赖；隐藏非扩展必需字段，减少手工装配；只在删除确实降低复杂度时合并 | Worth exploring；不是已确认可删四种类型 |
| P1 | SessionManager / core/runtime/mod.rs | agent()、store() 等公开入口允许上层绕过会话操作；第一批只收窄了 App | 审计每个使用点，装配内部访问收窄为 crate 可见；按真实命令和查询提供小 Interface | Worth exploring |
| P1 | Agent、AgentBuilder、ProviderSnapshot / core/context.rs | 第二批已删除 Providers 包装；共享 Config 是唯一可编辑来源；SessionManager 读取 execution 冻结副本；Agent、Builder、依赖快照均已删除配置字段 | 已删除包装和 ctx() 转发，bindings 已收窄，配置与依赖所有者已分开；剩余核查更新权 | 部分完成；Builder 与冻结快照保留 |
| P2 | SessionOptions、共享 Config 及七类数据 / assembly.rs、config.rs、context/prompt.rs | Rust/JSON 分类、统一修改入口和旧格式迁移已完成；Prompt 与可选 Hook 独立文件不建立独立管理器 | 使用现有分类配置契约，核对所有读写路径与资源装配的生效边界 | Worth exploring |
| P2 | Hook wire_output 与 core Hook outcome / harness/hooks、core/hooks.rs | 外部兼容协议和内部结果有相似名称及转换 | 检查转换是否分散；集中单一协议 Adapter。不要让外部 JSON 协议污染执行核心 | 候选；相似名称不足以证明过度设计 |
| P2 | ModelLimits、ModelTokenBudget、ModelBudget、BudgetSnapshot | 都叫 budget/limits，容易混淆容量、请求预算、累计用量 | 明确术语和所有者，检查重复计算；保持容量与用量账本独立 | 候选；尚未确认重复行为 |
| P2 | Renderer、Presentation、Canvas、TimelineCache / TUI | 显示结构多，但分别处理布局、提交和缓存 | 通过修改路径判断耦合；删除仅转发的函数，保留已验证的滚动/绘制职责 | 候选；不能凭数量合并渲染层 |

这些是后续核查范围，不是“已经确认全部过度设计”的名单。已确认的重复职责列在第 3 节；第 5 节的探索项必须先通过调用点与删除测试，才进入实施。

## 6. 最终执行顺序与验收

### 阶段 A：统一配置与进程内归属（已完成）

- [x] Config.execution 单一定义、ExecutionOptions::resolve 单一限制解析。
- [x] DefaultLoop 无状态，共用创建/恢复/子会话装配。
- [x] 删除空观测接口，强化持久化必需契约。
- [x] 会话拥有配置更新和标题，TUI 删除重复权限状态。
- [x] 更新类型清单和设计文档。
- [x] 628 项 workspace 测试、两组离线 TUI smoke、fmt 与 Clippy 通过。

### 阶段 B：收窄现有 Interface（部分完成）

第二批已实施：

- [x] 删除 Providers；Agent 直接拥有绑定、原子更新和快照，无额外 Arc 容器或 ctx() 转发。
- [x] Agent::bindings 收窄为 crate 内部，仅生命周期 Hook 需要未校验 Loop 的一致绑定读取。
- [x] 删除 SessionManager.history 异步转发，实际上下文操作仍由 ContextManager 拥有。
- [x] watch_paths 收窄为 crate 内部，由 Workspace 使用。
- [x] 删除未使用的 Turn.limits / check_control 转发；有效配置通过 Turn.config 查询，取消检查仍由 TurnContext 与执行模板负责。
- [x] 删除 TurnInfo.started_at；生产代码没有消费者，绝对 deadline 继续由 TurnLimits 持有。

第二批净减 1 个 struct，当时总计 307 个声明。不为本轮结构调整新增容器或兼容别名。验证：628 项 workspace 测试通过；删除最后的未用字段后 core 测试再次通过；最终 fmt、Clippy 和 diff 检查通过。第一批的 PTY smoke 结果保留，本批未重复运行。

配置所有权的当前调整（替代此前 Agent 或 SessionManager 独立持有可编辑配置的方案）：

- [x] 共享 Config 为唯一配置来源，Rust 与 JSON 都按七类字段组织。
- [x] config::update 修改缓存、同步保存所选来源后发布内存，不重新加载文件；旧平铺格式可读、保存时迁移。
- [x] SessionManager 通过绑定函数读取 execution 类别，每轮冻结；config() 返回当前执行数据，不再提供 set_config。
- [x] Agent、AgentBuilder、ProviderSnapshot 不保存第二份可编辑配置；TurnOptions 不含配置覆盖入口。
- [x] 模型、effort、YOLO、主题走后端已有操作并保存共享 Config，不从会话数据库恢复旧偏好。
- [x] Prompt 与可选 Hook 文件独立，各分类没有自己的加载/保存服务。

具体 API、存储和生效边界见 [当前配置契约](core-contracts.md#配置读写与持久化契约2026-10-13)。历史批次的测试数量仅记录当时结果，当前类型与验证结果以类型清单和配置契约为准。

剩余审计：

1. 审计 SessionManager、Turn*、Agent 绑定的全部生产调用点，标出谁真正需要扩展和内部访问。
2. 删除无行为转发函数、收窄内部类型/字段可见性；每次合并注明消失的复杂度与保留的不变量。
3. 核查配置转换、Hook 协议转换和 budget 计算，只修复证据明确的重复实现。
4. 通过现有公共 Interface 验证取消、deadline、Hook、恢复、热替换快照与子会话；避免给纯转发函数补镜像测试。

验收：调用方不需要手工拼接内部生命周期；相同状态和计算只有一个所有者；文档记录每个候选的实际处置，不用目标类型数量代替质量。

### 阶段 C：服务侧唯一执行所有权（进程内控制已完成，网络服务待实施）

1. 已在后端控制器持有 driver 并承接 submit、steer、reply、compact、interrupt、close；后续移入独立服务进程。
2. TUI Runtime 执行任务已删除；后续在网络协议中分离客户端断开与会话关闭。
3. 提供带游标的事件与快照，规定重连和恢复行为；基于一个实际通信方式实施，不提前建设多协议 Transport 框架。
4. 验证双客户端唯一执行、审批只生效一次、订阅不丢事件、服务重启恢复以及过期租约写入被拒绝。

验收：客户端数不影响执行实例数，关闭行为有唯一归属，持久化 fencing 保持有效。此阶段可能增加真实协议数据类型，不能承诺 struct 总数只降不升。

### 阶段 D：最终清理（待实施）

删除迁移后的废弃路径，更新完整类型清单与职责表，重新跑与改动相关的 workspace/PTY 检查。最终交付逐项说明保留、合并、删除的理由；不保留旧类型兼容别名或两套并行实现。

## 7. 完成标准

每份状态有一个所有者；每种设置有一个定义和明确覆盖顺序；每个实质生命周期流程有一个实现。公开 Interface 不暴露下层装配顺序。资源 guard、冻结快照、协议数据与行为对象各自表达真实差异。所有未实施和仍需验证的事项明确标注，不能把第一批通过测试表述成全局重构已结束。

## 8. Server 前置收敛（2026-10-13）

- [x] 控制器删除 operation 的 idle 布尔参数，统一按命令决定准入条件。
- [x] 删除/切换的身份检查与操作登记放入同一临界区；steer 的检查和入队同样原子化。
- [x] 后端错误移除 UI 草稿承诺，保持展示与控制边界。
- [x] 记录 [Server 实施计划](server-plan.md)，包括服务所有权、客户端断线、会话寻址、事件恢复和验收。
- [ ] 独立网络 Server 实施；当前 Drop 关闭与单消费者事件接口仍需在服务阶段调整。

本轮不新增类型、不删除类型；删除的是分散的控制判断和可误传的布尔参数。当前统计仍为 227 struct、16 trait、74 enum。

本轮验证：控制器 4 项集成测试通过；harness 库测试 111 项通过、1 项忽略；harness/TUI 全目标 Clippy（警告视为错误）及 diff 检查通过。


## 9. 后端所有者与客户端生命周期分离（2026-10-13）

- [x] SessionController 只拥有 worker 生命周期、关闭结果与当前事件接收器。
- [x] 把命令与快照字段移入可克隆 SessionClient；所有命令实现随字段迁移，不复制执行逻辑。Controller 通过 Deref 使用同一实现，避免逐个转发函数。
- [x] 客户端 Drop 不取消后端；只有所有者 close/Drop 触发关闭。未来服务持有所有者，连接持有客户端。
- [x] 关闭开始时同步拒绝新输入、回复和操作。输入接收器主动关闭并排空已经接受的输入，因此存活的客户端不会让关闭永久等待发送端释放。
- [x] 测试客户端释放后执行继续、存活客户端不阻碍显式关闭，以及所有者释放后会话关闭。

新增 1 个 struct SessionClient，不删除类型；总计 228 struct、16 trait、74 enum。新增类型表达不同的资源所有权，不新增管理层、trait 或配置。TUI 继续通过原所有者使用后端，控制流程没有回到 UI。

下一轮：把单消费者事件接收器改为独立订阅，设计有限缓冲与明确丢失提示；验证多个消费者和慢消费者不影响执行。当前命令客户端尚不包含事件订阅，尚未实现网络连接、事件游标或 Server。

SessionClient 不公开底层 Harness；嵌入访问保留在运行所有者 SessionController，避免客户端绕过命令入口直接关闭资源。本轮验证：6 项后端控制器集成测试、191 项 TUI 测试通过（1 项忽略）；workspace 编译、全目标 Clippy（警告视为错误）、fmt 与 diff 检查通过。


## 10. 独立事件订阅与有限缓冲（2026-10-13）

- [x] 把后端到消费者的无界单接收通道替换为 Tokio broadcast，保留最多 1024 条事件。每个订阅独立读取，后台发送不等待订阅者。
- [x] 新增 SessionSubscription，只拥有读取位置；SessionClient.subscribe() 创建独立订阅。释放订阅不停止执行，不影响其他订阅。
- [x] 落后订阅收到 SessionEvent::Lagged { skipped }，明确表示增量历史不完整；TUI 显示警告并继续读取最新状态快照，不声称已恢复历史。
- [x] 客户端只持有事件发送端的 Weak 引用；后端关闭后订阅读完缓冲即可结束，不被存活客户端延长通道生命周期。
- [x] SessionEvent / SessionHistory 可克隆以供广播；公开列表/删除错误转换为 String，与模型/打开错误保持同样的展示协议，私有 Effect 保留原始领域错误。

新增 1 个 struct SessionSubscription，删除原无界事件出口，不删除已有类型。当前 229 struct、16 trait、74 enum，总计 319 个声明。它表达独立消费者的真实读取状态，没有增加通用事件总线或 Transport trait。

注意：1024 是条数限制，不是字节限制；内部执行输出桥接和命令队列仍有无界通道，本轮没有宣称整个运行时内存有固定上限。新订阅只接收后续事件，不回放已发生事件；快照与订阅之间没有原子的恢复游标。

下一轮：整理会话寻址与单会话执行的职责，处理当前 switch 会替换共享会话的问题；未来多个客户端选会话应只改变各自访问目标。先收敛现有打开/恢复流程，避免再叠一个管理器。持久化事件游标、历史恢复及网络 Server 仍待实施。

本轮验证：8 项后端控制器集成测试、191 项 TUI 测试通过（1 项忽略）；workspace 编译、全目标 Clippy（警告视为错误）、fmt 与 diff 检查通过。广播集成测试验证快订阅完整且有序收到 3072 条事件，慢订阅报告缺口、任务正常完成、退订互不影响、关闭结束订阅。


## 11. 会话工厂复用现有依赖绑定（2026-10-13）

范围约束：用户要求暂停 controller 改动，留待 Server 阶段重构。本轮不修改 controller、SessionClient、SessionSubscription 或 TUI 控制代码。

- [x] runtime::create 从 5 个参数减到 3 个；runtime::restore 从 10 个参数减到 6 个。模型/Hook/工具/安全/沙箱通过已有 AgentBuilder 表达，ExecutionOptions 独立传入。
- [x] 私有 open 从 7 个参数减到 3 个；删除两处 too_many_arguments 豁免，以及逐项把参数复制回 builder 的实现。
- [x] 子会话一次装配 compaction model 等绑定；删除启动后补 set_compaction_model 的路径。
- [x] 模型缺失在创建存储前报告配置错误；usage 缺省装配 LocalUsage，显式提供时保留原实现。
- [x] 迁移全部生产与测试调用点，不保留旧参数兼容接口；补充独立配置、自定义 loop/usage/Hook 与缺失模型的工厂集成验证。

本轮新增/删除类型均为 0；仍为 229 struct、16 trait、74 enum。删除的是重复的依赖表达与启动后补设置步骤。AgentBuilder 继续只表示依赖，ExecutionOptions 继续只表示运行设置，SessionManager 继续唯一拥有配置。

同时审查了 TaskManager、LocalMemory、LocalSkills、Table 与模型计量对象：暂时保留。TaskManager 承担 Hook/事务/缓存发布；LocalMemory 与 LocalSkills 实现不同领域操作；Table 集中数据库修订校验；模型 Attempt 保证请求结算。没有仅因名字类似就合并。

下一轮先审查 Hook 配置解析、执行与结果聚合中的重复校验/转换；保持外部 wire 协议与 core outcome 分离。不继续 controller / 订阅 / Server 改造，直到用户重新指定。

本轮验证：configuration、runtime、lifecycle_regressions、lifecycle_repairs、coding_tools 共 90 项集成测试通过；workspace 全目标编译、Clippy（警告视为错误）、fmt 与 diff 检查通过。验证覆盖子会话模型设置继承、Hook、恢复租约、配置独立性和工具装配。


## 12. Hook 内部事件与超时解析收敛（2026-10-13）

- [x] dispatch、输出事件校验和聚合直接传 HookEventKind，删除内部字符串事件路由；外部 hookEventName 字段不变。
- [x] HookSpecificOutput.event_name() 替换为 event_kind()，在 wire Adapter 处映射外部变体到 core 事件；没有保留旧方法别名。
- [x] HandlerConfig.validate() 一次校验并返回解析后的 Option<Duration>。删除独立 timeout() 方法及 build_registrations 中的第二次读取/转换。
- [x] Duration::try_from_secs_f64 把溢出返回配置错误；拒绝舍入为零的超小值，避免转换 panic 或意外立即超时。
- [x] 删除只服务一个调用点的 parse_json_value 私有转发，直接使用已有 Value 解析入口。
- [x] 增加四种 handler 注册验证：无超时、1.25 秒、零、负数、1e300、1e-300；保留权限聚合、事件不匹配和异步执行回归。

本轮新增/删除类型均为 0；仍为 229 struct、16 trait、74 enum。删除 2 个函数（timeout、parse_json_value），替换 1 个事件查询方法；没有增加通用校验器、解析器对象或配置类型。Command、HTTP、Parsed 输出的错误策略有不同协议语义，保留；FailurePolicyConfig 与外部 wire / core outcome 也暂不合并。

controller、客户端句柄、订阅及 Server 继续暂停，没有修改相关代码。下一轮审查工具参数校验与安全上下文生成，核查重复解析/校验和配置依赖；保持审批策略与实际执行分工，不提前承诺删除类型。

本轮验证：63 项 Hook 单元测试、65 项 Hook/会话集成测试通过；workspace 全目标 Clippy（警告视为错误）、fmt 与 diff 检查通过。最终单元测试在删除解析转发后重新运行，验证当前实现。

## 13. 全量审计完成，实施暂未开始（2026-10-13）

用户要求先全面扫描，不急着修改。审计覆盖三个 crate 的 131 个 Rust 源文件（含测试文件），生产清单仍为 229 struct、16 trait、74 enum。源码哈希检查确认本轮没有新增、修改或删除运行/测试代码；不执行新测试，不将旧测试成绩表述为此次审计已验证所有风险。

完整结果见 [全量架构审计](global-architecture-audit.md)：8 项有明确源码证据的收敛候选、6 项需要先明确语义/验证的观察，以及明确保留的类型族。没有提出可立即删除的 struct/trait 清单，不用类型数量代替模块质量。

后续工作以该全量清单及依赖顺序为准，替代先前逐模块零碎推进的下一步。当前只记录候选，没有启动实施；controller/客户端/订阅/Server 继续暂停。

## 14. 重构计划 1（尚未实施）

按照用户提出的类型正交、公开函数正交及高内聚低耦合三个要求，生成 [重构计划 1](refactoring-plan-1.md)。计划以逐类型/公开函数职责矩阵和配置生效契约为起点，再按依赖顺序处理全量审计A1—A8；V1—V6先明确语义或验证，不强制改造。每批记录删除/移动/保留的理由、实际验证与具体下一步。不设删类型数量指标，不继续controller或Server相关改动。

本次只创建计划并添加索引，没有修改运行代码，未运行测试。

## 15. 重构计划 1 审查修订（尚未实施）

按用户要求审查并重写 [重构计划 1](refactoring-plan-1.md)，审查问题单见 [计划审查意见](refactoring-plan-1-review.md)。新版明确总体职责、默认行为契约、分批依赖、文件/实现步骤、风险门槛与局部/最终验收。完整矩阵不再阻塞所有独立工作；UI三项拆批，日志/Hook分开，存储及子会话先通过限定行为验证再推广。

此处原 set_config 计划已被统一 config::update 契约替代；明确配置值与运行资源的生效时机、schema执行错误阶段、子会话当前继承时机；导入不能依赖先构造拒绝旧JSON的正常Catalog。controller/Server持续冻结，公共接口改变需保证冻结代码编译。

两路只读复核已完成，文档链接与diff检查通过；131个源码文件哈希与审计基线一致。本次只修改计划及索引，未实施代码，未运行测试。

## 16. 拆分为简短任务文档（尚未实施）

按用户要求把重构计划 1 拆为 [18 个任务](refactoring-plan-1-tasks/README.md)：9 个改造、1 个契约整理、7 个前置验证或条件性检查、1 个最终验收。每份任务说明涉及的 struct/trait/函数、不合理原因、修改方式、验收和需要保持的行为；索引记录依赖与状态。

前置验证失败只阻止相关依赖；条件性检查可得出保留结论，不强制修改。controller/客户端/订阅/Server 继续冻结。本次只拆分文档，没有改动或删除源码，未运行代码测试。
