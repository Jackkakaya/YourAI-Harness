# ContextManager 设计

状态：已实现。ContextManager 内部完成恢复、追加、清理、摘要及事务协调；SQLite schema v2 保存清理标记。见 [会话存储](./session-storage-design.md)。

## 1. 整体架构

```text
SessionHost：管理会话，启动执行，互斥处理手动压缩
    │
    ▼
DefaultLoop：调用模型、执行工具，判断何时维护上下文
    │
    ▼
ContextManager：管理当前上下文及其变更流程
    ├─ restore：恢复
    ├─ append：追加
    ├─ build_request：读取并组装模型请求
    └─ compact：清理或摘要，保存后更新内存
         │
         ▼
SessionManager：读取记录、保存消息、清理标记与摘要事务
         │
         ▼
SQLite
```

| 组件 | 唯一职责 |
|---|---|
| DefaultLoop | 控制执行顺序、触发维护、管理执行预算与重试 |
| ContextManager | 决定给模型看什么，并完成上下文变更 |
| SessionManager | 实现记录读写与事务 |

ContextManager 允许调用 SessionManager，但不实现 SQL、不管理数据库连接。所有修改统一采用“先保存、后更新内存”。Loop 不再手工串联保存、append、reset，也不把 SessionHistory 作为新的顶层组件。

## 2. 四个公共操作

```text
restore()                    从 SessionManager 恢复已提交的活跃上下文
append(messages)             保存新消息，成功后追加到内存
build_request(system, tools, execution)  只读组装请求，返回用量估算和预算状态
compact(options, execution, cancel)     内部完成清理/摘要、提交和内存更新
```

- ContextManager 绑定 session_id；构造时只装配 SessionManager 和策略。ContextExecution 从本次执行快照取得 model、hooks、usage 和 Hook 基础信息，不在 ContextManager 内保存第二份 Provider 绑定。
- system/tools 由调用方提供；compact 使用本次请求的 system/tools 和预算配置进行完整请求估算，不能只估算消息。
- append 的消息 ID 在调用前确定，失败重试复用同一 ID。
- restore 用于首次加载和故障恢复；普通 append/compact 成功后不要求调用方再次 restore。
- prune、选区、候选、分块总结、reset 都是内部步骤，不公开为独立接口。
- build_request 不修改消息；三个写操作按会话串行执行。
- append 成功直接应用存储返回的已提交消息，不重读完整历史；首次加载、手动刷新及不确定提交恢复才读取数据库视图。
- ContextPolicy 是唯一的压缩策略来源，Loop 不另设 compact_threshold / compact_target。

```text
CompactionRequest
  trigger: Threshold（自动） | Overflow | Manual
  本次请求环境与预算
  可选摘要要求、总截止时间和剩余调用预算

CompactionResult
  action: Unchanged | Pruned | Summarized
  tokens_before
  tokens_after
  reason（例如未达阈值、没有可压缩内容、冷却中）
  stop_reason / notices（提交后 Hook 的停止与提示）
  usage（已记账用量，只用于展示，不再次写 UsageTracker）
```

compact 返回执行结果，不返回待调用方提交的摘要或消息范围。失败返回错误。内部生成的新摘要可供 PostCompact 使用，不依赖公共返回值暴露。

## 3. 预算与配置

```text
B = 可用输入上限 - 尚需扣除的输出预留 - 安全余量
U = 本次完整请求估算（system + tools + 摘要 + 消息 + 媒体）
T = B - 自动摘要提前预留量
```

U ≥ T 时考虑摘要；U > B 禁止发送。独立 input limit 按 provider 语义计算，不重复扣除输出预留。窗口未知时要求配置或报告未知。

真实 usage 基线只适用于同模型、同 system/tools、同请求消息前缀；仅追加消息时累加新增估算。模型、system/tools、展示内容或摘要变化后失效。未获得基线时，文本部分按序列化字节数 / 3 向上估算，媒体使用模型适配器估算；不是 provider 的精确 tokenizer。窗口未知时 build_request 明确返回 None，TUI 提示配置，摘要调用报配置错误。

| 配置 | 初版设置 |
|---|---|
| compact 内部主动清理阶段 | 默认关闭，启用后可独立满足维护目标，无需调用总结模型 |
| 近期保留预算 | 目标 20k tokens，受可用窗口限制 |
| 摘要目标长度 | 默认 2k tokens，可配置 |
| 输出预留 / 安全余量 / 自动摘要提前量 | 4096 / 1024 / 4096 tokens |
| 单条输出上限 | 16000 字符，含 JSON 包装和读取提示 |
| 清理门槛 / 最小回收 / 再次增长 | 与自动摘要共用 T / 1024 / 2048 tokens |
| 摘要最小节省量 / 冷却 / overflow 重试 | 256 tokens / 同一消息及请求环境不重复自动维护 / 默认 1 次 |

## 4. 三种处理

| 处理 | 执行位置 | 调用总结模型 | 保存什么 |
|---|---|---|---|
| 单条工具结果限长 | build_request 内部 | 否 | append 已保存完整原文；限长不另写记录 |
| 旧工具输出清理 | compact 内部第一阶段 | 否 | 清理标记，消息仍 active |
| 历史摘要 | compact 内部第二阶段 | 是 | 新摘要、来源消息及旧摘要的状态变化 |

### 4.1 单条工具结果限长

```text
① 工具返回完整结果，Loop 完成相关 Hook
② Loop 调 ContextManager.append(result)
③ ContextManager 调 SessionManager 保存，成功后追加到内存
④ build_request 检查单条结果大小
    ├─ 未超限 → 正常内容
    └─ 超限 → 有界预览 + 截断标记 + 完整结果读取入口
```

超长正文放入合法 JSON 预览，保留头尾、call_id、ok/error/exit_code 和读取入口；包装与提示合计受字符上限约束。无法容纳必要状态时明确报错。媒体用量交给 ModelProvider.media_tokens；默认适配器尚未提供估算，遇到媒体明确报错，不按 base64 长度伪估算。

原文、call_id 和 active 状态不变。完整结果通过 `read_tool_result(call_id, offset, limit)` 工具调用 SessionManager 分页读取，每次返回同样受展示上限约束。

### 4.2 compact 内部：先尝试清理旧工具输出

自动维护开启清理阶段时，检查请求用量、距上次清理后的新增内容量、可清理结果和预计回收量。

```text
① 选择近期保护区之外、尚未清理的合格结果
② 在内部副本上替换成“简短状态 + call_id + 读取入口”
③ 计算整个批次的回收量
    ├─ 无候选 / 回收不足 → 保持原视图，进入摘要判断
    └─ 满足回收门槛 → 计算清理后的完整请求用量
④ 判断是否还需摘要
    ├─ 自动维护目标已满足 → 保存标记 → 更新内存 → 返回 Pruned
    └─ 仍需摘要 → 携带内部清理方案，进入摘要阶段，暂不更新内存
```

只选已完成且曾被后续成功模型请求包含的结果。初版保守要求工具 JSON 明确带 `ok: true`、没有 error，且未设置 `preserve_context: true`。跳过未闭合、未消费和状态未知结果。消费标记只留内存；重启后须再次被成功模型请求包含才可清理。

手动触发明确要求摘要，跳过单独清理阶段。overflow 可采用允许的清理方案，但不能仅因本地估算变小就认定服务端已接受，仍受恢复次数和进展检查约束。

### 4.3 compact 内部：再决定是否摘要

```text
① 判断触发来源、阈值、冷却及可压缩选区
    └─ 无须摘要 → 按已完成处理返回 Unchanged / Pruned
② PreCompact（仅在实际进入摘要阶段时执行）
③ 固定快照，选择保留区和摘要区
④ 旧摘要 + 选区原文 → 模型总结；必要时按安全边界分批
⑤ 校验：非空、未截断、满足节省量、工具配对完整
⑥ 调 SessionManager 原子提交摘要变更及需要保留的清理标记
⑦ 成功后更新内存，触发 PostCompact，返回 Summarized
```

来源标识传入摘要 Hook；Hook 由 ContextManager 内部通过注入的 HookRuntime 调用，Loop/Host 不重复触发。清理方案与摘要方案在内存中计算，失败不留下半应用状态；同一次维护若需两类变更，最终合并在一个数据库事务提交。

```text
压缩前：system | 旧摘要 | 较早消息........ | 最近消息.... | 当前执行片段
压缩后：system | 新摘要                  | 最近消息.... | 当前执行片段
                       ↑
              旧摘要 + 选中的较早原文
```

选区规则：

- 从尾向前保留近期内容，软预算取配置值与 B/3 的较小值；overflow 缩到 B/8，已超过 B 时取消软保留。最新消息始终保护；长 Turn 可在完整工具批次之间切分。
- system 独立保留，最新真实用户请求和未完成工具批次受保护。
- assistant 工具调用及全部对应结果不能跨摘要/保留边界。
- 总结使用选区原文，不只总结被清理的占位内容。
- 分批调用共用取消、截止时间、执行预算，滚动摘要最终只提交一次；总结模型报告 overflow 时缩小分批输入预算，单个完整批次仍放不下则报错。
- 单个不可拆内容超限、多模态内容无法安全处理或摘要无效时返回错误，不提交。

摘要覆盖：任务目标与约束、已确认决定、已完成动作、关键文件与标识、当前进度和剩余工作。

### 4.4 多模态边界

- compact 只处理已有消息，不打开资源、不加载附件、不生成图片预览，也不调用读取工具。
- 保留区的原始消息不变；摘要区的媒体替换为文字占位，保留已有名称/URL，总结已有文字观察，不推断未见细节。
- 资源保存、权限检查、文本分页、图片/PDF/视频解析属于后续工具模块。当前不提供 read、read_asset 或 TUI /attach。
- 媒体估算由模型适配器负责；不支持的媒体不能静默按普通文本发送。

## 5. 三条调用入口

### A. 自动维护：下一次主模型调用前

```text
① Loop 接纳输入/steer，等待工具批次收尾
② build_request 返回请求用量和预算状态
③ 达到维护条件时，Loop 调 compact(Threshold)
    └─ ContextManager 内部：清理 → 必要时摘要 → 保存 → 更新内存
④ Loop 根据结果重建请求
    ├─ U ≤ B → 调用主模型，不立即重复处理同一历史
    └─ U > B → 停止并报告不可安全压缩；不强删受保护内容
```

首次输入、工具批次后继续、后续用户输入均经过此检查。没有下一次模型调用时不额外启动后台维护。自动冷却或没有新选区时可返回 Unchanged；原请求只有在 U ≤ B 时才能发送。

### B. overflow：主模型已拒绝请求

```text
① Provider 明确返回上下文超限
② Loop 检查取消、超时、恢复次数和安全重试条件
    └─ 已产生可见响应或本次工具已开始执行 → 不自动重试
③ 调 compact(Overflow)，内部缩小近期软保留区、扩大摘要区
④ 返回 Pruned / Summarized 且有实际进展 → 重建请求，检查 B，再重试
⑤ 再次 overflow → 消耗下一次恢复机会；无进展或次数耗尽则停止
```

绕过自动阈值和普通自动冷却，但不绕过保护规则与执行预算。不重复追加用户输入、不重放已完成工具。总结模型自身 overflow 由内部摘要算法处理，不重新进入此入口。

### C. 手动：用户执行 `/compact`

```text
① 前端把请求交给 SessionHost
② 宿主尝试取得会话执行互斥
    ├─ 正在执行 Turn / 另一次压缩 → 返回 Busy
    └─ 空闲 → 调 ContextManager.compact(Manual)
③ 内部忽略自动阈值与自动冷却，执行摘要流程
④ 宿主展示 Summarized / Unchanged / 错误，释放互斥
```

正常执行与手动压缩统一调用 AgentLoop.request_system，使用同样的 system、指令和已选技能。

不先单独清理，不自动续写、不自动消费排队输入。取消、截止时间、调用预算和保护规则仍生效。下一次正常执行重新检查请求预算。

## 6. 持久化与失败语义

SQLite v2 增加 nullable `messages.tool_output_pruned_at`（UTC 毫秒），启动时从 v1 事务迁移。标记与 status 分开。

| 记录状态 | 原文 | 发给模型 |
|---|---|---|
| active，未清理 | 保留 | 正常内容或有界预览 |
| active，已清理 | 保留 | 简短状态、call_id、读取入口 |
| compacted | 保留 | 原消息不发送，由摘要承接 |

- restore 读取清理标记，fork 复制标记；配置变更不自动清除标记。
- 计算或事务提交失败不应用内存变更；SQL 完成状态不确定时，由 ContextManager 恢复已提交视图后才允许继续。
- PostCompact 发生在提交之后；其失败或停止请求不回滚已提交摘要。错误需区分提交前失败与提交后停止。
- ContextManager 通过注入的 UsageTracker 记录已发生且已知的模型用量，包括验证/提交失败；不另存一份上下文用量账本。记账失败不得被当作未发生模型调用或未提交摘要。
- 摘要调用使用调用方传入的剩余调用额度，失败/取消仍计入已尝试调用；默认手动上限 8 次。Harness 的共享 MeteredModel 同时限制主模型、摘要、Hook 模型和子 Agent。摘要 Hook 单次默认 30 秒，并受整次 compact 截止时间约束。
- 自动摘要计算失败但原请求在 B 内时，Loop 可按恢复策略继续；持久化状态不确定时必须先恢复。
- 压缩期间新到的 steer 留在队列，下一个安全点消费。

## 7. 验收清单

- 四个公共操作分别负责恢复、追加、读取、压缩；调用方不提交候选或手动 reset。
- append 保存成功后更新内存，重试复用消息 ID。
- build_request 限长不修改原文，结果原文可分页读取。
- compact 仅清理时返回 Pruned，不调用总结模型、不触发摘要 Hook。
- 清理后仍需摘要时，两类变更原子提交；失败无半应用状态。
- 自动/overflow/手动入口的触发、冷却、互斥、进展和重试上限。
- 用户请求保护、并行工具配对、长 Turn 切分、分批总结预算。
- usage 基线失效、固定开销超限、已知失败调用记账。
- Hook 不重复触发，提交后停止不假装回滚。
- 取消、不确定提交恢复、重启与 fork 后视图一致。

## 8. 实现位置与配置

- `yourai-core/src/context_manager.rs`：四个操作及只读视图/身份查询。
- `yourai-runtime/src/memory_context.rs`：装配、串行写入、不确定提交恢复。
- `yourai-runtime/src/memory_context/projection.rs`：请求预算、usage 基线、输出预览。
- `yourai-runtime/src/memory_context/compact.rs`：选区、内部清理、分批摘要、Hook 与提交。
- `yourai-runtime/src/tool_result.rs`：会话范围内的原始工具结果分页读取。

TUI 使用 `yourai.toml` 的 `[context]` 配置；示例见 `yourai.example.toml`。必须按实际模型填写 context_window 或 input_limit。MemoryContext::memory 仅用于无持久化需求的临时评估 Agent；正式会话通过 ContextServices 注入 SessionManager 和策略，Loop / Host 从当前 ProviderSnapshot 构造 ContextExecution。
