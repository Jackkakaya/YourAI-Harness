# 公共执行入口与自定义 Loop

当前 core 的公共对象直接拥有 Hook 流程：Tool.exec、Model.exec、Compactor.exec，以及 Turn、SessionHost、Workspace、TaskManager、Subagent 的语义操作。DefaultLoop 只决定调用顺序。完整对象划分见 [最终实现](hook-template-design.md)。

## 自定义调度

AgentLoop 协议不变。Turn::open 恢复历史并接纳初始输入；Turn 持有唯一输入消费者和调用账本。工具、模型流程由相应对象执行。

```rust
use yourai_core::prelude::*;
use yourai_core::execution::{Completion, ExecutionConfig, Turn};

struct MyLoop;
impl AgentLoop for MyLoop {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            let mut turn = Turn::open(tc, ExecutionConfig::default()).await?;
            let result = async {
                if !turn.input_accepted() { return Ok(()); }
                let call = turn.enqueue("my_tool", serde_json::json!({})).await?;
                turn.tool(&call.call_id)?.exec(&mut turn, &call.call_id).await?;
                loop {
                    let response = turn.model()?.exec(&mut turn, ModelOptions::default()).await?;
                    for call in turn.pending_tools() {
                        turn.tool(&call.call_id)?.exec(&mut turn, &call.call_id).await?;
                    }
                    if response.calls.is_empty()
                        && turn.complete(response.text).await? == Completion::Completed {
                        return Ok(());
                    }
                }
            }.await;
            turn.finish(result).await
        })
    }
}
```

手动装配使用 Agent::builder().agent_loop(...)；完整装配使用 HarnessConfig.agent_loop。Loop 的局部调度状态在 Stop 要求继续时保留。配置更新只改变后续 turn 的输入设置，不替换 Loop。

## 接口

| 调用 | 含义 |
|---|---|
| `Turn::open(tc, config)` | 恢复并接纳首条输入；恢复也响应控制信号 |
| `turn.accept_input(input)` | 接纳新输入；明确拒绝返回 false 并交还输入 |
| `turn.enqueue(name, input)` | 保存程序发起的调用，返回 ToolCall；不执行 |
| `turn.model()?.exec(&mut turn, options)` | 构造请求、流、恢复和提交，返回 ModelOutput |
| `turn.tool(call_id)?.exec(&mut turn, call_id)` | 执行该身份固定的工具；开始后禁止重放 |
| `turn.pending_tools()` | 当前尚未结算的调用；正常批次在 Loop 中显式迭代 |
| `turn.reject_pending_tools(reason)` | 保存未执行调用的失败结果；不改写已开始调用 |
| `permission::authorize(turn, call, tool, permission)` | 参数校验与授权；同一工具流程复用 |
| `interaction::elicit(turn, request)` | 普通 / MCP 交互；同一工具流程复用 |
| `turn.compact(trigger)` | 转发控制与计账，实际流程属于 Compactor |
| `turn.complete(text)` | Stop 检查；Completed / NeedsMoreWork |
| `turn.finish(result)` | 交还输出与 pending；失败时结算实际结果和部分消息 |
| `turn.wait(future)` / `checkpoint()` | 路由输入、响应控制、提交延迟上下文 |

成功必须先 complete；初始输入拒绝可以直接结束。未结工具阻止新模型请求和完成。Completed 后不能再新增业务操作；finish 不另发 Stop。

模型结束和程序 enqueue 都先保存完整响应与调用账本，再等待历史确认。确认失败时，禁止继续执行工具、请求模型或接纳输入，必须 finish；收尾先保存 assistant，再保存每个调用的稳定结果，最后保存延迟上下文。reject_pending_tools 在等待保存前就把调用标为已观察到拒绝，保存失败不会恢复 Pending。

用户输入在入口取得稳定 id，原始输入随 StoredMessage 保存，用于核对不确定提交。用户消息与 UserPromptSubmit 附加上下文作为同一批原子保存；重试恢复已提交记录后，直接接纳原请求，不重复读取附件、召回、加载 skill 或执行该 Hook。同一身份换内容报冲突。

## 实现侧与执行侧

ToolProvider 只实现 definition、security_context、run。Tool 构造时固定名字和 schema，backend 私有。ToolRegistry 必需 register / unregister / snapshot；其余元数据查询由一次默认实现派生。模型请求和固定工具来自同一快照。

ToolContext.cwd 来自本轮 SessionContext 快照，security_context 也接收同一 cwd。内置文件和 Shell 工具据此解析执行与审批路径；独立调用没有会话快照时使用构造时目录。切换 cwd 不改变自定义安全策略的许可范围。子会话启动前继承本次 ToolContext 的安全和沙箱快照。

PreToolUse 的拒绝优先于 YOLO；YOLO 只跳过审批。修改参数先校验再授权；审批后的修改再次检查 schema 和硬策略。真实工具结果先进入调用记录，再执行 post 和结果保存。post 失败不让调用变回 Pending。

Model 私有 run 保存 emit 前的部分响应。业务恢复只发生在没有可见响应的失败阶段；StopFailure 只报告最终模型故障。不同 Provider 的网络能力与错误分类留在实现侧。

收到 End 后，在任何异步等待前接管完整内容、有效调用和已知用量。用量保存与自动维护后的 restore 都响应取消和超时；截断或身份非法的调用不发布，但保留已知用量。interaction::elicit 的同一绝对期限覆盖前后 Hook、Ask 和最终校验。

ContextManager.prepare_compaction 返回 Complete 或持锁 Summary job。Compactor.exec 负责 prepare → PreCompact → private run → PostCompact；不变与只剪枝没有摘要 Hook。提交后的停止不回滚摘要。请求投影只依赖模型能力，不接收 Compactor。提交取消规则见 [最终实现](hook-template-design.md#必要的失败边界)。

## 28 个 Hook 的入口

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

HookRuntime.dispatch 只匹配、运行、校验和聚合，不修改业务状态。Hook 结果由对应业务入口落实。外部 trait 方法是实现侧协议，直接调用原始 Provider 不获得公共对象的执行契约。
