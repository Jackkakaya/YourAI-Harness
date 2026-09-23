# DefaultLoop 完整执行流程与组件设计入口

> ContextManager 内部完成提交协调与摘要 Hook；SessionManager 负责 SQLite 读写和事务。职责见 [存储设计](./session-storage-design.md)。

记录日期：2026-09-18。

**状态：五张图对应的默认实现已落地，并已合并到 `yourai-harness`：主循环在 `default_loop/`，宿主在 `runtime/`，存储与 compact 在 `storage/`、`context/`，Hook 执行器在 `hooks/`。下文保留的旧 crate 名仅表示历史模块边界。**

实际模块、配置、保证边界与测试见 [DefaultLoop 实现](./default-loop-implementation.md)。本文保留完整目标流程，已落地范围以实现文档为准。

ContextManager 的接口与维护流程见 [压缩算法设计](./context-manager-design.md)：Loop/Host 调 compact 后接收执行结果，工具清理、摘要、持久化协调及摘要 Hook 均在内部完成。已移除 SessionHistory；图中摘要子流程属于 ContextManager。

已落地的接口及 Rust API 变更见 [Core 组件接口契约](./core-contracts.md)。图中的业务行为是各组件的设计契约。

本文保存会话宿主、DefaultLoop、工具执行和 Hook 的完整协作流程，作为后续逐图设计组件的依据。图中的组件名是职责名称，不代表现在已经存在对应的 struct 或 crate，也不要求每个内部模块都成为可替换 Provider。

`Harness::open` 提供统一装配入口；`SessionHost` 实现 SessionRuntime。Loop 接入 12 个 Hook，宿主与扩展接入另外 15 个。实际组件与逐图验证见 [Runtime 实现与验收](./runtime-implementation.md)。

## 阅读导航

- [术语与责任边界](#boundaries)
- [五张图与组件的对应关系](#component-map)
- [图 1：会话宿主](#flow-session)
- [图 2：DefaultLoop 主循环与 compact](#flow-loop)
- [图 3：工具、审批和 MCP 交互](#flow-tools)
- [图 4：外部事件与扩展模块](#flow-extensions)
- [图 5：Hook 注册、执行与结果消费](#flow-hooks)
- [消息、队列与统一控制规则](#messages-control)
- [27 个 Hook 覆盖表](#hook-coverage)
- [逐图组件设计清单](#component-design)

<a id="boundaries"></a>
## 术语与责任边界

```text
TUI / Web
    |
    v
会话宿主：创建、恢复、同会话运行协调、后续任务调度
    |
    v
Core / Agent：装配、Provider 快照、通道、启动、取消、结果交付
    |
    v
DefaultLoop：输入 -> 上下文 -> 模型 -> 审批与工具 -> 继续或结束
    |
    +-- Provider：模型、上下文、权限、沙箱、工具等具体能力
    +-- HookRuntime：在规定节点执行扩展并返回效果
    +-- 扩展工具：工作区、子 Agent、协作任务等可选能力
```

- **Session**：跨越多次执行的对话，拥有会话身份、历史与运行配置。
- **Turn**：一次 `Agent::start()` / `run_turn()` 执行，可以包含多次模型调用和工具调用。
- **Step**：一次模型响应及其发起的一批工具调用；避免与 pi 的 `turn` 命名混淆。
- **Steer**：进入当前 Turn 的追加输入，在下一检查点接纳。
- **Follow-up**：留待当前 Turn 结束，由会话宿主启动后续 Turn 的输入。

前端仍决定用户交互与启动意图，会话宿主承接跨次运行的状态与调度。宿主不进入模型—工具循环；Loop 不直接创建下一 Turn。

历史写入只有一条主路径：**会话宿主打开并绑定 ContextManager，Loop 在正确节点调用它，具体实现负责持久化。** 不再由宿主监听输出重复保存同一消息。

<a id="component-map"></a>
## 五张图与组件的对应关系

五张图对应五组职责，不是五个都需要新增的独立组件，也不是要求新增五个 crate。

| 图 | 主要组件 | 定位 | 当前状态 |
|---|---|---|---|
| 图 1 | SessionRuntime（暂名） | 长期存在的会话宿主，跨 Turn 协调状态与调度 | SessionHost 已实现，SessionCatalog 另管元数据 |
| 图 2 | DefaultLoop | 实现 AgentLoop，推进当前一次执行 | 已在 yourai-loop 实现 |
| 图 3 | ToolExecutor（暂名） | 完成一次工具调用的编排流程 | 建议先作为 DefaultLoop 内部模块，不新增公共 Provider |
| 图 4 | 工作区、配置、子 Agent、协作任务等模块 | 各自独立的可选扩展，通过工具或宿主事件接入 | Workspace、TaskBoard、SubagentTool 已实现 |
| 图 5 | ConcreteHookRuntime | 注册、匹配、执行、解析与聚合 Hook | yourai-hooks 执行；Loop、宿主、扩展调用点均已接入 |

```text
TUI / Web
    |
    v
SessionRuntime                         图 1：会话宿主
    |
    +-- Agent / TurnHandle             已有 Core 运行机制
            |
            v
        DefaultLoop                    图 2：当前执行的主循环
            |
            +-- ToolExecutor           图 3：工具调用内部流程
            |      +-- ToolRegistry / ToolHandler
            |      +-- SecurityProvider / SandboxProvider
            |
            +-- ContextManager         历史、上下文与实际压缩
            +-- ModelProvider          模型请求

可选扩展模块                           图 4
    +-- 注册工具，供 DefaultLoop 调用
    +-- 向 SessionRuntime 投递事件

ConcreteHookRuntime                    图 5
    +-- 供会话宿主、Loop、工具流程和扩展模块在规定节点调用
```

上图表示运行时关系。编译依赖仍通过 core 中的接口倒置：具体 Loop 和 Provider 实现依赖 core，core 不反向依赖具体实现。

### 状态归属与调用边界

| 状态或行为 | 负责组件 |
|---|---|
| 会话身份、历史绑定、运行配置 | SessionRuntime |
| 当前 Agent、TurnHandle、同会话运行互斥 | SessionRuntime 持有并协调，Core 提供执行机制 |
| 尚未投递或 Turn 返回的后续输入 | SessionRuntime；交接时转移所有权，不建立重复消费队列 |
| 当前阶段、steer、当前执行接收的 follow-up、计数与截止时间 | DefaultLoop 的本次 TurnState；不作为共享 Loop 实例的跨会话状态 |
| 一次工具调用的 handler、参数、审批进度和结果 | ToolExecutor 的调用状态；生命周期受当前 Turn 管理 |
| inbox 与请求回复路由 | DefaultLoop 独占消费；ToolExecutor 通过内部交互接口等待对应回复 |
| 何时写入历史、何时 compact | DefaultLoop 协调，包括调用工具内部流程；不在多个模块重复提交同一结果 |
| 历史如何保存、compact 如何实现 | ContextManager 的具体实现 |
| Hook 注册与执行 | ConcreteHookRuntime |
| Hook 返回效果如何影响业务 | 当前触发 Hook 的组件 |

<a id="flow-session"></a>
## 图 1：会话宿主

### 对应组件：SessionHost（实现 SessionRuntime）

管理一段持续存在的对话，生命周期跨越多次 Turn。

```text
SessionRuntime
    +-- 会话身份、历史与配置绑定
    +-- Agent
    +-- 当前 TurnHandle
    +-- 后续输入与任务队列
    +-- 扩展事件和后台任务的会话归属
```

- **输入：** 前端输入、审批回复、取消与关闭请求，以及扩展事件。
- **输出：** 转发给前端的执行事件与结果，以及后续 Turn 的调度。
- **职责：** 创建/恢复会话、路由输入、保证同会话串行执行、启动后续 Turn、恢复与关闭。
- **Hook：** 直接负责 SessionStart / SessionEnd；通过装配的模块接入指令加载、配置等事件。
- **边界：** 不执行模型—工具循环，不重复保存 Loop 已提交的历史。

现有 SessionManager 管会话元数据生命周期；SessionRuntime 管活着的会话及其运行。它们不是同一个组件，也不应仅因名字相近而合并。

```text
TUI / Web
    |
    v
创建 / 恢复会话
    |
    +-- 打开会话历史，绑定 ContextManager
    +-- 装配模型、工具、权限、HookRuntime
    +-- 加载指令 --------------------> [Hook: InstructionsLoaded]
    |
    v
[Hook: SessionStart]
    |
    +-- 应用附加上下文、初始输入、文件监听配置
    |
    v
等待用户输入 <--------------------------------------------------+
    |                                                          |
    v                                                          |
当前有没有运行中的 Turn？                                      |
    |                                                          |
    +-- 有 --> 追加输入送入当前 inbox                            |
    |          审批回复送入当前 inbox                            |
    |          取消操作触发 CancellationToken                    |
    |                                                          |
    +-- 无 --> Agent.start(input)                               |
                   |                                            |
                   +-- 获取 Provider 快照                       |
                   +-- 建立 inbox / outbox                      |
                   +-- 启动 DefaultLoop                         |
                   |                                            |
                   v                                            |
             [执行图 2 的主循环]                                |
                   |                                            |
                   v                                            |
             获取本次执行结果                                   |
                   |                                            |
                   +-- 有 follow-up --> 启动下一次 Turn          |
                   |                                            |
                   +-- 可恢复故障 --> 保存失败，显式继续，不重放副作用       |
                   |                                            |
                   +-- 正常完成 / 取消 / 不再重试 ----------------+

关闭会话
    |
    +-- 若仍有运行：请求取消并等待有界清理
    |
    v
[Hook: SessionEnd]
    |
    v
关闭监听器、处理后台任务、释放资源
```

### 本图约束

- 同一会话不同时启动两个修改同一份历史的 Turn；operation 锁协调 Turn、compact 与关闭，文件锁防止重复打开。
- 会话宿主持有当前 TurnHandle，并协调前端输入、事件转发和执行结果。
- `SessionStart` / `SessionEnd` 属于会话生命周期，不在每次模型请求或每个 Turn 都触发。
- 宿主恢复不能直接重放已完成的工具副作用；需要从已记录的执行状态确定恢复点。
- Loop 内请求重试与宿主级恢复应有明确边界，不能各自独立无限重试。

<a id="flow-loop"></a>
## 图 2：DefaultLoop 主循环与 compact

### 对应组件：DefaultLoop（已在 yourai-loop 实现）

实现已有 AgentLoop 接口，完成当前一次任务。每次 run_turn 创建自己的运行状态。

```text
DefaultLoop
    +-- TurnState：阶段、输入队列、运行计数、截止时间
    +-- 上下文准备与 compact 协调
    +-- 模型请求与流式输出
    +-- ToolExecutor：工具执行子流程
    +-- 交互回复路由、取消、限制检查与统一收尾
```

- **输入：** Core 提供的 TurnContext，包括 Provider 快照、inbox、outbox 和取消信号。
- **输出：** Out 事件和 TurnResult，包括部分输出、用量与待交还输入。
- **依赖：** ModelProvider、ContextManager、工具、安全、Hook 等能力接口。
- **Hook：** Loop 直接调用 UserPromptSubmit、Stop、StopFailure；PreCompact/PostCompact 由 ContextManager 内部调用。
- **边界：** 决定何时调用模型与 compact，不实现模型客户端或数据库，不自行启动下一 Turn。

上下文准备、模型调用和控制处理先作为内部模块，不因流程图拆分就增加新的公共 Provider。

```text
进入 run_turn()
    |
    +-- 校验必要 Provider
    +-- 初始化预算、截止时间、输入队列
    |
    v
从 inbox 读取首条用户输入
    |
    v
[Hook: UserPromptSubmit]
    |
    +-- 拒绝 --> 输出 Out::Notice --> 收尾
    |
    +-- 允许 --> 保存用户输入和 Hook 附加上下文
                     |
                     v
                [统一检查点] <----------------------------+
                     |                                    |
                     +-- 取消 / 断开 / 总超时              |
                     |   / 预算耗尽 -------------> 收尾   |
                     |                                    |
                     v                                    |
               处理已排队的 steer                          |
                     |                                    |
                     +-- UserPromptSubmit Hook             |
                     +-- 允许的输入写入历史                 |
                     +-- 拒绝的输入发 Notice，不写入模型历史|
                     |                                    |
                     v                                    |
               准备模型上下文                              |
               历史 + 系统指令 + 技能/记忆 + 工具定义       |
                     |                                    |
                     v                                    |
               是否需要 compact？                         |
                     |                                    |
                     +-- 否 ----------------------+        |
                     |                           |        |
                     +-- 是                      |        |
                          |                      |        |
                          v                      |        |
                   ContextManager.compact()      |        |
                          |                      |        |
                          +-- 内部清理            |        |
                          +-- 需要摘要时：        |        |
                          |    PreCompact         |        |
                          |    选区 → 分批摘要    |        |
                          +-- 单事务保存 → 内存更新       |
                          +-- 摘要后 PostCompact  |        |
                          |                      |        |
                          v                      |        |
                   重新构建模型请求 --------------+        |
                                                 |        |
                                                 v        |
                                          检查请求预算     |
                                                 |        |
                                                 v        |
                                          ModelProvider   |
                                          .stream()       |
                                                 |        |
                          +----------------------+        |
                          |                      |        |
                          v                      v        |
                   Out::Chunk             Out::Reasoning  |
                          |                      |        |
                          +-----------+----------+        |
                                      |                   |
                                      v                   |
                               模型请求结束               |
                                      |                   |
        +-----------------------------+                   |
        |                             |                   |
        v                             v                   |
     请求成功                       请求失败              |
        |                             |                   |
        |                             +-- 可恢复的上下文溢出
        |                             |      标记强制压缩 |
        |                             |      回统一检查点 -+
        |                             |
        |                             +-- 允许重试的其他错误
        |                             |      有界退避 --> 统一检查点
        |                             |
        |                             +-- 不可恢复 / 恢复次数耗尽
        |                                    |
        |                                    v
        |                            [Hook: StopFailure]
        |                                    |
        |                                    v
        |                                   收尾
        v
     保存完整 assistant 消息
     Out::Message / Out::Usage
        |
        v
     有工具调用？
        |
        +-- 有 --> [执行图 3 的工具流程]
        |                  |
        |                  +-- 本批完成 --> 统一检查点
        |                  +-- Turn 终止 --> 收尾
        |
        +-- 无 --> 有待处理 steer / 可接纳的内部事件？
                        |
                        +-- 有 --> 统一检查点
                        |
                        +-- 无
                             |
                             v
                        [Hook: Stop]
                             |
                             +-- 阻止正常结束
                             |      |
                             |      +-- 记录反馈
                             |      +-- 检查继续次数 --> 统一检查点
                             |
                             +-- 允许结束 --> 收尾

统一收尾
    |
    +-- 取消或等待当前操作清理，有清理时限
    +-- 保存部分输出、工具中断状态
    +-- 关闭 inbox，回收未处理输入和 follow-up
    +-- 汇总用量、结果、结束原因
    |
    v
关闭 outbox，返回结果给会话宿主
```

### Compact 的职责

Loop 决定检查和调用时机，ContextManager 实现压缩和存储。模型上下文是历史的视图，压缩不应把完整执行事实变成无法恢复的数据丢失。

- 请求前根据上下文容量判断是否需要压缩；上下文溢出可触发有限次数的强制压缩与重试。
- 压缩后重新构建请求，不重复写入当前用户输入。
- `PreCompact` 的效果在压缩前消费；压缩成功并保存后触发 `PostCompact`。
- 若实际重新加载指令，额外触发 `InstructionsLoaded`，不因每次构建请求而重复触发。
- 摘要实际尝试调用计入 Turn 预算，失败仍计数；只清理不消耗模型额度。ContextManager 记账，Loop 只汇总。
- 压缩失败且上下文无法发送时，结束并报告错误，不能无限压缩。
- 手动压缩由会话宿主协调到安全边界，复用同一流程，不与当前历史写入并发。

当前接口为 `compact(CompactionRequest, &CancellationToken) -> Result<CompactionResult, YourAiError>`，请求携带触发原因、完整请求环境和执行预算，结果只返回执行状态、前后估算与提示。摘要 Hook、算法和原子提交都在 ContextManager 内部。

### 模型、继续与失败

- 模型重试需要判断是否已经输出可见内容；不能无条件重试并重复显示文本。
- 工具默认不因宿主恢复而自动重新执行；已发生副作用无法通过重放安全撤销。
- `Stop` 可以请求继续正常任务，但不能解除取消、截止时间或预算限制。
- `StopFailure` 在不可恢复的模型失败路径调用，不是所有 Provider 错误的通用 Hook。
- 其他不可恢复的 Provider、历史写入或编排错误进入统一收尾。
- 模型输出截断时，不执行参数可能不完整的工具调用；直接终止当前执行。

<a id="flow-tools"></a>
## 图 3：工具、审批和 MCP 交互

### 对应组件：ToolExecutor（建议作为 Loop 内部模块）

把一次工具调用从准备到结果处理完整执行完，是 DefaultLoop 的子流程，不是替代 Loop 的第二套循环。

```text
ToolExecutor
    +-- 绑定本次 ToolHandler 与 call_id
    +-- 工具调用前后 Hook
    +-- 输入校验、权限合并与审批
    +-- 执行、进度、错误与结果处理
    +-- 通过内部接口请求用户交互
```

- **输入：** ToolCall、本次执行的 Provider、取消与时间约束、进度和交互接口。
- **输出：** 标准化工具结果或 Turn 终止状态，以及工具执行事件。
- **依赖：** ToolRegistry、ToolHandler、SecurityProvider、SandboxProvider、HookRuntime。
- **边界：** Registry 查找工具，Handler 实现动作，Executor 组织完整调用流程。它不发起下一次模型调用，不直接消费 inbox。
- **历史提交：** Executor 产出最终结果，由 Loop 的历史提交路径保存一次；结果持久化与 ToolDone 的先后顺序在本图接口设计中落实。

审批和 MCP 请求交给 Loop 路由；Executor 或适配器等待专属回复。先做内部模块，暂不新增 ToolExecutor trait 或独立 crate。

```text
取出一条 ToolCall，绑定本次 ToolHandler
    |
    v
Out::ToolStarted
    |
    v
[Hook: PreToolUse]
    |
    +-- 应用修改后的 input
    +-- 记录 additional_context
    |
    v
参数校验 + Security 检查
    |
    +-- 拒绝 ---------------------------------------------+
    |                                                     |
    +-- 需要审批                                          |
    |      |                                              |
    |      v                                              |
    |  [Hook: PermissionRequest]                          |
    |      |                                              |
    |      +-- 有有效决策 --> 应用并检查硬性限制            |
    |      |                      |                       |
    |      |                      +-- 拒绝 ----------------+
    |      |                      +-- 允许 --> 执行        |
    |      |                                              |
    |      +-- 无决策                                     |
    |             |                                       |
    |             v                                       |
    |         Out::Ask                                    |
    |             |                                       |
    |         等待 In::Reply                               |
    |             |                                       |
    |             +-- 拒绝 / 审批超时 --------------------+
    |             |                                       |
    |             +-- 允许 --> 检查批准范围和硬性限制      |
    |                              |                      |
    +-- 允许 ----------------------+                      |
                                   |                      |
                                   v                      v
                         ToolHandler.execute     [Hook: PermissionDenied]
                                   |                      |
                         内部策略 / Sandbox      有限重审或生成拒绝结果
                                   |                      |
                                   +-- 进度               |
                                   |   Out::ToolProgress  |
                                   |                      |
                                   +-- 需要 MCP 用户交互  |
                                   |       |              |
                                   |       v              |
                                   |   [Hook: Elicitation]|
                                   |       |              |
                                   |   Hook 答复，或      |
                                   |   Out::Ask           |
                                   |       |              |
                                   |   In::Reply          |
                                   |       |              |
                                   |   [Hook: ElicitationResult]
                                   |       |              |
                                   |   回复 MCP，继续工具 |
                                   |                      |
                      +------------+------------+         |
                      |                         |         |
                      v                         v         |
                    成功                  失败 / 中断      |
                      |                         |         |
                      v                         v         |
             [Hook: PostToolUse]    [Hook: PostToolUseFailure]
                      |                         |         |
                      +------------+------------+---------+
                                   |
                                   v
                          保存最终工具结果
                          Out::ToolDone
                                   |
                                   v
                    下一条工具 / 回主循环 / 终止收尾
```

### 本图约束

- `ToolStarted` 表示开始处理调用，包含 Hook 与审批阶段，不表示副作用已经发生；此含义是本设计约定。
- 工具定义、审批描述与实际执行应绑定同一 handler，避免期间注册表变化导致审批与执行对象不同。
- 权限检查针对 Hook 修改后的最终输入。后续审批若再次修改输入，重新验证批准范围及硬性限制。
- Hook 或用户的 Allow 不能覆盖不可放宽的硬拒绝。`PermissionDenied.retry` 仅请求有限重审，不能绕过拒绝。
- 普通工具错误形成 tool result 交给模型；Turn 取消、总超时和预算耗尽导致统一收尾。
- `PostToolUse` 可以按事件规则替换 MCP 工具输出或补充上下文，不能撤销已经发生的副作用。
- 已批准操作在执行前仍检查取消、截止时间与预算。第一版按串行工具批次组织流程。
- 中断时在有界收尾中为本批剩余调用记录 interrupted_or_not_executed 结果。持久化失败时明确报告，不自动重放副作用。

### 工具执行期间的交互

Loop 仍是 inbox 的唯一消费者。MCP 适配器通过内部请求与回复通道把交互请求交给 Loop；工具不能直接读取 inbox。普通工具提问可复用通道，但不触发 MCP 专属 Hook。

```text
工具 / MCP 适配器
    |
    +-- 内部交互请求 --> Loop --> Out::Ask --> 前端
    |                    ^                      |
    |                    |                      v
    |                    +------ In::Reply -----+
    |
    +<-- 专属回复通道 <-- Loop 按 request_id 路由
```

`Ask/Reply` 使用独立 request_id，payload 关联所属 call_id；过期、重复或无效回复不能投递给其他请求。MCP 回复需要校验 `accept / decline / cancel` 及内容结构。取消或超时时结束等待，并在连接可用时回复 MCP。

当前 ToolContext 已提供可选 ToolInteraction 接口与 ask() 方法；内部请求通道、回复路由、MCP Hook 和超时处理已在 yourai-loop 实现。

<a id="flow-extensions"></a>
## 图 4：外部事件与扩展模块

### 对应组件：多个独立的可选扩展

| 模块 | 负责能力 | 接入方式 |
|---|---|---|
| 工作区模块 | 工作树、工作目录、文件监听 | 工具调用或宿主操作；变化事件交给会话宿主 |
| 配置与指令模块 | 配置变化、指令加载 | 会话装配与宿主事件 |
| 子 Agent 模块 | 创建、管理和回收子会话 | 注册工具；子会话使用自己的宿主与 Loop |
| 协作任务模块 | 任务状态、队友状态 | 工具调用与宿主事件 |

各模块持有自己的资源和业务状态。DefaultLoop 只调用通用工具接口，SessionRuntime 接收需要调度的事件；两者不集中实现扩展业务。图中所有事件共用图 5 的 HookRuntime。

```text
触发模块                     事件与 Hook
--------------------------   ------------------------------------------

应用初始化 / 维护            Setup

指令加载器                   实际加载指令 --> InstructionsLoaded
                             启动、按路径加载、压缩后重载均适用

通知入口                     业务通知 --> Notification --> Out::Notice
                             Hook 自身提示不递归触发 Notification

配置管理器                   候选配置变更 --> ConfigChange --> 决定应用

工作区管理器                 创建工作树 --> WorktreeCreate
                             移除工作树 --> WorktreeRemove
                             工作目录变化 --> CwdChanged
                             监听到文件变化 --> FileChanged

子 Agent 扩展                启动子执行 --> SubagentStart
                             子执行准备结束 --> SubagentStop
                             子执行内部仍运行自己的 DefaultLoop

协作任务扩展                 创建任务 --> TaskCreated
                             准备完成任务 --> TaskCompleted
                             队友准备空闲 --> TeammateIdle
```

这些事件按实际业务发生触发，不在每一模型轮次重复执行。`TaskCreated / TaskCompleted` 不等于 Turn 开始和结束。`FileChanged` 是对已发生变化的通知，不能否决已发生的文件修改。

扩展可以作为工具被 Loop 调用，也可以在会话宿主侧运行监听器，不构成主循环中的固定新一层。

```text
扩展事件 / 后台 Hook 完成
    |
    v
会话宿主登记事件
    |
    +-- Turn 正在运行 --> 内部事件队列 --> Loop 在安全边界消费
    |
    +-- Turn 已结束
           |
           +-- 仅记录 / 展示
           +-- 请求唤醒 --> 检查会话策略 --> 启动后续 Turn
```

RuntimeEvents 提供事件 ID 去重，宿主持久化事件并在 Loop 检查点消费。后台结果不能追溯修改已经执行的工具权限，结束后的唤醒不能写入已关闭 inbox。父工具取消子执行，父宿主关闭子会话；后台 Hook 按 session_id 归属并由 close 回收。

<a id="flow-hooks"></a>
## 图 5：Hook 注册、执行与结果消费

### 对应组件：ConcreteHookRuntime（已有）

位于 yourai-hooks，实现 core 的 HookRuntime 和 HookRegistry 接口。

- **持有：** Hook 注册表、执行配置与相关执行能力。
- **输入：** 管理方的注册/注销请求，以及业务调用方构造的 HookInvocation。
- **输出：** HookDispatchResult，以及后台 Hook 完成事件。
- **调用方：** SessionRuntime、DefaultLoop、ToolExecutor 和各扩展模块。
- **边界：** runtime 负责执行与聚合；触发方负责在自己掌管的流程里应用效果。

这一图主要复用已有实现。下一步设计重点是连接各个触发点和效果消费规则，而不是重新实现一套 Hook 系统。

```text
应用装配
    |
    +-- 配置 --> register_config ---------+
    +-- Rust --> HookRegistry.register ---+
                                         |
                                         v
                                  Hook 注册表
                                         |
Loop / 会话宿主 / 扩展模块                 |
    |                                    |
    v                                    |
构造 HookInvocation                      |
    |                                    |
    v                                    |
HookRuntime.dispatch() <------------------+
    |
    +-- 按事件、matcher、条件筛选；无匹配则返回空效果
    +-- 并行执行 Command / HTTP / Native / Prompt / Agent
    +-- 处理超时、失败策略、解析与校验
    +-- 按注册顺序聚合，避免完成顺序改变结果
    |
    v
HookDispatchResult：common + outcome + runs
    |
    v
调用方消费结果
    |
    +-- 修改输入 / 输出 --> 按当前节点规则应用
    +-- 附加上下文 ------> 记录后加入模型上下文
    +-- 提示消息 --------> 按展示规则发送 Out::Notice
    +-- 阻断 / 停止 -----> 按事件契约改变流程
    +-- 执行记录 --------> 日志与可观测性
    +-- 后台执行 --------> 完成事件交给会话宿主
```

### 执行与效果边界

- Hook runtime 负责匹配、执行、超时、解析、校验与聚合，不直接修改主循环历史、执行目标工具或向用户提问。
- Command/HTTP/Native 已有执行实现；Prompt/Agent 的模型与 Agent 能力需注入 HookModelExecutor。
- 同一 dispatch 的处理器并行运行，结果按规则聚合；不是前一个处理器的修改自动成为后一个处理器输入的 waterfall。
- PreToolUse 权限聚合为 `Deny > Ask > Allow > Pass`，最终仍需与 Security 决策组合。
- `FailurePolicy::Open` 记录非阻断错误，`Closed` 生成阻断效果，调用方按对应节点消费。
- 通用停止请求、`blocking_errors`、工具权限是不同语义；不能只看一个布尔值决定所有事件的处理。
- 阻断 PreToolUse 是阻断工具；阻断 UserPromptSubmit 是拒绝输入；阻断 Stop 是要求任务继续；阻断 TaskCompleted/TeammateIdle 是拒绝对应状态转换。
- 终止、关闭和清理类 Hook 不得无限阻止资源释放。所有回调受取消或有界清理时间约束。

细化单个事件效果时，以 [Hook 协议规范](./hook-protocol.md) 中的类型和 runtime 语义为依据；业务消费规则仍需在对应组件中实现。

<a id="messages-control"></a>
## 消息、队列与统一控制规则

### 当前全部输入输出消息

| 消息 | 使用位置 |
|---|---|
| `In::UserText { text, mode }` | 首条输入、运行中追加输入；图 1、2 |
| `In::Reply { id, payload }` | 审批、普通提问、MCP 回复；图 3 |
| `Out::Chunk { text }` | 模型正文增量；图 2 |
| `Out::Reasoning { text }` | 模型推理增量；图 2 |
| `Out::Message { text }` | 完整 assistant 文本；图 2 |
| `Out::ToolStarted { id, name, input }` | 开始处理工具调用；图 3 |
| `Out::ToolProgress { id, payload }` | 工具进度、子 Agent 事件；图 3、4 |
| `Out::ToolDone { id, name, output, is_error }` | 最终工具结果；图 3 |
| `Out::Ask { id, payload }` | 请求用户交互；图 3 |
| `Out::Usage { usage }` | 模型用量；图 2 |
| `Out::Notice { level, message }` | 拒绝、重试、压缩、Hook 提示等 |

`Chunk` 是增量，`Message` 是完整文本，前端不能把二者重复追加。多条 assistant 消息的边界与标识需要在输出协议设计时明确。用量是增量还是累计值也需统一约定，避免前端重复计数。

取消走 CancellationToken，不新增伪造的取消输入消息。现有协议没有 `Out::Done` / `Out::Error`：流关闭表示结束，join 提供最终结果或错误。

### 队列与等待

```text
inbox（Loop 独占消费）
    |
    +-- 首条 UserText --> 输入 Hook --> 当前任务
    +-- 追加 UserText --> steer 队列 / follow-up 队列
    +-- Reply --------> 按 request_id 路由到待处理交互

模型 / 工具 / Hook / 审批 / 退避等待
    |
    +-- 操作输出与完成事件
    +-- 用户输入和回复
    +-- CancellationToken
    +-- 总截止时间、单次超时
    +-- 内部交互请求（已实现）、后台事件（由后续宿主接入）
```

- 第一版 steer 不强制打断模型请求或工具操作，在主循环检查点生效；整批工具完成后再接纳 steer。
- 当前 UserText 已增加 InputMode::Steer / FollowUp；旧 wire 消息省略 mode 时默认 Steer。In::user_text / In::follow_up 提供对应构造入口，消费时机仍由 Loop 实现。
- 单次工具超时可形成工具失败；总超时终止 Turn。审批超时不代表同意。
- 所有等待必须响应取消与时间限制，但不要求新增一个公共调度 Provider；可先采用 Loop 内部辅助模块。

### 预算与结束

第一版优先明确最大模型轮数、总截止时间、单次操作超时和有限重试。全局 token/费用预算需要模型、压缩、模型 Hook、子 Agent 等全部接入记账，不能只统计主模型请求就称为全局预算。

成功、取消、断开、超时、预算耗尽、失败都走统一收尾。关闭输入后再 drain，晚到输入发送失败时由宿主保留并决定下一次投递，不能静默丢失。

**已落地的公共契约：** TurnResult 为 `Result<TurnOutput, TurnFailure>`，失败侧包含 error 和部分 output。Core 在 Loop 返回后关闭并 drain inbox；Loop 已消费但未处理的输入仍由 Loop 显式带回。AbortReason 已增加 DeadlineExceeded / LimitReached。panic 等未返回路径不能恢复 Loop 的局部状态。

会话宿主的单次驱动入口为 run_next；返回报告前须把 pending 移回宿主队列并清空报告中的 pending，避免前端重复续跑。外层驱动再次调用 run_next 处理 follow-up。详见接口契约文档。

<a id="hook-coverage"></a>
## 27 个 Hook 覆盖表

| 位置 | Hook | 数量 |
|---|---|---:|
| 图 1：会话 | `SessionStart`、`SessionEnd` | 2 |
| 图 2：输入、压缩、结束 | `UserPromptSubmit`、`PreCompact`、`PostCompact`、`Stop`、`StopFailure` | 5 |
| 图 3：工具与权限 | `PreToolUse`、`PostToolUse`、`PostToolUseFailure`、`PermissionRequest`、`PermissionDenied` | 5 |
| 图 3：MCP | `Elicitation`、`ElicitationResult` | 2 |
| 图 4：初始化、指令、通知 | `Setup`、`InstructionsLoaded`、`Notification` | 3 |
| 图 4：工作区与配置 | `ConfigChange`、`CwdChanged`、`FileChanged`、`WorktreeCreate`、`WorktreeRemove` | 5 |
| 图 4：子 Agent 与协作 | `SubagentStart`、`SubagentStop`、`TaskCreated`、`TaskCompleted`、`TeammateIdle` | 5 |
| 合计 | 与当前 HookEventKind 的事件集合对应 | 27 |

图 5 是上述每次 Hook 调用共用的执行过程，不新增事件类型。

<a id="component-design"></a>
## 逐图组件设计清单

以下边界已落实在默认实现中，逐图文件与测试映射见 Runtime 实现与验收文档。

| 图 | 组件/内部模块 | 必须确定的边界与接口 |
|---|---|---|
| 图 1 | SessionRuntime trait 的具体宿主实现 | 会话身份、历史绑定、运行互斥、TurnHandle 所有权、事件转发、恢复点、后续调度、关闭流程 |
| 图 2 | DefaultLoop、TurnState、上下文准备与压缩流程、模型调用流程 | 状态转换、检查点、steer 接纳、预算与时间、压缩输入输出、模型重试、统一收尾 |
| 图 3 | ToolExecutor（Loop 内部模块）、审批管理、ToolInteraction 实现 | handler 绑定、权限合并、request_id、批准范围、取消传播、结果持久化与消息顺序 |
| 图 4 | 工作区、子 Agent、协作任务等扩展 | 事件归属、工具与宿主入口、后台任务生命周期、内部事件与唤醒，不扩大默认 Loop 职责 |
| 图 5 | ConcreteHookRuntime（已有）与调用方的效果消费连接 | 哪个组件 dispatch、每种事件允许的效果、失败策略、通知展示、背景执行与主流程的隔离 |
| 跨图 | 已定义接口的接入与剩余协议细节 | InputMode、TurnResult 的实际消费；RuntimeEvents 身份、增量用量与中断恢复记录已接入 |

上述边界已落实；后续修改保持状态所有权，不为每个方框增加公共 trait。

## 与现有文档和代码的关系

- [Core 组件接口契约](./core-contracts.md)：已定义接口、执行保证边界和迁移说明。
- [架构文档](./architecture.md)：整体分层、既有接口与设计背景。其部分图和示例仍为早期设计，涉及会话宿主和新默认执行流程时结合本文阅读。
- [Hook 协议规范](./hook-protocol.md)：Hook 类型、wire 协议和 runtime 聚合语义。
- [Core 运行机制](../crates/yourai-core/src/context.rs)：当前 Agent、TurnContext、TurnHandle、快照和通道实现。
- [ContextManager 接口](../crates/yourai-core/src/context_manager.rs)：当前历史与压缩接口。
- [工具接口](../crates/yourai-core/src/tool.rs)：当前 ToolContext、ToolHandler、ToolRegistry。
- [Hook 类型与接口](../crates/yourai-core/src/hooks.rs)：当前 27 个事件及结果类型。
- [消息协议](../crates/yourai-core/src/protocol.rs)：当前 `In` / `Out` 词汇。

本文约束完整流程；默认策略、实际组件与保证边界见 [Runtime 实现与验收](./runtime-implementation.md)。
