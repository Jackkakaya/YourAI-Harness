# Hook 全量审查与修复

2026-10-09，范围为 PR10、PR11 合并后的代码及后续简化改造。以下 R / C 编号保留原审查发现，用于对照修复；不代表这些问题仍存在。28 个入口、接口和对象归属见 [最终实现](hook-template-design.md)。

## 设计结论

已把实际工具、模型和压缩流程放回 Tool、Model、Compactor。Turn 保留必要的共同控制和调用记录；授权、MCP 交互用普通函数，没有新的 Executor 类型。Task 和 Subagent 的 JSON 接口直接在业务对象实现，删除空 adapter。重复缓存、登记、配置和 handler 创建路径已合并。

## 问题与当前处理

| 编号 | 原问题 | 当前实现与验证 |
|---|---|---|
| R1 | YOLO 可忽略 Hook 拒绝 | permission::authorize 先处理 Deny；execution 测试验证 backend 从不运行 |
| R2 / R3 | post 或保存失败可重放，单槽覆盖多个结果 | 每调用一条状态 / 结果记录，Running 后禁止重放；execution 验证两个保存失败的真实结果均可结算 |
| R4 | Completed 后仍可继续业务 | ensure_active 拒绝新模型、工具和输入；finish 检查未结数据；execution 验证终态 |
| R5 | checkpoint 保存失败丢 Hook 上下文 | 稳定 StoredMessage 身份，成功才移除；execution 验证 cleanup 仍保存 |
| R6a | 外层取消遮蔽压缩提交状态 | Turn 转发取消并等待内层；宿主 owned supervisor 保留操作锁；Compactor 统一结算 |
| R6b | DB 已提交但确认过晚 | SQLite 事务提交后立即更新共享进度；DefaultContext 保持 dirty 恢复屏障；context 验证取消后恢复实际持久历史 |
| R7 | Task 多缓存覆盖 | SessionHost 唯一 TaskManager，SQLite 按行事务保存；runtime / sqlite 验证共享、并发、恢复 |
| R8 | Task Hook 等待期间关闭后晚写 | 活动所有权覆盖操作，worker 持锁再次检查；runtime 验证关闭竞态和 dropped waiter |
| R9 | 可公开构造第二份 Workspace | new 收窄，host 在资源锁内唯一初始化；所有正常入口取同一对象 |
| R10 | 启动失败或放弃结果遗留资源 | owned 初始化等待接收确认，失败清理；runtime 验证失败、等待中取消、成功结果无人接收 |
| R11 | Hook shutdown 错误跳过资源释放 | 聚合警告，继续持久关闭和释放；runtime 验证原输入可交还、租约可重获 |
| R12 | 动态指令不进入模型请求 | 写入 runtime context，路径去重；runtime 检查真实下一次请求 |
| R13 | system_prompt 设置接受但不生效 | 新设置明确拒绝；旧文件显式迁移，基础 prompt 仍冻结；runtime 验证 |
| R14 | once 并发重复执行 | 注册写锁内先认领，失败 / 取消不恢复；Hook runtime 并发和超时测试 |
| R15 | Worktree 登记先改内存再保存 | next-map 原子写成功才发布；报告实际 Git 效果，重试可采用；runtime Git 集成测试 |
| R16 | dropped ConfigChange 遗留候选 | 私有文件 Drop guard；runtime 验证 |
| R17 | FileChanged 静默吞错误 | 变化事实保留，错误写可见 notice / last_error；runtime 验证 |

## 契约澄清

| 编号 | 当前规则 | 验证 |
|---|---|---|
| C1 | 名字、schema 和实现作为同一个 Tool 注册固定；模型使用同一原子快照 | execution 可变 definition / 重新注册测试 |
| C2 | 可见模型响应在 emit 前保存到 Turn；主动丢弃 exec future 也可收尾 | execution partial message 测试 |
| C3 | Turn 恢复与宿主入口恢复参与取消 / 期限；关闭恢复另作存储结算屏障 | 控制路径与宿主恢复测试 |
| C4 | SubagentStart 的 additionalContext 属于 child；可见 notices 属于 parent | runtime 检查 child 实际请求 |
| C5 | Hook worktree_path 必须是同仓库的真实 Git worktree；保存创建仓库供删除 | runtime 真实 Git 路径验证 |
| C6 | 已确认 SessionEnd 不重发；未知效果有阶段记录和警告，不宣称跨进程 exactly once | runtime 并发关闭与 journal 保存失败重试测试 |

对应源码：[Tool](../crates/yourai-core/src/tool.rs)、[Model](../crates/yourai-core/src/model.rs)、[Turn](../crates/yourai-core/src/execution/mod.rs)、[Compactor](../crates/yourai-core/src/context_manager.rs)、[SessionHost](../crates/yourai-core/src/runtime/mod.rs)、[Workspace](../crates/yourai-core/src/workspace/operations.rs)、[TaskManager](../crates/yourai-core/src/tasks.rs)、[Subagent](../crates/yourai-core/src/subagent.rs)、[Hook 注册与调度](../crates/yourai-harness/src/hooks/runtime.rs)。

对应测试：[execution](../crates/yourai-harness/tests/execution.rs)、[context](../crates/yourai-harness/tests/context.rs)、[runtime](../crates/yourai-harness/tests/runtime.rs)、[sqlite](../crates/yourai-harness/tests/sqlite.rs)、[生命周期故障](../crates/yourai-harness/tests/lifecycle_repairs.rs)。完整运行结果单独记录于 [实施记录](hook-execution-refactor.md)。

## 保留的类型有实际职责

ContextManager / SessionManager 是替换边界；DefaultContext / SqliteStore 是默认实现。CompactionPlan / Job 承载 PreCompact 前准备与锁；私有 guard 承载资源回收。Hook wire、配置、事件、结果 DTO 保留各自数据语义。它们不是附加业务执行层。当前生产源码类型数量与定位见 [类型清单](current-type-inventory.md)，不能把配置和枚举计作 Hook 类。

## 最后一轮审查的 14 项修复

保留原有 8 个业务对象，不增加 Executor、Manager 或委托层。此轮只增加稳定输入身份和所属仓库登记所需的数据，并修正现有状态转换。

| # | 原问题 | 修复与回归 |
|---|---|---|
| 1 | child 丢失普通安全策略和沙箱 | 启动前注入 ToolContext 快照；lifecycle_regressions 验证普通拒绝、沙箱和 startup 热替换 |
| 2 | cwd 改了但文件 / Shell 仍用旧目录 | 执行和审批共用本轮 cwd；真实读、写、编辑、Shell 回归 |
| 3 | 拒绝保存丢回执后仍可执行 | 等待保存前标记 Observed；禁止执行，收尾复用同一拒绝结果 |
| 4 | 模型响应保存失败丢调用 | End 时先接管完整记录和固定调用；未确认提交禁止新业务，finish 补齐结果 |
| 5 | 用户消息已提交后重试重复追加 | 入口分配稳定身份，SQLite 保存原输入；用户和 Hook 上下文原子提交，重试保留原附件且 Hook 只执行一次 |
| 6 | 用量保存失败 / 卡住丢 End 数据 | 完整记录、调用、已知用量先接管；用量保存受取消和超时控制 |
| 7 | 自动维护失败后的 restore 无取消 | 使用 Turn 同一等待控制；卡住 restore 的取消回归 |
| 8 | 指令 A→B→A 最后一次被历史去重 | 仅比较该路径最新值；真实模型请求包含最终 A |
| 9 | linked worktree 按主仓库 HEAD 创建 | 在当前 cwd 执行 Git add 并解析 HEAD；两个 HEAD 的真实 Git 回归 |
| 10 | 丢目录后保留 Git 登记 | 保存创建仓库身份，清理原仓库目标登记；跨仓 cwd、重建和旧格式回归 |
| 11 | 已迁移 tasks.json 仍影响启动 | 数据库确认导入后保存标记，此后不解析备份；损坏备份和标记失败重试回归 |
| 12 | MCP Hook 超时后仍接受答案 | 同一绝对 deadline 覆盖前后 Hook、Ask、最终校验 |
| 13 | 压缩普通错误丢提交状态和 PostCompact | 普通错误按提交进度解释；默认实现已提交后 reload 失败仍返回摘要 / stop_reason 并执行 PostCompact |
| 14 | 大输入 command Hook 管道死锁 | OwnedChild 持有 stdin writer，并发读取输出；262144 字节及异步交接回归 |

替换 CompactionJob 确认提交后，若后续记账或恢复失败，须返回带 stop_reason 的已提交结果及真实摘要，才能执行 PostCompact。返回 Err 仍会明确报告提交阶段，但没有摘要可发送。

回归位于 execution_repairs、lifecycle_regressions、hook_fix_flow、context 和 command 单元测试。SQLite fork 还同步更新新输入身份，避免复制后的消息与其元数据身份不一致。最终验证见 [实施记录](hook-execution-refactor.md)。
