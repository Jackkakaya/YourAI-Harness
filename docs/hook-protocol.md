# Hook 协议规范

> 本文是 YourAI Hook 系统的权威协议参考，同时记录 Claude Code 的 wire 协议和
> YourAI 的内部类型化协议，以及两者之间的映射关系。
>
> - Claude Code 协议核对版本：commit `a371abb`（2026-04-05）
> - YourAI 实现：`yourai-hooks` crate
> - 完整 Claude 源码核对：[`claude-code-hook-protocol.md`](./claude-code-hook-protocol.md)
> - 设计背景：[`hooks.md`](./hooks.md)

---

## 0. 核心设计决策

### 0.1 兼容目标与当前状态

YourAI 以 Claude Code commit `a371abb` 的 Hook 协议作为兼容基线。兼容性按四层分别验收，
不得用“字段名称相同”代替端到端兼容：

1. **Wire Schema**：输入/输出 JSON 的字段、判别器和取值约束
2. **Settings Schema**：handler 配置字段及其行为
3. **Runtime Semantics**：匹配、并行执行、错误、异步和聚合
4. **Loop Consumption**：在 Agent 流程中触发事件并正确消费结果

只有四层全部通过 conformance 测试，才可以声称“Claude Hook 无需修改即可运行”。

Wire 兼容目标包括：

- 输入 JSON：`snake_case`，全顶层，无包装信封
- 输出 JSON：`camelCase`，全顶层，无包装信封
- Command 传输：stdin `JSON + "\n"`，stdout JSON，exit code `0/2/other`
- HTTP 传输：POST JSON，2xx 成功，不跟随重定向
- 聚合：PreToolUse 权限 `deny > ask > allow`

### 0.2 数据流和内部协议

Loop 构造类型化的 `HookEvent`；其中 Claude 定义为 `unknown` 的动态 payload 仍使用
`serde_json::Value`。Runtime 负责将输入序列化给外部 Hook，并把外部 Hook 的返回值解析、
校验、聚合为类型化结果。

```text
YourAI Loop                                      外部 Hook
───────────                                      ─────────
HookInvocation ──序列化──► snake_case input JSON ──► 执行
                                                      │
HookPointOutcome ◄──聚合── Contribution ◄──解析校验── camelCase output JSON
       │
       └── Loop 按 hook point 消费
```

`input JSON` 不是 LLM 原始输入，而是 Loop 在特定 hook point 构造的调用快照。
`output JSON` 由 Hook 返回给 Runtime；`HookPointOutcome` 从不序列化回 Hook。

### 0.3 与设计稿的差异

`docs/hooks.md` 的原始设计稿提出了 `protocol_version` / `invocation_id` / `context`
信封结构。实现时放弃了这个方案，改为以 Claude Code 的扁平 wire shape 为兼容目标。原因：

1. **生态复用**：完成端到端兼容后，可直接复用 Claude Code hook 脚本
2. **协议简单**：扁平结构比嵌套信封更容易编写和调试
3. **验证充分**：Claude Code 的协议已经在生产环境验证过
4. **版本管理**：通过 crate 语义版本控制演化，不需要 wire 层的 `protocol_version`

---

## 1. 输入协议

### 1.1 Wire 输入格式

Command hook 从 stdin 读取一个 JSON；HTTP hook 接收相同 JSON 的 POST body。
字段采用 `snake_case`，全部在顶层，无嵌套包装。

```json
{
  "session_id": "sess_01",
  "transcript_path": "/tmp/transcript.jsonl",
  "cwd": "/workspace",
  "permission_mode": "default",
  "agent_id": "subagent_01",
  "agent_type": "code-reviewer",
  "hook_event_name": "PreToolUse",
  "tool_name": "Bash",
  "tool_input": { "command": "ls -la" },
  "tool_use_id": "call_01"
}
```

### 1.2 公共字段（BaseInput）

每个事件都包含以下基础字段：

| 字段 | 必需 | 类型 | 说明 |
|---|:---:|---|---|
| `session_id` | 是 | string | 当前 session 标识 |
| `transcript_path` | 是 | string | transcript 文件绝对路径 |
| `cwd` | 是 | string | 当前工作目录 |
| `permission_mode` | 否 | string | 当前权限模式 |
| `agent_id` | 否 | string | 子 agent 标识（仅子 agent 时存在；主线程没有） |
| `agent_type` | 否 | string | agent 类型（子 agent 或 `--agent` 主线程） |
| `hook_event_name` | 是 | string | 事件判别字段 |

YourAI Rust 类型（`yourai-core/src/hooks.rs`）：

```rust
pub struct BaseInput {
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    pub permission_mode: Option<String>,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
}
```

### 1.3 事件专属字段

#### 工具事件

| hook_event_name | 专属字段 |
|---|---|
| `PreToolUse` | `tool_name: string`、`tool_input: unknown`、`tool_use_id: string` |
| `PostToolUse` | `tool_name: string`、`tool_input: unknown`、`tool_response: unknown`、`tool_use_id: string` |
| `PostToolUseFailure` | `tool_name: string`、`tool_input: unknown`、`tool_use_id: string`、`error: string`、`is_interrupt?: boolean` |
| `PermissionRequest` | `tool_name: string`、`tool_input: unknown`、`permission_suggestions?: PermissionUpdate[]` |
| `PermissionDenied` | `tool_name: string`、`tool_input: unknown`、`tool_use_id: string`、`reason: string` |

#### Prompt 与会话生命周期事件

| hook_event_name | 专属字段 |
|---|---|
| `UserPromptSubmit` | `prompt: string` |
| `SessionStart` | `source: "startup" \| "resume" \| "clear" \| "compact"`、`model?: string` |
| `SessionEnd` | `reason: ExitReason` |
| `Setup` | `trigger: "init" \| "maintenance"` |
| `Stop` | `stop_hook_active: boolean`、`last_assistant_message?: string` |
| `StopFailure` | `error: SDKAssistantMessageError`、`error_details?: string`、`last_assistant_message?: string` |
| `SubagentStart` | `agent_id: string`、`agent_type: string` |
| `SubagentStop` | `stop_hook_active: boolean`、`agent_id: string`、`agent_transcript_path: string`、`agent_type: string`、`last_assistant_message?: string` |
| `PreCompact` | `trigger: "manual" \| "auto"`、`custom_instructions: string \| null` |
| `PostCompact` | `trigger: "manual" \| "auto"`、`compact_summary: string` |
| `Notification` | `message: string`、`title?: string`、`notification_type: string` |

#### Team、MCP、配置与文件事件

| hook_event_name | 专属字段 |
|---|---|
| `TeammateIdle` | `teammate_name: string`、`team_name: string` |
| `TaskCreated` | `task_id: string`、`task_subject: string`、`task_description?: string`、`teammate_name?: string`、`team_name?: string` |
| `TaskCompleted` | 与 `TaskCreated` 相同 |
| `Elicitation` | `mcp_server_name: string`、`message: string`、`mode?: "form" \| "url"`、`url?: string`、`elicitation_id?: string`、`requested_schema?: object` |
| `ElicitationResult` | `mcp_server_name: string`、`elicitation_id?: string`、`mode?: "form" \| "url"`、`action: "accept" \| "decline" \| "cancel"`、`content?: object` |
| `ConfigChange` | `source: "user_settings" \| "project_settings" \| "local_settings" \| "policy_settings" \| "skills"`、`file_path?: string` |
| `InstructionsLoaded` | `file_path: string`、`memory_type: "User" \| "Project" \| "Local" \| "Managed"`、`load_reason: "session_start" \| "nested_traversal" \| "path_glob_match" \| "include" \| "compact"`、`globs?: string[]`、`trigger_file_path?: string`、`parent_file_path?: string` |
| `WorktreeCreate` | `name: string` |
| `WorktreeRemove` | `worktree_path: string` |
| `CwdChanged` | `old_cwd: string`、`new_cwd: string` |
| `FileChanged` | `file_path: string`、`event: "change" \| "add" \| "unlink"` |

共 27 种事件。YourAI 的 `HookEvent` enum 有对应变体（`#[non_exhaustive]`）。

### 1.4 枚举类型

```text
ExitReason = "clear" | "resume" | "logout" | "prompt_input_exit"
           | "other" | "bypass_permissions_disabled"

SDKAssistantMessageError = "authentication_failed" | "billing_error"
  | "rate_limit" | "invalid_request" | "server_error" | "unknown"
  | "max_output_tokens"
```

---

## 2. 输出协议

### 2.1 Wire 输出格式

Hook 通过 stdout（command）或 response body（HTTP）返回 JSON。
字段采用 `camelCase`，全部在顶层。两个分支由 `async` 字段判别。

### 2.2 分支 A：异步输出

```json
{
  "async": true,
  "asyncTimeout": 60000
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `async` | `true`（literal） | 标记为异步 |
| `asyncTimeout` | number? | 后台超时（毫秒） |

Command hook 把它作为 stdout 第一行输出，进程移入后台。

### 2.3 分支 B：同步输出

所有字段均可选，`{}` 即合法成功响应。

```json
{
  "continue": true,
  "suppressOutput": false,
  "stopReason": "停止原因",
  "decision": "approve",
  "reason": "决策原因",
  "systemMessage": "展示给用户的消息",
  "hookSpecificOutput": { ... }
}
```

| 字段 | 类型 | 语义 |
|---|---|---|
| `continue` | boolean | `false` → 阻止后续继续 |
| `suppressOutput` | boolean | `true` → 隐藏 stdout 展示 |
| `stopReason` | string | `continue: false` 时的停止原因 |
| `decision` | `"approve" \| "block"` | 通用/旧式决策 |
| `reason` | string | `decision` 的原因 |
| `systemMessage` | string | 展示给用户的系统消息 |
| `hookSpecificOutput` | object | 事件专属输出（见下） |

### 2.4 hookSpecificOutput 完整成员

`hookSpecificOutput.hookEventName` 必须与当前触发事件一致，否则 → 协议错误。

| hookEventName | 允许字段 |
|---|---|
| `PreToolUse` | `permissionDecision?: "allow" \| "deny" \| "ask"`、`permissionDecisionReason?: string`、`updatedInput?: object`、`additionalContext?: string` |
| `UserPromptSubmit` | `additionalContext?: string` |
| `SessionStart` | `additionalContext?: string`、`initialUserMessage?: string`、`watchPaths?: string[]` |
| `Setup` | `additionalContext?: string` |
| `SubagentStart` | `additionalContext?: string` |
| `PostToolUse` | `additionalContext?: string`、`updatedMCPToolOutput?: unknown`（仅替换 MCP 工具输出） |
| `PostToolUseFailure` | `additionalContext?: string` |
| `PermissionDenied` | `retry?: boolean` |
| `Notification` | `additionalContext?: string` |
| `PermissionRequest` | `decision: PermissionRequestDecision`（见下） |
| `CwdChanged` | `watchPaths?: string[]` |
| `FileChanged` | `watchPaths?: string[]` |
| `Elicitation` | `action?: "accept" \| "decline" \| "cancel"`、`content?: object` |
| `ElicitationResult` | `action?: "accept" \| "decline" \| "cancel"`、`content?: object` |
| `WorktreeCreate` | `worktreePath: string`（**必需**） |

### 2.5 PermissionRequest 嵌套 decision

```json
{
  "hookSpecificOutput": {
    "hookEventName": "PermissionRequest",
    "decision": {
      "behavior": "allow",
      "updatedInput": {},
      "updatedPermissions": []
    }
  }
}
```

或：

```json
{
  "hookSpecificOutput": {
    "hookEventName": "PermissionRequest",
    "decision": {
      "behavior": "deny",
      "message": "拒绝原因",
      "interrupt": false
    }
  }
}
```

```text
PermissionRequestDecision =
  | { behavior: "allow", updatedInput?: object, updatedPermissions?: PermissionUpdate[] }
  | { behavior: "deny",  message?: string, interrupt?: boolean }
```

### 2.6 没有专属输出的事件

以下事件没有 `hookSpecificOutput` variant，Hook 只能返回公共字段或 `{}`：

`SessionEnd`、`Stop`、`StopFailure`、`SubagentStop`、`PreCompact`、`PostCompact`、
`TeammateIdle`、`TaskCreated`、`TaskCompleted`、`ConfigChange`、`InstructionsLoaded`、
`WorktreeRemove`

---

## 3. Command Hook 传输协议

### 3.1 交互流程

```text
YourAI                         hook process
   │ spawn(command)                │
   │──────────────────────────────►│
   │ stdin: JSON + "\n"            │
   │──────────────────────────────►│
   │ close stdin                   │
   │──────────────────────────────►│
   │                    stdout      │
   │◄──────────────────────────────│
   │                    stderr      │
   │◄──────────────────────────────│
   │                    exit code   │
   │◄──────────────────────────────│
```

### 3.2 stdout 解析规则

| stdout 状态 | 处理 |
|---|---|
| `trim()` 以 `{` 开头 | 解析为 JSON，用输出 Schema 校验 |
| 不以 `{` 开头 | 作为 `hook_success` plain-text message；不是 `additionalContext` |
| 以 `{` 开头但不符合 Schema | 产生 validation diagnostic；Command 按 non-blocking message 处理 |
| 空 stdout | 等价于 `{}`（空成功） |

### 3.3 退出码

| 退出码 | 含义 | YourAI 处理 |
|:---:|---|---|
| `0` | 成功 | 解析 stdout |
| `2` | blocking error | 产生 blocking feedback；具体阻断对象由当前 hook point 决定 |
| 其他非零 | non-blocking error | 记录失败，按 `FailurePolicy` 处理 |

结构化 Hook 应通过 JSON 表达业务结果并退出 `0`，避免混合 JSON 语义和非零退出码。

### 3.4 可选双向交互

Claude 的部分 Command Hook 路径允许子进程先输出 prompt request，再由宿主向同一 stdin
写回选择。YourAI 当前 Command transport 在写入首次 input 后关闭 stdin，尚未实现这条可选
双向通道。它需要由未来 Loop 提供统一的 Ask/Reply capability，不能由 Runtime 私自读取 UI。

---

## 4. HTTP Hook 传输协议

| 规则 | 说明 |
|---|---|
| 方法 | `POST` |
| Content-Type | `application/json` |
| body | 与 command stdin 完全相同的 input JSON |
| 成功状态 | 仅 `2xx` |
| 重定向 | 不自动跟随（`maxRedirects: 0`） |
| 空 body | 按 `{}` 处理 |
| 非 JSON body | 协议错误 |
| Header 环境变量 | `$VAR` / `${VAR}` 插值，受 `allowedEnvVars` 限制 |
| Header 注入防护 | 清理 CR/LF/NUL 字节 |
| URL allowlist | 可选，全局策略限制可访问 URL |

HTTP status 只表达传输成败，业务控制仍由 JSON body 表达。

---

## 5. Matcher 协议

### 5.1 匹配语法

| 语法 | 含义 | 示例 |
|---|---|---|
| 省略或 `"*"` | 匹配全部 | `""`、`"*"` |
| 精确字符串 | 精确匹配 | `"Write"` |
| `\|` 分隔多选 | 多个精确值 | `"Read\|Write\|Edit"` |
| JavaScript 正则 | 正则匹配 | `"^mcp__.*"` |

YourAI 的 `CompiledMatcher` 预编译为三种变体（`All` / `Exact` / `Regex`），
避免每次 dispatch 重新编译正则。

### 5.2 各事件的 matcher 匹配字段

| 事件 | matcher 匹配对象 |
|---|---|
| `PreToolUse`、`PostToolUse`、`PostToolUseFailure` | `tool_name` |
| `PermissionRequest`、`PermissionDenied` | `tool_name` |
| `SessionStart` | `source` |
| `Setup` | `trigger` |
| `PreCompact`、`PostCompact` | `trigger` |
| `Notification` | `notification_type` |
| `SessionEnd` | `reason` |
| `StopFailure` | `error` |
| `SubagentStart`、`SubagentStop` | `agent_type` |
| `Elicitation`、`ElicitationResult` | `mcp_server_name` |
| `ConfigChange` | `source` |
| `InstructionsLoaded` | `load_reason` |
| `FileChanged` | `file_path` 的 basename |

无 matcher 对象的事件（`UserPromptSubmit`、`Stop`、`TeammateIdle`、`TaskCreated`、
`TaskCompleted`、`WorktreeCreate`、`WorktreeRemove`、`CwdChanged`）不执行 matcher
过滤，其 handler 都会被选中。

---

## 6. 聚合协议

### 6.1 多 Handler 并行执行

一个事件可以匹配多个 Handler。YourAI 的行为（与 Claude Code 一致）：

1. 获取当前事件的所有注册项
2. 按 `event_name` + matcher 过滤
3. 同一来源内对完全相同的配置 Handler 去重
4. **并行执行**所有匹配 Handler（`JoinSet`）
5. 逐个解析输出为 `Contribution`
6. `HookRun` 保留完成顺序用于观测；`Contribution` 恢复注册顺序
7. 按注册顺序聚合为最终 `HookPointOutcome`

### 6.2 公共控制与 PreToolUse 权限是两个维度

以下返回值不得合并为同一个 `Block`：

| Wire 输出 | 内部含义 |
|---|---|
| `continue: false` | `preventContinuation`，请求停止后续 Agent 流程 |
| `permissionDecision: "deny"` | 拒绝本次工具调用，模型仍可收到失败结果后继续 |
| exit code `2` / `decision: "block"` | blocking feedback；由触发该 Hook 的 Loop 消费点解释 |

`PreToolUse` 的权限与公共 continuation 控制分别聚合。仅有 `deny` 时不得设置
`preventContinuation`。

### 6.3 PreToolUse 权限聚合

权限结果按以下优先级聚合：

```text
deny > ask > allow
```

- 任一 `deny` → 最终 `Deny`
- 没有 `deny` 但存在 `ask` → 最终 `Ask`
- 只有 `allow` → 最终 `Allow`
- 全部 `Pass` → 最终 `Pass`（透传给 SecurityProvider）

Hook 的 `Allow` 不等于绕过 YourAI 其他权限检查——最终决策还需与
`SecurityProvider` 合并。

### 6.4 其他事件的聚合

| 事件 | 聚合规则 |
|---|---|
| `UserPromptSubmit` | blocking feedback → 拒绝本次 prompt；`additionalContext` 收集全部 |
| `PostToolUse` | blocking feedback 反馈给模型；`additionalContext` 收集全部；`updatedMCPToolOutput` 取最后一个 |
| `SessionStart` | `additionalContext` 收集；`watchPaths` 合并；`initialUserMessage` 取最后一个 |
| `PermissionDenied` | `retry` 任一为 true → true |
| `PermissionRequest` | `decision` 取最后一个非空 |
| `CwdChanged` / `FileChanged` | `watchPaths` 合并 |
| `Elicitation` / `ElicitationResult` | `action` / `content` 取最后一个；`decline` → Block |
| `WorktreeCreate` | `worktreePath` 取最后一个 |
| 其他 | 由逐事件消费矩阵解释 blocking feedback；`additionalContext` 收集全部 |

---

## 7. YourAI 内部类型映射

### 7.1 输入映射

```text
Wire JSON (snake_case)          YourAI Rust 类型
──────────────────              ─────────────────
{                               HookInvocation {
  session_id,                     base: BaseInput {
  transcript_path,                  session_id,
  cwd,                              transcript_path,
  permission_mode?,                 cwd,
  agent_id?,                        permission_mode?,
  agent_type?,                      agent_id?,
  hook_event_name,                  agent_type?,
  ...事件专属字段...               },
}                                 event: HookEvent::PreToolUse {
                                    tool_name,
                                    tool_input,
                                    tool_use_id,
                                  },
                                }
```

`HookInvocation::to_wire_json()` 把类型化事件序列化回 Claude 兼容的扁平 JSON。

### 7.2 输出映射

```text
Wire JSON (camelCase)           YourAI Rust 类型
──────────────────              ─────────────────
{                               HookDispatchResult {
  "continue": false,              common: HookCommonOutcome {
                                      prevent_continuation: true,
                                      blocking_errors: [...],
                                      ...
                                    },
                                    outcome: HookPointOutcome::PreToolUse(
  "hookSpecificOutput": {           PreToolUseOutcome {
    "hookEventName": "PreToolUse",
    "permissionDecision": "deny",     permission: HookPermission::Deny { reason },
    "permissionDecisionReason": ".."  updated_input: Option<Value>,
  }                                   additional_contexts: Vec<String>,
}                                   }
                                  ),
                                  runs: Vec<HookRun>,
                                }
```

### 7.3 HookPermission 与 wire permissionDecision 的映射

| Wire `permissionDecision` | YourAI `HookPermission` |
|---|---|
| `"allow"` | `Allow { reason }` |
| `"deny"` | `Deny { reason }` |
| `"ask"` | `Ask { reason }` |
| 省略 | `Pass` |

### 7.4 公共结果与 wire 字段的映射

| Wire 字段 | YourAI 公共结果 |
|---|---|
| `continue: false` | `common.prevent_continuation = true`，保留 `stopReason` |
| exit code `2` | 追加 `common.blocking_errors`，由 Loop 按事件解释 |
| `decision: "block"` | 权限 deny + blocking feedback；不自动设置 prevent continuation |
| 其他 | 不改变公共 continuation 状态 |

---

## 8. 注册配置

### 8.1 配置格式

兼容 Claude `settings.json` 的 hooks 分组格式：

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Read|Write|Edit",
        "hooks": [
          {
            "type": "command",
            "command": "python3 .claude/hooks/check.py",
            "timeout": 30
          }
        ]
      }
    ],
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "http",
            "url": "https://memory.example.com/recall",
            "timeout": 5,
            "headers": { "Authorization": "Bearer $HOOK_TOKEN" },
            "allowedEnvVars": ["HOOK_TOKEN"]
          }
        ]
      }
    ]
  }
}
```

### 8.2 四类 Handler

| type | 必填字段 | 可选字段 |
|---|---|---|
| `command` | `command` | `shell`、`timeout`、`if`、`statusMessage`、`once`、`async`、`asyncRewake` |
| `http` | `url` | `timeout`、`headers`、`allowedEnvVars`、`if`、`statusMessage`、`once` |
| `prompt` | `prompt` | `timeout`、`model`、`if`、`statusMessage`、`once` |
| `agent` | `prompt` | `timeout`、`model`、`if`、`statusMessage`、`once` |

YourAI Runtime 内建 `command` 和 `http` capability。`prompt` / `agent` 通过
`HookModelExecutor` 由宿主注入；未注入时配置在注册阶段明确失败，不允许延迟到首次
dispatch 才报错。

Runtime 不自行判断 workspace 是否可信。宿主配置层只能把已经通过 trust policy 的
Project 配置交给 `register_config`；`HookSource` 是来源元数据，不是权限证明。

### 8.3 YourAI Rust 类型

```rust
pub struct HookRegistration {
    pub id: String,
    pub event_name: String,
    pub matcher: CompiledMatcher,
    pub handler: HandlerConfig,
    pub timeout: Option<Duration>,
    pub source: HookSource,
    pub failure_policy: FailurePolicy,
    pub once: bool,
}

pub enum HookSource {
    Managed,    // 管理员策略预置
    User,       // 用户配置
    Project,    // 项目配置
    Plugin,     // 插件提供
    Session,    // 会话临时注册
}
```

---

## 9. YourAI Runtime 执行流程

### 9.1 dispatch 完整流程

```text
Loop 调用 HookRuntime::dispatch(invocation)
  │
  ├── 1. 从事件提取 event_name + match_query
  ├── 2. 快照注册项，按 event_name + matcher 过滤
  ├── 3. 同来源重复配置去重；无匹配 → 返回空 HookDispatchResult
  ├── 4. 并行执行所有匹配 handler（JoinSet + timeout）
  │      ├── CommandHandler: spawn → stdin JSON+\n → 读 stdout/stderr → exit code
  │      ├── HttpHandler: POST JSON → 读 response body → status code
  │      ├── Prompt/Agent: 调用宿主注入的 HookModelExecutor
  │      └── NativeHandler: 直接调用 Rust handler
  ├── 5. 解析每个 handler 输出
  │      ├── exit 0 + JSON → parse_hook_json → Contribution
  │      ├── exit 0 + plain text → HookMessage::Success
  │      ├── exit 0 + empty → 空 Contribution
  │      ├── exit 2 → HookBlockingError { stderr }
  │      ├── exit other → Failed（按 FailurePolicy）
  │      └── HTTP 2xx + JSON → parse_hook_json → Contribution
  ├── 6. 校验 hookSpecificOutput.hookEventName 与输入事件一致
  │      └── 不一致 → EventNameMismatch 错误 → handler Failed
  ├── 7. 聚合所有 Contribution
  │      ├── PreToolUse: permission deny>ask>allow
  │      ├── additional_contexts: 收集全部
  │      ├── common: continuation / system message / hook message / blocking error
  │      └── specific: permission / updated input / context / event-specific fields
  └── 8. 返回 HookDispatchResult { event, common, outcome, runs }
```

### 9.2 返回类型

```rust
pub struct HookDispatchResult {
    pub event: HookEventKind,          // 保留真实事件身份
    pub common: HookCommonOutcome,     // 所有事件共享的效果
    pub outcome: HookPointOutcome,  // 聚合后的类型化结果
    pub runs: Vec<HookRun>,         // 每个 handler 的执行记录
}

pub enum HookPointOutcome {
    PreToolUse(PreToolUseOutcome),
    PostToolUse(PostToolUseOutcome),
    UserPromptSubmit(UserPromptSubmitOutcome),
    SessionStart(SessionStartOutcome),
    PermissionDenied(PermissionDeniedOutcome),
    PermissionRequest(PermissionRequestOutcome),
    CwdChanged(WatchPathsOutcome),
    FileChanged(WatchPathsOutcome),
    Elicitation(ElicitationOutcome),
    ElicitationResult(ElicitationOutcome),
    WorktreeCreate(WorktreeCreateOutcome),
    Generic(GenericOutcome),  // 无专属 outcome 的事件
}

pub struct HookRun {
    pub hook_id: String,
    pub source: String,
    pub status: HookRunStatus,  // Completed | Backgrounded | Failed | Blocked | Cancelled | TimedOut
    pub started_at: i64,
    pub duration: Duration,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub exit_code: Option<i32>,
    pub suppress_output: bool,
    pub status_message: Option<String>,
    pub background_task_id: Option<String>,
}
```

### 9.3 Loop 消费规则

HookRuntime 只返回数据，不产生业务副作用。Loop 负责消费：

| 事件 | Loop 消费动作 |
|---|---|
| `UserPromptSubmit` | blocking feedback → 拒绝本次输入；`preventContinuation` → 停止；`additionalContexts` → 写入 `ContextManager` |
| `PreToolUse` | `permission` 与 `SecurityProvider` 合并；deny 只拒绝工具；`preventContinuation` 才停止；`Ask` → 发 `Out::Ask` 等 `In::Reply` |
| `PostToolUse` | `additionalContexts` 写入历史；`updatedMCPToolOutput` 替换 MCP 工具输出 |
| `SessionStart` | `additionalContexts` 注入；`watchPaths` 注册文件监听 |
| 其他 | Block → 阻断；`additionalContexts` 收集 |

关键边界：**HookRuntime 负责把外部程序变成可信的类型化结果；Loop 负责让这个结果在 Agent 流程中真正生效。**

当前仓库尚无生产 `DefaultLoop`，所以上表是 Loop 的规范性消费契约，而不是“已经接线”的
声明。`ProviderSnapshot.hooks` 已提供 turn 级 Runtime 快照，实际触发点与副作用应用留在下一阶段。

---

## 10. 完整示例

### 10.1 PreToolUse — deny 一个 Bash 命令

**配置**：
```json
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash",
        "hooks": [{ "type": "command", "command": "python3 check.py", "timeout": 10 }] }
    ]
  }
}
```

**输入（stdin）**：
```json
{
  "session_id": "sess_01",
  "transcript_path": "/tmp/t.jsonl",
  "cwd": "/workspace",
  "permission_mode": "default",
  "hook_event_name": "PreToolUse",
  "tool_name": "Bash",
  "tool_input": { "command": "rm -rf /" },
  "tool_use_id": "call_01"
}
```

**脚本输出（stdout）**：
```json
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "deny",
    "permissionDecisionReason": "禁止 rm -rf"
  }
}
```
exit 0。

**YourAI 解析结果**：
```rust
HookDispatchResult {
    event: HookEventKind::PreToolUse,
    common: HookCommonOutcome {
        prevent_continuation: false,
        blocking_errors: [HookBlockingError { message: "禁止 rm -rf", ... }],
        ...
    },
    outcome: HookPointOutcome::PreToolUse(PreToolUseOutcome {
        permission: HookPermission::Deny { reason: "禁止 rm -rf" },
        updated_input: None,
        additional_contexts: [],
    }),
    runs: [HookRun { status: Completed, exit_code: Some(0), ... }],
}
```

### 10.2 UserPromptSubmit — 注入记忆上下文

**输入（stdin）**：
```json
{
  "session_id": "sess_01",
  "transcript_path": "/tmp/t.jsonl",
  "cwd": "/workspace",
  "hook_event_name": "UserPromptSubmit",
  "prompt": "帮我看看这个项目的架构"
}
```

**脚本输出（stdout）**：
```json
{
  "hookSpecificOutput": {
    "hookEventName": "UserPromptSubmit",
    "additionalContext": "项目架构文档位于 docs/ARCHITECTURE.md，请优先参考。"
  }
}
```

**YourAI 解析结果**：
```rust
HookDispatchResult {
    event: HookEventKind::UserPromptSubmit,
    common: HookCommonOutcome::default(),
    outcome: HookPointOutcome::UserPromptSubmit(UserPromptSubmitOutcome {
        additional_contexts: ["项目架构文档位于 docs/ARCHITECTURE.md，请优先参考。"],
    }),
    runs: [...],
}
```

生产 Loop 必须将 `additional_contexts` 写入上下文，使其进入模型请求和会话记录；当前
阶段 Runtime 只返回这组类型化数据，不直接修改 `ContextManager`。

### 10.3 多 Hook 聚合 — allow + deny = deny

两个 PreToolUse hook 并行执行：

- Hook A 返回 `permissionDecision: "allow"`
- Hook B 返回 `permissionDecision: "deny", permissionDecisionReason: "no rm -rf"`

**聚合结果**：
```rust
permission: HookPermission::Deny { reason: "no rm -rf" }
```

`deny` 覆盖 `allow`，与 Claude Code 行为一致。

---

## 11. 协议兼容性验收状态

| 方面 | Claude Code | YourAI 目标 | 当前状态 |
|---|---|---|:---:|
| 输入字段命名 | `snake_case` | `snake_case` | 已对齐 |
| 输出字段命名 | `camelCase` | `camelCase` | 已对齐 |
| 输入包装 | 无（全顶层） | 无（全顶层） | 已对齐 |
| 输出包装 | 无（全顶层） | 无（全顶层） | 已对齐 |
| 事件判别 | `hook_event_name` | `hook_event_name` | 已对齐 |
| 输出判别 | `hookSpecificOutput.hookEventName` | `hookSpecificOutput.hookEventName` | 已对齐 |
| Command stdin | `JSON + "\n"` | `JSON + "\n"` | 已对齐 |
| Command stdout | JSON / plain text | JSON / plain text message | 已对齐 |
| Command 双向 prompt request | 部分调用路径支持 | 依赖未来 Loop Ask/Reply capability | 未对齐 |
| Exit code 0 | 成功 | 成功 | 已对齐 |
| Exit code 2 | blocking feedback | blocking feedback | 待 Loop 验收 |
| Exit code other | non-blocking error | non-blocking error | 已对齐 |
| HTTP method | POST | POST | 已对齐 |
| HTTP success | 2xx | 2xx | 已对齐 |
| HTTP redirect | 不跟随 | 不跟随 | 已对齐 |
| HTTP empty body | `{}` | `{}` | 已对齐 |
| HTTP URL/env/SSRF | allowlist + env 交集 + DNS guard | 相同 | 已对齐 |
| Matcher 语法 | 精确/\|/ECMAScript 正则/\* | 相同 | 部分对齐 |
| 权限聚合 | deny > ask > allow | deny > ask > allow | 已对齐 |
| 并行执行 | 是，按完成顺序产生结果 | 相同，保留 registration 身份 | 已对齐 |
| 同来源重复 Handler | 执行前去重 | 相同 | 已对齐 |
| async/asyncRewake | 后台执行 + 完成事件 | 后台执行 + 完成事件；Loop rewake 待接入 | 部分对齐 |
| prompt/agent handler | 内置模型能力 | 宿主注入 `HookModelExecutor`；缺失时注册失败 | 部分对齐 |
| Workspace trust | 未信任项目 Hook 不执行 | 注册前由宿主策略层检查 | 待宿主验收 |
| 事件数量 | 27 | 27 | 已对齐 |
| hookSpecificOutput 变体 | 15 | 15 | 已对齐 |

### 11.1 下一阶段必须补齐的运行边界

以下项目尚未完成，不属于当前“已对齐”范围：

1. 生产 Loop 在所有 hook point 的触发与 outcome 消费，以及 `asyncRewake` 唤醒。
2. Loop 丢弃 `dispatch` future 的取消集成测试；core trait 已明确 drop-based 取消契约。
3. Command/HTTP 的默认 timeout（兼容基线为 60 秒）及 stdout、stderr、HTTP body
   的最大字节数。
4. Command 超时/取消时终止整个进程组，而不仅是直接 shell 子进程。
5. Command 环境变量继承和默认 shell 策略；需要兼顾最小暴露原则、跨平台行为与
   Claude 脚本兼容性。
6. Managed/User/Project/Plugin/Session 的 source precedence、显式 `order` 和稳定装配顺序。
7. Command 同进程双向 prompt request、workspace trust 的宿主接线。

“无需修改即可运行”是最终验收结论，不是设计前提；只有本表全部通过自动化
conformance 测试后才可启用该表述。

---

## 12. YourAI 独有扩展

以下能力是 YourAI 在 Claude 兼容协议之上的扩展，不影响兼容性：

| 扩展 | 说明 |
|---|---|
| `HookPermission::Pass` | Claude 没有"不做决策"的显式值；YourAI 用 `Pass` 表示透传 |
| `FailurePolicy::Open/Closed` | Claude 的 non-blocking error 隐含 Open；YourAI 默认 Open，可由实现层显式配置 Closed |
| `NativeHandler` | Rust 闭包直接返回类型化结果，不走 JSON 序列化 |
| `HookRun` 执行记录 | 每个 handler 的执行状态、耗时、stdout/stderr 用于可观测性 |
| `HookSource` 分类 | Managed/User/Project/Plugin/Session 五种来源；Runtime 记录，宿主策略层解释 |
| 类型化公共结果 | continuation、blocking feedback、permission 分开表达，避免丢失 Claude 语义 |
| 串行 waterfall（设计预留） | PreToolUse 的 `updatedInput` 链式传递（设计稿规划，当前实现为并行） |

---

## 13. 源码索引

| 内容 | 文件 |
|---|---|
| 协议类型与 trait | `yourai-core/src/hooks.rs` |
| Wire 输入序列化 | `yourai-hooks/src/event.rs` |
| Wire 输出反序列化 | `yourai-hooks/src/wire_output.rs` |
| Matcher 编译与匹配 | `yourai-hooks/src/matcher.rs` |
| 配置解析 + 注册项 | `yourai-hooks/src/config.rs` |
| Native 与模型 capability adapter | `yourai-hooks/src/handler.rs` |
| Command 执行器 | `yourai-hooks/src/command.rs` |
| HTTP 执行器 | `yourai-hooks/src/http.rs` |
| Runtime（注册 + 分发 + 聚合） | `yourai-hooks/src/runtime.rs` |
| Claude Code 源码核对 | `docs/claude-code-hook-protocol.md` |
| 设计背景 | `docs/hooks.md` |
