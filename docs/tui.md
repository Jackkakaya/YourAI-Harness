# TUI 与模型配置

当前产品布局与交互规范见 [TUI review 修订设计](./tui-review-design.md)。

默认按 XDG Base Directory 规范查找配置文件：`$XDG_CONFIG_HOME/yourai/yourai.json`，未设置时回退到 `~/.config/yourai/yourai.json`。可用 `--config PATH` 指定任意路径覆盖。

```sh
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/yourai"
cp yourai.example.json "${XDG_CONFIG_HOME:-$HOME/.config}/yourai/yourai.json"
# 编辑 yourai.json 中的 provider、baseURL 与 apiKey
export OPENAI_API_KEY='你的密钥'
cargo run -p yourai-tui
```

`yourai.json` 含密钥，请勿提交；仓库根的 `yourai.json` 与 `.yourai/` 已加入 gitignore。配置中的 `apiKey` 可用 `{env:VAR}` 引用环境变量，避免明文。不会将密钥打印到界面或写入会话历史。

## 模型配置

```json
{
  "model": "gateway/YOUR_MODEL_NAME",
  "session_dir": ".yourai/sessions",
  "system_prompt": "你是一个编程助手。",
  "provider": {
    "gateway": {
      "api": "openai-compatible",
      "options": {
        "baseURL": "https://api.openai.com/v1",
        "apiKey": "{env:OPENAI_API_KEY}",
        "headerTimeout": 300000,
        "chunkTimeout": 300000
      },
      "models": {
        "YOUR_MODEL_NAME": {
          "id": "YOUR_MODEL_NAME",
          "limit": { "context": 128000, "output": 4096 },
          "options": { "maxOutputTokens": 4096 },
          "pricing": { "input": 5.0, "output": 15.0 },
          "variants": {
            "fast": { "temperature": 0.2 },
            "disabled-example": { "disabled": true }
          }
        }
      }
    }
  },
  "context": {
    "keep_recent_tokens": 20000,
    "summary_tokens": 2000,
    "prune_enabled": false
  }
}
```

- `api` 可取 `openai-compatible`、`openai-responses`、`anthropic`、`gemini` 或 `ollama`；省略时会根据 provider ID 推断，未知 ID 默认按 OpenAI Compatible 处理。
- `options.baseURL` 填基础地址（例如 `/v1`），不要填 `/chat/completions`；省略时使用 genai 的默认服务地址。
- `options.apiKey` 支持 `{env:VAR}`；无需认证的本地服务可填任意非空占位值，或按 provider 默认认证方式省略。
- `session_dir` 和提示词文件路径相对配置文件解析；工作目录仍是启动 TUI 时所在目录。
- 模型窗口与输出上限放在 `provider.*.models.*.limit`；`options.maxOutputTokens` 不得超过 `limit.output`。旧的 `context.context_window/input_limit/output_reserve` 配置会被拒绝。
- `variants` 会合并到模型 `options`；含 `"disabled": true` 的 variant 不会出现在 `/models` 选择器中。
- 可选 `pricing` 使用每百万 token 的美元价格：`{"input": 5.0, "output": 15.0}`。状态栏和仪表盘会显示估算成本；未配置时不显示。

未配置窗口时会提示预算未知，自动摘要关闭，手动摘要报配置错误。工具输出预览默认 16000 字符；原文可用 read_tool_result 分页读取。

仅验证配置、不请求模型：

```sh
cargo run -p yourai-tui -- --config /path/to/yourai.json --check-config
```

## 操作

| 操作 | 行为 |
|---|---|
| 输入后 Enter | 空闲时启动执行；运行中作为 steer |
| 审批时输入 y / n | 允许或拒绝本次工具调用 |
| 普通提问时输入 JSON | 回复工具问题；文本写成 JSON 字符串，如 `"答案"` |
| Esc / Ctrl-C | 取消当前执行，保留后续队列 |
| `/queue 内容` | 排队为后续 Turn |
| `/compact` | 手动压缩；运行中会拒绝并提示 busy |
| `/models [provider/model [variant]]` | 打开模型选择器或直接切换；仅空闲时可切换，模型与上下文限制一起更新，失败保留原选择 |
| `/sessions` | 打开会话选择器，过滤/切换；当前 turn 进行中会拒绝；Ctrl-D 请求删除，Y 确认，N/Esc 保留 |
| `/status` | 切换仪表盘覆盖层（等价 `^B`） |
| PgUp / PgDn | 滚动输出 |
| Ctrl-B | 开关仪表盘覆盖层（任意宽度） |
| Ctrl-T | 开关 Todo 面板（有任务且宽度 ≥80 时显示，窄屏回退单行 dock） |
| Ctrl-Q / `/quit` | 有序关闭并退出 |
| `/help` | 查看帮助 |

支持流式正文、reasoning、工具事件、token 用量和中文输入/粘贴。编辑器支持多行输入、Unicode 光标移动、Home/End、按词移动与删除、历史导航；`Ctrl-J` 或 `Alt-Enter` 插入换行。完整键位可在 TUI 中按 `F1` 查看。

界面以对话和修改结果为主。顶部不设标题栏，直接展示对话；Ctrl-Home 回到最新提问，Ctrl-↑/↓ 浏览前后提问，Ctrl-End 跟随最新输出。用户提问以独立色块和 YOU 标题区分；工具卡片展示命令结果、退出状态和文件 diff，未知工具展开后保留结构化字段值。输入框支持多行和 Unicode，忙碌时其上方显示当前动作、耗时和 Esc 停止提示。

底栏始终一行，横跨窗口。正常宽度显示标题、目录、累计 token、ctx、最近一次回复速度及权限；窄屏缩略标题和路径，优先保留 ctx 与权限，放不下的次要指标用 … 示意，Ctrl-B 查看模型及完整统计。速度未返回时显示 —。右侧仅在有 Todo 且宽度 ≥80 时出现任务面板，Ctrl-T 收起；没有任务时对话全宽。`/theme dark` 是石墨灰配灰绿强调，`/theme light` 是暖白配墨绿强调。

`/new` 开启全新会话；`/clear` 重置上下文（等同于新会话，旧记录仍在 `/sessions`）。运行或排队中的输入需先结束。`/yolo` 或 Ctrl-G 切换 YOLO，`/yolo on|off` 显式设置；仅在两轮之间切换，运行中先 Esc 停止。关闭后恢复原审批策略；本次进程中的选择在新建/切换会话时保留，不修改持久权限规则。

所有弹层独占输入：Esc 关闭弹层，Ctrl-Q 退出。弹层里的文本、粘贴和鼠标不会操作背后的任务；仅会话搜索接受粘贴。删除会话显示标题和 ID，必须按 Y 确认，Enter 不会删除。帮助支持滚动。最小主界面为 30×10，所有弹层限制在实际终端内。

模型切换在空闲边界同时更新模型、上下文限制及主模型超时；provider 请求 gate 按 provider 与请求策略复用，切回时保留已有冷却/节流状态。主模型、Hook 和子代理仍共享总调用/token 预算；Hook 和子代理保持各自已绑定的模型与 gate。配置没有枚举 models 时，选择器仍包含当前模型。


退出时打印会话 ID，可恢复上下文：

```sh
cargo run -p yourai-tui -- --config /path/to/yourai.json --resume 会话ID
```

`--resume` 不带 ID 时进入启动会话选择器（launcher）：搜索框过滤 title/id/model，`↑↓`/`Ctrl-P/N` 移动，`Enter` 恢复选中会话，`Esc` 开启全新会话，`Ctrl-Q` 直接退出。恢复会话会继续使用原历史；界面恢复原始历史消息（不重复展示压缩摘要）。退出时尚未处理的输入由 close 交还，打印在终端，由调用者决定是否再次提交。

TUI 复用 Harness → SessionHost → DefaultLoop，默认安装代码读写、shell、Web、历史工具结果读取和任务板；`extensions = true` 安装子代理、工作区、记忆和技能扩展。请求错误显示在界面，修改配置后退出重启即可。真实模型连通性由你配置服务后验证。

离线终端冒烟测试使用本地模拟 HTTP 服务和伪终端，不调用外部模型：

```sh
cargo build -p yourai-tui
python3 apps/yourai-tui/tests/smoke.py
python3 apps/yourai-tui/tests/smoke_launcher.py
python3 apps/yourai-tui/tests/smoke_models.py
python3 apps/yourai-tui/tests/smoke_ui.py
```

这些脚本覆盖自定义地址、认证、工具审批、流式响应、手动 compact、SQLite 摘要提交、重启恢复、启动会话选择器、模型切换与终端模式恢复。

## SQLite 会话存储

默认使用 `<session_dir>/sessions.sqlite3`（sessions、messages、usage_events 三张表）。`--resume` 从数据库恢复。宿主队列和扩展配置仍在会话子目录。

旧 JSON 会话先关闭再显式导入；命令不调用模型，保留原文件：

```sh
cargo run -p yourai-tui -- --config /path/to/yourai.json --import-json-sessions
```

导入范围和边界见 [会话存储](./session-storage-design.md)。

模型切换保留当前 Harness 的共享调用预算、限流与计量策略；不会重置已用额度。既有 hook/subagent 执行器继续使用原模型。运行时选择的 variant 在 `/sessions` 切换中保留，重启仍以配置和 CLI 为准。
