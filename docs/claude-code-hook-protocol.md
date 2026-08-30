# Claude Code Hook 协议

> 本文只描述本地 `../claude-code` 源码中已经实现的协议，不包含 YourAI 的设计建议。
>
> 核对版本：Claude Code commit `a371abb`，最后提交日期 `2026-04-05`。

## 1. 协议总览

Claude Code Hook 协议由三部分组成：

1. settings 中的 Hook 注册配置；
2. Claude Code 发送给 Hook 的事件输入；
3. Hook 通过 stdout 或 HTTP response body 返回的结果。

最重要的结构是：

```text
输入 = BaseHookInput + EventSpecificInput
输出 = CommonOutput + hookSpecificOutput
```

输入通过 `hook_event_name` 判别事件类型；输出通过
`hookSpecificOutput.hookEventName` 判别事件专属结果类型。

Claude Code 的协议中没有以下统一包装字段：

```text
version
request_id
context
input
payload
output
effects
```

输入字段采用 `snake_case`，输出字段主要采用 `camelCase`。所有字段都直接位于顶层，
没有 `context` 或 `input` 嵌套对象。

## 2. Hook 注册配置

Hook 配置以事件名分组。每个事件下是 matcher group 数组，每个 group 包含一个可选
matcher 和一组 Handler：

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Read|Write|Edit",
        "hooks": [
          {
            "type": "command",
            "command": "python3 .claude/hooks/check_tool.py",
            "timeout": 30
          }
        ]
      }
    ]
  }
}
```

持久化配置支持四类 Handler：

### 2.1 Command

```json
{
  "type": "command",
  "command": "python3 hook.py",
  "if": "Bash(git *)",
  "shell": "bash",
  "timeout": 30,
  "statusMessage": "Checking policy…",
  "once": false,
  "async": false,
  "asyncRewake": false
}
```

- `command` 必填；
- `shell` 可选，支持源码中 `SHELL_TYPES` 定义的 shell；
- `timeout` 单位为秒；
- `once` 表示成功触发后移除；
- `async` 表示后台执行；
- `asyncRewake` 表示后台执行，退出码为 `2` 时唤醒模型；它隐含 async；
- `if` 使用 permission rule 语法，仅对工具类事件有效。

### 2.2 HTTP

```json
{
  "type": "http",
  "url": "https://hooks.example.com/claude",
  "timeout": 30,
  "headers": {
    "Authorization": "Bearer $HOOK_TOKEN"
  },
  "allowedEnvVars": ["HOOK_TOKEN"],
  "statusMessage": "Sending hook…",
  "once": false
}
```

HTTP Hook 使用 `POST`，请求 `Content-Type` 为 `application/json`。环境变量只会在
`allowedEnvVars` 允许时插入 header。HTTP Hook 还受全局 URL 和环境变量策略限制。

### 2.3 Prompt

```json
{
  "type": "prompt",
  "prompt": "判断下面的 Hook 输入是否安全：$ARGUMENTS",
  "timeout": 30,
  "model": "model-name",
  "statusMessage": "Evaluating…",
  "once": false
}
```

### 2.4 Agent

```json
{
  "type": "agent",
  "prompt": "验证这次操作是否符合项目要求：$ARGUMENTS",
  "timeout": 60,
  "model": "model-name",
  "statusMessage": "Verifying…",
  "once": false
}
```

配置 Schema 见
[`src/schemas/hooks.ts`](../../claude-code/src/schemas/hooks.ts)。

## 3. Matcher

`matcher` 支持：

- 未配置或 `"*"`：匹配全部；
- 精确字符串：例如 `"Write"`；
- `|` 分隔的多个精确值：例如 `"Write|Edit"`；
- JavaScript 正则表达式：例如 `"^(Read|Write)$"`。

不同事件使用不同值进行匹配：

| 事件 | matcher 对象 |
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

没有 matcher 对象的事件不会执行 matcher 过滤；其 matcher group 中的 Handler 都会被
选中。

## 4. 输入协议

### 4.1 公共输入字段

每个 Hook 输入都包含：

```json
{
  "session_id": "session-id",
  "transcript_path": "/absolute/path/to/transcript.jsonl",
  "cwd": "/current/working/directory",
  "permission_mode": "default",
  "agent_id": "optional-subagent-id",
  "agent_type": "optional-agent-type",
  "hook_event_name": "PreToolUse"
}
```

| 字段 | 必需 | 含义 |
|---|---:|---|
| `session_id` | 是 | 当前 session 标识 |
| `transcript_path` | 是 | 当前 transcript 文件路径 |
| `cwd` | 是 | 当前工作目录 |
| `permission_mode` | 否 | 当前权限模式 |
| `agent_id` | 否 | 子 agent 标识；主线程没有该字段 |
| `agent_type` | 否 | agent 类型 |
| `hook_event_name` | 是 | 事件判别字段 |

### 4.2 工具事件

#### PreToolUse

```json
{
  "session_id": "sess_01",
  "transcript_path": "/tmp/transcript.jsonl",
  "cwd": "/workspace",
  "permission_mode": "default",
  "hook_event_name": "PreToolUse",
  "tool_name": "get_order",
  "tool_input": {
    "order_id": "A10001"
  },
  "tool_use_id": "call_01"
}
```

专属字段：`tool_name: string`、`tool_input: unknown`、`tool_use_id: string`。

#### PermissionRequest

专属字段：

```text
tool_name: string
tool_input: unknown
permission_suggestions?: PermissionUpdate[]
```

#### PostToolUse

专属字段：

```text
tool_name: string
tool_input: unknown
tool_response: unknown
tool_use_id: string
```

#### PostToolUseFailure

专属字段：

```text
tool_name: string
tool_input: unknown
tool_use_id: string
error: string
is_interrupt?: boolean
```

#### PermissionDenied

专属字段：

```text
tool_name: string
tool_input: unknown
tool_use_id: string
reason: string
```

### 4.3 Prompt、session 与生命周期事件

| `hook_event_name` | 事件专属字段 |
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

`UserPromptSubmit.prompt` 是用户提交的 prompt，不是组装完成的完整模型请求。

### 4.4 Team、MCP、配置与文件事件

| `hook_event_name` | 事件专属字段 |
|---|---|
| `TeammateIdle` | `teammate_name: string`、`team_name: string` |
| `TaskCreated` | `task_id: string`、`task_subject: string`、`task_description?: string`、`teammate_name?: string`、`team_name?: string` |
| `TaskCompleted` | 与 `TaskCreated` 相同 |
| `Elicitation` | `mcp_server_name: string`、`message: string`、`mode?: "form" \| "url"`、`url?: string`、`elicitation_id?: string`、`requested_schema?: object` |
| `ElicitationResult` | `mcp_server_name: string`、`elicitation_id?: string`、`mode?: "form" \| "url"`、`action: "accept" \| "decline" \| "cancel"`、`content?: object` |
| `ConfigChange` | `source: "user_settings" \| "project_settings" \| "local_settings" \| "policy_settings" \| "skills"`、`file_path?: string` |
| `InstructionsLoaded` | `file_path: string`、`memory_type: "User" \| "Project" \| "Local" \| "Managed"`、`load_reason`、`globs?: string[]`、`trigger_file_path?: string`、`parent_file_path?: string` |
| `WorktreeCreate` | `name: string` |
| `WorktreeRemove` | `worktree_path: string` |
| `CwdChanged` | `old_cwd: string`、`new_cwd: string` |
| `FileChanged` | `file_path: string`、`event: "change" \| "add" \| "unlink"` |

完整输入 union 见
[`HookInputSchema`](../../claude-code/src/entrypoints/sdk/coreSchemas.ts#L767)。

## 5. 输出协议

### 5.1 同步输出公共字段

所有字段均可选，因此 `{}` 是合法同步响应：

```json
{
  "continue": true,
  "suppressOutput": false,
  "stopReason": "停止原因",
  "decision": "approve",
  "reason": "决策原因",
  "systemMessage": "展示给用户的消息"
}
```

| 字段 | 类型 | 语义 |
|---|---|---|
| `continue` | `boolean` | 默认 `true`；`false` 表示阻止后续继续 |
| `suppressOutput` | `boolean` | 默认 `false`；控制是否隐藏 stdout 展示 |
| `stopReason` | `string` | `continue: false` 时的停止原因 |
| `decision` | `"approve" \| "block"` | 通用/旧式决策；PreToolUse 应使用事件专属字段 |
| `reason` | `string` | `decision` 的原因 |
| `systemMessage` | `string` | 展示给用户的系统消息 |
| `hookSpecificOutput` | discriminated union | 事件专属输出 |

### 5.2 PreToolUse 输出

```json
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "ask",
    "permissionDecisionReason": "该操作需要用户确认",
    "updatedInput": {
      "order_id": "A10001"
    },
    "additionalContext": "提供给模型的附加上下文"
  }
}
```

| 字段 | 类型 |
|---|---|
| `hookEventName` | 固定为 `"PreToolUse"` |
| `permissionDecision` | `"allow" \| "deny" \| "ask"`，可选 |
| `permissionDecisionReason` | `string`，可选 |
| `updatedInput` | object，可选 |
| `additionalContext` | `string`，可选 |

### 5.3 UserPromptSubmit 输出

```json
{
  "hookSpecificOutput": {
    "hookEventName": "UserPromptSubmit",
    "additionalContext": "需要注入模型上下文的内容"
  }
}
```

### 5.4 PostToolUse 与 PostToolUseFailure 输出

```json
{
  "hookSpecificOutput": {
    "hookEventName": "PostToolUse",
    "additionalContext": "附加上下文",
    "updatedMCPToolOutput": {}
  }
}
```

`updatedMCPToolOutput` 只用于替换 MCP 工具输出。

`PostToolUseFailure` 的事件专属输出只有：

```json
{
  "hookEventName": "PostToolUseFailure",
  "additionalContext": "附加上下文"
}
```

### 5.5 其他事件专属输出

| `hookEventName` | 允许字段 |
|---|---|
| `SessionStart` | `additionalContext?`、`initialUserMessage?`、`watchPaths?: string[]` |
| `Setup` | `additionalContext?` |
| `SubagentStart` | `additionalContext?` |
| `PermissionDenied` | `retry?: boolean` |
| `Notification` | `additionalContext?` |
| `CwdChanged` | `watchPaths?: string[]` |
| `FileChanged` | `watchPaths?: string[]` |
| `WorktreeCreate` | `worktreePath: string` |
| `Elicitation` | `action?: "accept" \| "decline" \| "cancel"`、`content?: object` |
| `ElicitationResult` | `action?: "accept" \| "decline" \| "cancel"`、`content?: object` |

`PermissionRequest` 使用嵌套 decision：

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

或者：

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

没有列入 `hookSpecificOutput` union 的事件只能使用同步输出公共字段。

`hookSpecificOutput.hookEventName` 必须与当前触发事件相同；不一致会产生协议错误。

完整输出 Schema 见
[`SyncHookJSONOutputSchema`](../../claude-code/src/entrypoints/sdk/coreSchemas.ts#L907)。

### 5.6 异步输出

异步响应是同步响应的替代分支：

```json
{
  "async": true,
  "asyncTimeout": 60000
}
```

- `asyncTimeout` 可选；
- Command Hook 可以将该对象作为第一行 stdout，Claude Code 随后把进程移入后台；
- 配置中的 `async: true` 也可直接启用后台执行。

## 6. Command Hook 传输协议

Claude Code 启动 command 后：

1. 将完整 Hook 输入序列化为一行 JSON；
2. 写入子进程 stdin；
3. 追加换行 `\n`；
4. 普通模式下关闭 stdin；
5. 同时读取 stdout 和 stderr；
6. 等待退出、超时或取消；
7. 解析 stdout 并生成内部 HookResult。

```text
Claude Code                         Hook command
     │ spawn                             │
     │──────────────────────────────────►│
     │ stdin: JSON + "\n"                │
     │──────────────────────────────────►│
     │                         stdout    │
     │◄──────────────────────────────────│
     │                         stderr    │
     │◄──────────────────────────────────│
     │                         exit code │
     │◄──────────────────────────────────│
```

### 6.1 stdout 解析

- `stdout.trim()` 以 `{` 开头：解析为 JSON，并用 Hook output Schema 校验；
- 不以 `{` 开头：作为 legacy/plain-text 输出；
- 以 `{` 开头但不符合 Schema：产生 JSON validation error；
- JSON 响应应占据完整 stdout，不要在其前后输出普通日志；日志写到 stderr。

### 6.2 退出码

对于没有结构化 JSON 响应的兼容路径：

| 退出码 | 处理 |
|---:|---|
| `0` | 成功；stdout 作为普通 Hook 输出 |
| `2` | blocking error；阻断原因取 stderr |
| 其他非零 | non-blocking error；向用户展示错误，但通常不阻止原流程 |

结构化 Hook 应通过 JSON 表达业务结果并退出 `0`，避免混合 JSON 语义和非零退出码。

### 6.3 可选交互协议

部分 Command Hook 调用路径允许 Hook 在 stdout 单独输出一行 prompt request：

```json
{
  "prompt": "request-id",
  "message": "请选择",
  "options": [
    {
      "key": "allow",
      "label": "允许",
      "description": "继续执行"
    }
  ]
}
```

Claude Code 在同一个子进程 stdin 写回：

```json
{
  "prompt_response": "request-id",
  "selected": "allow"
}
```

Hook 最后仍需输出正常的 Hook JSON result。该能力不是所有 Hook 调用点都保证提供，
脚本不能把它当作所有事件通用的交互能力。

## 7. HTTP Hook 传输协议

HTTP Hook：

- 使用 `POST`；
- body 是与 Command stdin 完全相同的 Hook input JSON；
- `Content-Type: application/json`；
- 只把 HTTP `2xx` 视为成功；
- 不自动跟随 redirect；
- response body 必须是合法 Hook JSON；
- 空 response body 按 `{}` 处理；
- 非 JSON response body 是协议错误；
- 支持 timeout、取消、URL allowlist、header 环境变量 allowlist 和 SSRF 防护。

HTTP transport 没有 Command exit code；HTTP status 只表达 transport 成败，业务控制仍由
Hook JSON response 表达。

## 8. 多 Hook 执行与结果聚合

一个事件可以匹配多个 Handler。Claude Code 的行为是：

1. 获取当前事件的所有 matcher group；
2. 按 matcher 过滤；
3. 展开 group 中的 Handler；
4. 对同一来源中的重复 Handler 去重；
5. 并行执行所有匹配 Handler；
6. 逐个归一化为内部 HookResult；
7. 将结果交给事件调用者消费。

PreToolUse 权限结果按以下优先级聚合：

```text
deny > ask > allow
```

- 任一 `deny` 最终为 deny；
- 没有 deny 但存在 `ask`，最终为 ask；
- 只有 allow 时为 allow；
- Hook 的 allow 不等于绕过 Claude Code 其他权限检查。

## 9. 返回值如何进入 Claude Code

Hook 执行器只负责解析和归一化；真正消费结果的是触发事件的业务调用点。

### 9.1 UserPromptSubmit

调用链：

```text
processUserInput
  → executeUserPromptSubmitHooks
  → executeHooks
  → processUserInput 消费结果
```

- `decision: "block"` 或 blocking error：不发起模型请求；
- `continue: false`：停止当前处理；
- `additionalContext`：转换为 `hook_additional_context` attachment，加入本次模型消息；
- `systemMessage`：转换为 UI/system attachment。

### 9.2 PreToolUse

调用链：

```text
toolExecution
  → runPreToolUseHooks
  → executePreToolHooks
  → executeHooks
  → toolExecution 消费结果
```

- `permissionDecision`：进入工具权限决策；
- `updatedInput`：替换后续权限检查和工具调用使用的输入；
- `additionalContext`：作为 attachment 加入消息；
- `continue: false` 或 block：阻止工具调用或后续继续；
- `ask`：进入 Claude Code 原有用户确认流程，Hook command 自身不拥有该权限 UI。

### 9.3 PostToolUse

- `additionalContext`：作为 attachment 加入消息；
- `updatedMCPToolOutput`：仅对 MCP 工具替换后续使用的工具输出；
- blocking/stop 结果可阻止后续 agent continuation，但不能撤销已经完成的工具副作用。

## 10. 最小脚本示例

下面只演示协议读写，不表达任何具体策略。

### 10.1 Python

```python
#!/usr/bin/env python3
import json
import sys

request = json.load(sys.stdin)
event = request["hook_event_name"]

# 不产生事件专属效果时，空对象就是合法的同步成功响应。
response = {}

json.dump(response, sys.stdout)
sys.stdout.write("\n")
```

注意：并非所有事件都有合法的 `hookSpecificOutput` variant。通用脚本如果不知道当前
事件是否支持专属输出，最安全的成功响应就是：

```json
{}
```

### 10.2 只记录事件的 shell Hook

```sh
#!/bin/sh
payload=$(cat)
printf '%s\n' "$payload" >> /tmp/claude-hook-events.jsonl
printf '%s\n' '{}'
```

业务 stdout 必须只输出 Hook response；调试日志应写 stderr。

## 11. Hook 作者检查清单

1. 从 stdin 读取完整 JSON，不从 argv 猜测事件数据；
2. 先读取 `hook_event_name`，再按对应事件 Schema 解析其他字段；
3. 不假设 `permission_mode`、`agent_id`、`agent_type` 一定存在；
4. `tool_input`、`tool_response` 是 unknown，按具体工具自行校验；
5. JSON response 的字段使用 camelCase；
6. `hookSpecificOutput.hookEventName` 必须与输入事件相同；
7. JSON 之外的日志写 stderr；
8. 结构化成功响应退出 `0`；
9. 不依赖多个并行 Hook 的完成顺序；
10. 记住 Hook 可能因 timeout、取消或 workspace trust 被跳过或终止。

## 12. 源码索引

| 内容 | 源码 |
|---|---|
| 事件列表、公共输入、事件输入 union | [`coreSchemas.ts`](../../claude-code/src/entrypoints/sdk/coreSchemas.ts) |
| 同步/异步输出 Schema | [`coreSchemas.ts`](../../claude-code/src/entrypoints/sdk/coreSchemas.ts#L799) |
| 配置与 Handler Schema | [`schemas/hooks.ts`](../../claude-code/src/schemas/hooks.ts) |
| matcher、执行、解析、聚合 | [`utils/hooks.ts`](../../claude-code/src/utils/hooks.ts) |
| HTTP POST transport | [`execHttpHook.ts`](../../claude-code/src/utils/hooks/execHttpHook.ts) |
| UserPromptSubmit 消费 | [`processUserInput.ts`](../../claude-code/src/utils/processUserInput/processUserInput.ts) |
| 工具 Hook 消费 | [`toolExecution.ts`](../../claude-code/src/services/tools/toolExecution.ts) |
