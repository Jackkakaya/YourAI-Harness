> 当前 core 直接拥有 Tool、Model、Turn、SessionHost、Workspace、TaskManager、Subagent 和 Compactor 的执行模板。下表中的 Harness runtime / execution / workspace 路径是兼容再导出；Provider、SQLite及装配仍在Harness，任务JSON适配已并入core TaskManager。当前接口见 [execution.md](./execution.md)。

# Runtime 实现与验收

> 后续架构决定：默认由统一用户级 Server 持有和驱动会话，所有前端连接该服务，按 `session_id` 复用宿主并同步状态。尚未实现；当前 TUI 仍在进程内创建宿主。详见 [默认共享 Server 决定](./architecture.md#已确定的后续方向默认共享-server2026-10-01)。

> 会话记录使用 SQLite；ContextManager 完成消息提交和模型视图更新，公共 Compactor.exec 处理摘要 Hook，公共工具操作处理未结调用清理。详见 [存储设计](./session-storage-design.md) 与 [ContextManager 设计](./context-manager-design.md)。

`yourai_harness::Harness::open` 负责装配。core 实现公共业务流程、协议和 turn 运输；Harness 提供默认 Loop、Provider、SQLite 和实际装配，core 不反向依赖 Harness。

## 五张图对应的代码

| 图 | 实现 | 验证 |
|---|---|---|
| 1 会话宿主 | `runtime/mod.rs`：SessionHost 业务；`runtime/lifecycle.rs`：会话生命周期包装 | runtime 测试：队列恢复、驱动取消、并发关闭、跨会话事件隔离 |
| 2 主循环与 compact | `default_loop/`：调度；`execution/`：共享执行；Harness `context/`：算法；core `Compactor`：摘要生命周期 | loop 测试：压缩调度、限制、重试与收尾；runtime/context 测试：真实摘要、完整归档、手动与自定义调度压缩 |
| 3 工具、审批、MCP | core `tool.rs` / `permission.rs` / `interaction.rs`；Harness 的 ToolSet 与 PolicySecurity | loop/execution 测试：参数重检、审批、MCP、批次中断；runtime 测试：权限持久化、完整 tasks 调用 |
| 4 扩展 | core `workspace/` / `subagent.rs` / `tasks.rs`；Harness `collaboration/` / `memory/` / `skills/` | runtime 测试：Git worktree、配置恢复、任务状态、独立子会话 |
| 5 Hook | `crates/yourai-harness/src/hooks/`；`assembly/model_hooks.rs` 模型执行器 | Hook、Loop 与 runtime 测试：注册、匹配、聚合、agent Hook、后台唤醒、关闭进程 |

主要测试文件：[Loop 测试](../crates/yourai-harness/tests/loop_flow.rs)、[Runtime 测试](../crates/yourai-harness/tests/runtime.rs)、[Context 测试](../crates/yourai-harness/tests/context.rs)、[SQLite 测试](../crates/yourai-harness/tests/sqlite.rs)。

## 装配与前端入口

```text
Harness::open(config, model)
  +-- SessionCatalog + DefaultContext + SessionHost
  +-- AgentLoop + 公共 execution + ToolSet + PolicySecurity
  +-- DefaultHookRuntime + DefaultHookEvaluator
  +-- MeteredModel / ModelBudget
  +-- LocalUsage
  +-- extensions=true 时：LocalMemory / LocalSkills / Workspace / tasks / subagent

TUI 键盘 / Web 请求 -> host.submit(In)
TUI 渲染 / Web SSE  <- host.serve(limits, sink, cancel)
一次驱动           -> host.run_next(...) / run_until_idle(...)
手动压缩           -> host.compact(...)
中断当前执行       -> host.interrupt()
退出               -> harness.close()，接收剩余输入
```

[终端示例](../crates/yourai-harness/examples/run.rs)：

```sh
cargo run -p yourai-harness --example run -- <model-id> <prompt>
```

模型凭据由 genai 从环境读取。示例输出 JSON 事件，并拒绝交互审批；完整 TUI/Web 界面不在这个库中。前端实现 OutSink，将 Ask 对应的 Reply 交回 submit 即可复用同一条执行路径。

基础装配和子会话共用 `harness::assemble`；子会话显式继承父会话的 ContextPolicy。扩展默认关闭；调用 workspace() / 注册 watch path 也可显式启用工作区能力。

## 持久化与恢复

session_dir/sessions.sqlite3 保存元数据、消息/摘要和用量。宿主队列、配置、权限、任务、记忆、技能仍保存在会话目录；宿主持有 host.lock。DefaultContext 内部先提交数据库再更新活跃视图。

Harness 恢复及 `runtime::restore` 在初始化系统提示、写会话元数据和装配 provider 前取得执行 lease。lease 持有 `host.lock`，随后直接转交给 Host，途中不释放或重复获取；锁拒绝的恢复不会改写活动会话，装配失败会释放 lease。

- follow-up 在接纳前落盘；运行中 steer 记入 active。Turn 返回的 pending 转移回宿主队列；失效 Reply 不得进入后续 Turn。
- 恢复时保留未开始的输入，将上次 active 标记为 interrupted。缺少结果的工具调用补中断结果，不自动重放工具或整次失败 Turn。调用方可读取 last_error / interrupted_inputs 后决定继续。
- Core 返回 `LoopTerminated`（watchdog 强制丢弃或 panic）时，Host 进入 Closing，将全部 active 输入保留到 interrupted。未读 steer 已在 active 中，不再从 Turn.pending 重复入队；独立排队的 follow-up 由 close 归还。
- compact 调用模型生成摘要，成功且候选来源仍 active 才事务提交。模型视图使用系统指令、摘要和后续消息；完整 transcript 保留。工具调用及内部事件身份查询覆盖归档，压缩不会让已完成调用重新执行。
- 手动 compact 与 Turn 互斥，有取消和总截止时间。系统指令保留；显式加载或重载指令才触发 InstructionsLoaded。
- close 取消执行、停止监听、关闭子会话、执行有界 SessionEnd、回收后台 Hook、释放宿主锁。并发关闭不重复交还输入或触发 SessionEnd。应用应显式 close；Drop 只是取消兜底。

## 扩展与后台事件

Workspace 执行指令读取、通知、候选配置验证、工作目录切换、Git worktree 创建/移除和文件监听。ConfigChange 拒绝时不应用候选配置；成功配置用于后续 Turn 并在恢复后加载。目录监听递归检测文件新增、修改、删除，不跟随目录符号链接。默认轮询 250ms。

TaskManager直接实现tasks工具，保存到现有sessions.sqlite3；工具与TUI读取同一对象。完成前允许TaskCompleted阻止；队友有未完成任务时不得报告idle。旧tasks.json原子导入，数据库已有任务不会被旧文件覆盖。Subagent创建独立宿主和Loop，转发输出与交互；SubagentStop可要求有界继续，取消和关闭传播到子会话。

Workspace、TaskManager和Subagent在自身公共操作中执行Hook，业务步骤私有；不另设Operation接口。任务保存复用host阻塞worker和写入锁，提交及缓存更新一起完成。公共入口与自定义Loop的关系见[公共执行层](./execution.md)。

RuntimeEvents 按 ID 去重，运行中在 Loop 检查点消费。空闲时的 asyncRewake 在宿主允许的情况下生成 follow-up，由 serve 或外层驱动执行。普通通知不自动启动模型；后台结果按 session_id 路由。

## 28 个 Hook 的调用方

| 调用方 | Hook |
|---|---|
| SessionHost | SessionStart、SessionEnd、TurnCompleted |
| 公共 Compactor.exec | PreCompact、PostCompact（自动/overflow/手动共用） |
| 公共 execution 完成、输入和模型入口 | UserPromptSubmit、Stop、StopFailure |
| 公共 execution 工具、审批和交互入口 | PreToolUse、PostToolUse、PostToolUseFailure、PermissionRequest、PermissionDenied、Elicitation、ElicitationResult |
| Workspace | Setup、InstructionsLoaded、Notification、ConfigChange、CwdChanged、FileChanged、WorktreeCreate、WorktreeRemove |
| SubagentTool | SubagentStart、SubagentStop |
| TaskManager | TaskCreated、TaskCompleted、TeammateIdle |

DefaultHookRuntime 支持 command/http/native/prompt/agent。DefaultHookEvaluator 提供 prompt 和 agent 的模型执行；评估 Agent 不装配 HookRuntime，避免递归。异步 command 归属于会话，显式关闭时回收；Unix 使用独立进程组终止 shell 及组内子进程。

## 默认策略的保证边界

- Harness 的共享 ModelBudget 对主模型、compact、模型 Hook、子 Agent 统一记录调用次数并累计已知 token，但只做观测与限速（RequestPolicy 的 RPM/cooldown），不再设调用次数或 token 准入上限——与 OpenCode 一致：执行边界由 per-turn 的 `steps`、deadline 和各超时构成，没有跨 turn 费用硬上限。用量按当前装配生命周期累计，恢复后重新计数，历史用量仍保存。
- 超时策略以 OpenCode `70a24697ea0028e19f22712fd63059538cb4bee7` 为参照：普通 provider 操作、用户审批、Hook 调度、压缩和持久化清理不再默认设置总时限；相应配置为 `Option<Duration>`，`None` 不启动计时器。显式 turn/compaction deadline、取消和断连仍生效。模型 HTTP 响应头和后续原始字节读取默认各 300 秒，覆盖主循环、压缩、模型 Hook 和子 Agent；SSE 心跳与尚未解析完成的事件也属于读取进展，持续输出不受总耗时限制。压缩通过流式请求收集最终回复。传输层由仓库内固定版本的 genai 补丁实现（`vendor/genai/PATCHES.md`）；ModelProvider 声明 `uses_transport_timeouts()` 后，主循环只保留显式总 deadline/取消/断连，不再叠加模型事件间隔计时。默认 Loop/TurnLimits 的模型时限通过 ChatOptions 传给传输层，MeteredModel 与 SourceModel 保留该能力。未提供传输层计时的自定义 ModelProvider 继续使用原事件级计时兜底。模型底层显式 HTTP 请求 timeout 仍独立生效。
- 工具不再默认被统一的 610 秒外层计时器截断；由工具自己负责默认超时（shell 默认 120 秒、最大 600 秒，webfetch 自有网络时限），宿主仍可用 `LoopConfig.tool_timeout` / `TurnLimits.tool_timeout` 增加上限。工具内部问题没有 deadline 时也可等待用户回复。
- 重试最多 5 次，初始 2 秒、指数倍率 2、最多 25% 抖动；无响应头时上限 30 秒，服务端 Retry-After 优先，所有重试等待封顶 `i32::MAX` 毫秒。计量包装层不再把缺失的提示变成零秒提示；共享 provider cooldown 默认关闭（`cooldown_seconds = 0`），不再额外等待 60 秒；用户显式配置的 cooldown/RPM 和服务端提示仍可施加共享准入限制。[OpenCode retry.ts](https://github.com/anomalyco/opencode/blob/70a24697ea0028e19f22712fd63059538cb4bee7/packages/opencode/src/session/retry.ts)
- 取消收尾区分工具宽限和历史写入：先取消工具 token，再给工具 `tool_cleanup_timeout`（默认 250 毫秒）完成收尾；保留宽限期内返回的真实结果，超时后丢弃 future 并记录中断。取消后的失败 Hook 只使用同一宽限期的剩余时间，不再另加等待。Host 对 cancel 只消费一次，避免无总清理超时时忙循环；可选 `cleanup_timeout` 仍支持隔离不合作的执行。正常 Harness/子 Agent 关闭使用 `close(None)` 等待收尾，调用方可用 `close(Some(duration))` 显式限制关闭期限。[OpenCode cleanup](https://github.com/anomalyco/opencode/blob/70a24697ea0028e19f22712fd63059538cb4bee7/packages/opencode/src/session/processor.ts#L553-L610)

- steps 语义与 OpenCode 对齐：计 agentic iteration，重试与压缩不消耗 step；最后一步不绑定工具定义、请求级追加 assistant 角色 MAX_STEPS_PROMPT prefill（不写入历史）并设 `tool_choice=none`，模型仍返回工具调用时按 failUnsettledTools 语义回写失败结果并继续循环（有界）。
- 附件协议双形态（`UserAttachment.data`，untagged 保证旧 base64 wire 兼容）：内联 base64 媒体（图片/PDF/音频），或文件引用 `{"path": "...", "lines": [10, 20]}`（OpenCode FilePart 语义，行窗口 1-based 闭区间）。引用形态由 harness 在 `accept_input` 统一解析：文本文件读出并截断为 text part（默认 50k 字符上限 + 行窗口），图片走归一化阶梯，目录展开为一级列表，audio/PDF 转 Binary；前端只传路径不做任何读取。图片附件入库前归一化（对齐 OpenCode image.ts）：5MB base64 与 2000x2000 上限，超限先缩放（Lanczos3，PNG→JPEG 降质阶梯）再拒绝；audio/PDF 尺寸由 provider 侧约束。上下文预算中图片按 provider 计费公式估算（Anthropic (w·h)/750、OpenAI tiles），PDF/音频按字节粗估。
- ModelProvider 只提供 complete / stream_events；genai 原始流转换封装在 GenaiModel。UsageTracker 只通过 record_event 记录原始用量事件。
- PolicySecurity 持久化会话级精确工具规则，支持 addRules/removeRules/replaceRules；不支持的目的地、模式或范围表达式报错，不扩大授权。硬拒绝不能由 Hook 覆盖。
- 外部副作用和文件历史不能提供跨系统 exactly-once。崩溃后的不确定调用不会自动重试。Provider 自行脱离的任务、进程组之外的进程不在取消保证内。
- 默认装配不包含业务 shell/file/MCP 客户端或操作系统沙箱。通过 ToolProvider / SandboxProvider 接入；core Tool 已实现共用的权限、取消、交互和结果流程。
- 自动测试使用可控模型，不请求在线模型；GenaiModel 的在线连通性未验证。


## 策略归属与持久化更新

模型超时统一由所选模型携带；Hook 共同校验、流式 HTTP 错误元数据、异步 journal
事务和有序请求日志的实现与取消语义见[代码质量整改](./architecture-quality-repairs.md)。
