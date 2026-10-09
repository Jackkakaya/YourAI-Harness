# Hook 最终实现

状态：已在 `refactor/simplify-hook-design` 实现。基于 PR10、PR11 合并后的代码；日期：2026-10-09。本文描述当前源码，替代之前的目标方案。测试结果见 [实施记录](hook-execution-refactor.md)。

## 一句话

**一个业务对象拥有完整流程：公共 exec / 操作入口执行 Hook，再调用自己的私有业务步骤，然后保存结果与收尾。**

用组合实现模板方法。没有通用 Executor 基类，没有为 28 个事件建立 28 个类。具体工具只提供业务和 metadata；注册后的 Tool 不公开 backend。

```text
Loop：决定顺序
  → Tool.exec：PreToolUse → 授权 → private run → PostToolUse / Failure → 保存结果
  → Model.exec：构造请求 → private run → 重试或最终 StopFailure → 保存响应
  → Turn.complete：Stop → 继续或结束

Compactor.exec：prepare → PreCompact → private run → 提交 → PostCompact
SessionHost / Workspace / TaskManager / Subagent：各自公共操作拥有相应 Hook 流程
```

## 正交的职责

| 对象 / 函数 | 负责什么 | 不重复保存什么 |
|---|---|---|
| Tool | 一次工具调用的完整执行流程 | 调用状态保存在 Turn 的同一条记录 |
| Model | 请求、流式响应、模型恢复、失败报告与响应提交 | 部分响应保存在 Turn，emit 前更新 |
| Turn | 本轮输入、输出、取消、限额、调用账本、输入接纳与完成 | 不再实现工具、模型、授权或 MCP 算法 |
| Compactor | 压缩准备、摘要前后 Hook、执行和提交状态报告 | 请求投影不再依赖整个执行对象 |
| SessionHost | 会话运行、资源所有权与关闭 | 唯一 TaskManager、Workspace 和 children 登记 |
| Workspace | 配置、指令、cwd、Git worktree 与变化检测 | 不允许为同一 host 公开构造第二份状态 |
| TaskManager | 任务规则、查询、工具参数和 SQLite 保存 | 没有 TaskTool、TaskStore、JsonTasks 或第二份序号 |
| Subagent | 子会话创建、运行、继续与清理 | 没有 SubagentTool 或另一份 children map |
| permission::authorize | 校验参数和审批 | 普通函数，没有 PermissionExecutor |
| interaction::elicit | 普通 / MCP 提问、Hook 答案、schema 校验 | 普通函数，复用 Turn 的唯一输入路由 |

ContextManager 和 SessionManager 保留原领域名字：前者管理上下文，后者管理持久化。Providers 是注册槽容器，ProviderSnapshot 是本次选择的依赖快照，Agent 是装配后的启动入口；它们不再兼任 Hook 执行器。

HookRuntime、HookRegistry、HookHandler 分别表示分发能力、注册管理和具体处理。默认实现 DefaultHookRuntime 在注册时把配置编译成实际 handler；dispatch 不再按注册来源临时造 handler。once 在注册写锁内先认领；失败、超时或取消也不自动恢复。

## 工具实现和公共调用

```rust
pub use genai::chat::Tool as ToolDefinition;

pub trait ToolProvider: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    fn security_context(&self, input: &Value, cwd: Option<&Path>) -> SecurityContext;
    fn run<'a>(&'a self, tc: ToolContext<'a>, input: Value)
        -> BoxFuture<'a, Result<Value, YourAiError>>;
}

pub struct Tool {
    backend: Arc<dyn ToolProvider>,
    definition: Arc<ToolDefinition>, // 注册时固定，唯一名字和 schema
}

impl Tool {
    pub async fn exec(&self, turn: &mut Turn<'_>, call_id: &str)
        -> Result<ExecutedTool, YourAiError>;
    // 私有 run 装配 ToolContext、交互桥和取消，调用 backend.run。
}
```

这里列的是接口，完整实现直接位于 [tool.rs](../crates/yourai-core/src/tool.rs)。没有 Tool.exec → Turn.exec_tool 的转发链。ToolProvider.run 是 Rust 实现侧协议；正常运行使用 Tool.exec。

名字只取自 definition，不再同时实现 name。ToolRegistry 只要求 register、unregister、snapshot；查找、定义、计数等派生查询各只有一份默认实现。模型请求和待执行工具从同一次 snapshot 取出，即使 Hook 重新注册同名工具，本次调用也继续使用原定义和原实现。

```rust
let call = turn.enqueue("my_tool", serde_json::json!({})).await?;
turn.tool(&call.call_id)?.exec(&mut turn, &call.call_id).await?;
let response = turn.model()?.exec(&mut turn, ModelOptions::default()).await?;
```

enqueue 只登记调用，exec 只执行已登记调用。模型生成的调用和程序发起的调用进入同一账本，没有另一套 call_tool / exec_tool / exec_pending_tools 入口。

## 只保留一份状态

- 工具调用只有一条记录：call、固定 Tool、Pending / Running / Observed、稳定结果 ID。开始后禁止重放；多个调用各自保存结果，post 或保存失败不能互相覆盖。
- 部分模型响应和延迟 Hook 上下文带稳定消息身份。保存成功后才删除；失败时 finish 继续保存。Completed 是终态，finish 同时检查未结调用和未提交消息。
- LoopConfig 只有 steps 和 ExecutionConfig；默认值只定义一次。DefaultLoop 自己持有调度步数，Turn 持有执行额度。
- SessionHost 每个会话只初始化一个 TaskManager 和 Workspace。children 的唯一强引用集合也在 host；确认 child 关闭后才取消登记。

## Task 就是一个具体工具

TaskManager 自己实现 ToolProvider，直接解析 JSON 并进入 create / complete / idle；TUI 也从同一个对象读取 list / version。Task 只是任务数据。

```text
Tool.exec("tasks")
  → 工具 Hook、校验与授权
  → TaskManager.run
      → create / complete / idle
      → 任务 Hook
      → private save → SessionManager → SqliteStore
  → 工具 post 与结果保存
```

两组事件分别表达“调用工具”和“保存任务”，不在 Tool 里按工具名或 action 特判。SQLite schema v5 新增任务表，以 session_id / task_id 为键；事务保证批次原子、身份冲突回滚、完成状态单向变化。旧 tasks.json 只导入数据库里缺少的任务，确认导入后写迁移标记，原文件保留为备份；之后启动不再解析该备份。

任务转换锁覆盖检查、Hook 和保存。数据库提交与唯一缓存更新由同一个 owned worker 完成；等待方消失后 worker 仍结算。会话关闭等待已开始的写入，关闭期间返回的 Hook 不能再启动新写入。

## 必要的失败边界

压缩计划和 CompactionJob 保留：它们用于在 PreCompact 前准备并持锁、在通过后才摘要提交，不是 Hook 转发层。ContextManager 的请求构建只接收 tools 和 model；摘要任务只接收 model / usage，不接收 HookRuntime。

提交进度只有 Pending、Started、Committed。SQLite 在真正事务提交后立即确认 Committed。取消时分别报告未提交、提交状态未知、已提交后中断；未知状态必须 restore，不能自动重放。Turn 和宿主只转发取消，由 Compactor 结算提交状态。

SessionHost 的初始化任务保留所有权直到调用者接收成功实例；启动失败或调用者丢弃启动结果仍清理资源。关闭等待操作与存储结算，再结束 Hook 和释放租约；Hook 清理报错仍进入资源释放。已确认的 SessionEnd 不因保存失败的重试重复发送，未知效果明确报告。

Workspace 的候选配置文件由私有 Drop guard 清理；实际 Git 变化与登记保存分开确认，next-map 保存成功才替换内存。动态指令进入实际模型上下文并去重；system_prompt 不再作为无效运行设置被接受。FileChanged 是已发生事实，Hook 失败可见，不能声称撤销文件变化。

指令去重比较同一路径的最新内容，A→B→A 会发布最后的 A。Git worktree 从当前 cwd 的 HEAD 创建，登记保存路径和所属仓库；目录丢失或 cwd 切换到另一仓库后，仍清理原仓库的目标登记。旧裸路径登记保持兼容。

ToolContext.cwd 与审批使用同一本轮目录；子会话启动前继承安全和沙箱快照。模型与 enqueue 在保存前接管响应和调用，未确认提交阻止后续业务，finish 按 assistant→结果→上下文收尾。用户输入使用稳定身份，原输入与 Hook 上下文原子保存，已提交重试沿用冻结内容。具体恢复规则见 [执行入口](execution.md)。

## 全部 28 个入口

| # | Hook | 入口 | 效果 |
|---|---|---|---|
| 1 | PreToolUse | `Tool.exec` | 执行前修改参数、拒绝或要求审批 |
| 2 | PostToolUse | `Tool.exec` | 成功后处理结果和补充上下文；MCP 输出允许修改 |
| 3 | PostToolUseFailure | `Tool.exec` | 失败或取消后报告；保留已观察到的真实结果 |
| 4 | PermissionRequest | `permission::authorize` | Hook 决定审批，必要时请求用户 |
| 5 | PermissionDenied | `permission::authorize` | 报告拒绝、有限次重新检查；不能绕过硬策略 |
| 6 | UserPromptSubmit | `Turn.accept_input` | 初始输入和 steer 共用接纳流程，通过后提交 |
| 7 | Stop | `Turn.complete` | 决定完成或继续；Completed 后拒绝新业务操作 |
| 8 | StopFailure | `Model.exec` | 最终模型故障报告；不用于普通工具错误或取消 |
| 9 | SessionStart | `SessionHost.open / open_owned` | 初始化后应用上下文、初始输入和监视路径 |
| 10 | SessionEnd | `SessionHost.close / finish_close` | 清理资源、结束通知、持久关闭与释放租约 |
| 11 | TurnCompleted | `SessionHost.run_next` | 本轮有新提交行且成功后通知；不重复保存历史 |
| 12 | SubagentStart | `Subagent.exec` | 先分配身份，Hook 通过后创建 child；上下文交给 child |
| 13 | SubagentStop | `Subagent.exec` | 候选完成时检查；反馈在同一 child 中继续 |
| 14 | PreCompact | `Compactor.exec` | 只在摘要阶段执行；可阻断或补充摘要指令 |
| 15 | PostCompact | `Compactor.exec` | 摘要提交后处理上下文和停止；不撤销提交 |
| 16 | Elicitation | `interaction::elicit` | MCP 提问前可拒绝或提供答案 |
| 17 | ElicitationResult | `interaction::elicit` | 得到答案后修改；最终再次校验 schema |
| 18 | TaskCreated | `TaskManager.create` | SQLite 保存前检查 |
| 19 | TaskCompleted | `TaskManager.complete` | 完成提交前检查；已完成不重复触发 |
| 20 | TeammateIdle | `TaskManager.idle` | 同一任务锁下检查未完成任务后通知 |
| 21 | Setup | `Workspace.setup` | 初始化或维护扩展；应用结果 |
| 22 | Notification | `Workspace.notify` | 应用 Hook 后发送通知；内部日志不递归通知 |
| 23 | ConfigChange | `Workspace.change_config` | 候选校验、Hook 检查、原子保存、更新设置 |
| 24 | InstructionsLoaded | `Workspace.load_instructions` | 读取后、激活前检查；指令提交到实际模型上下文 |
| 25 | WorktreeCreate | `Workspace.create_worktree` | 检查后创建或采用经过验证的 Git worktree，并登记 |
| 26 | WorktreeRemove | `Workspace.remove_worktree` | 检查后移除 Git worktree，再提交登记变化 |
| 27 | CwdChanged | `Workspace.change_cwd` | 验证目录、Hook 检查、提交 cwd 与监视路径 |
| 28 | FileChanged | `Workspace.file_changed` | 变化已发生；保留事实通知并报告 Hook 失败 |

Setup、idle 等本身就是操作边界，不制造空的 run。多个真实工作区动作保留语义方法，不压成 exec(kind, flags)。结果 DTO、协议枚举和内部资源 guard 保留实际职责；完整源码声明见 [类型清单](current-type-inventory.md)。

## 删除与合并

已删除空 Operation / Executor 包装、ExecutionState、HookHost、SessionStartSink、SessionRuntime 转发协议和多余阶段数据。ToolBinding → Tool，ToolHandler → ToolProvider，TurnExecution → Turn，ContextExecution → Compactor，MemoryContext → DefaultContext，Context 注册容器 → Providers；这些名字不保留兼容别名。

TaskBoard / TaskTool 合为 TaskManager，TaskStore / JsonTasks 删除；SubagentTool 并入 Subagent，SubagentBackend 改为只创建的 SubagentFactory。RegisteredHandler 分支、UnsupportedHandler、重复 handler factory、重复取消 guard 与重复时间 / JSON 基础函数也已删除或共用。
