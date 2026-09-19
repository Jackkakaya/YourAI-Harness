# 简易 TUI 与模型配置

从仓库根目录运行：

```sh
cp yourai.example.toml yourai.toml
# 编辑 yourai.toml 中的 provider、name、base_url
export OPENAI_API_KEY='你的密钥'
cargo run -p yourai-tui -- --config yourai.toml
```

`yourai.toml` 和 `.yourai/` 已加入 gitignore。配置也可以使用 `api_key` 明文值，但不能与 `api_key_env` 同时设置。不会将密钥打印到界面或写入会话历史。

## 模型配置

```toml
session_dir = ".yourai/sessions"
system_prompt = "你是一个编程助手。"

[model]
provider = "openai"
name = "你的模型名称"
base_url = "https://你的服务地址/v1/"
api_key_env = "OPENAI_API_KEY"

[context]
context_window = 128000 # 示例；按实际模型修改
output_reserve = 4096
keep_recent_tokens = 20000
summary_tokens = 2000
prune_enabled = false
```

- `openai`：Chat Completions，包括兼容该协议的网关。base_url 填基础地址，如 `/v1/`，不要填 `/chat/completions`。
- `openai-responses`：Responses 协议。
- `anthropic`、`gemini`、`ollama`：原生协议。省略 base_url 使用 genai 默认地址；省略密钥配置使用其默认认证方式。
- 本地 OpenAI 兼容服务无需认证时，可移除 api_key_env 并设置 `api_key = "local"`。
- session_dir 相对配置文件解析；工作目录是启动 TUI 时所在目录。

未配置窗口时会提示预算未知，自动摘要关闭，手动摘要报配置错误。工具输出预览默认 16000 字符；原文可用 read_tool_result 分页读取。

仅验证配置、不请求模型：

```sh
cargo run -p yourai-tui -- --config yourai.toml --check-config
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
| PgUp / PgDn | 滚动输出 |
| Ctrl-Q / `/quit` | 有序关闭并退出 |
| `/help` | 查看帮助 |

支持流式正文、reasoning、工具事件、token 用量和中文输入/粘贴。输入编辑保持简单：在末尾输入与退格。

退出时打印会话 ID，可恢复上下文：

```sh
cargo run -p yourai-tui -- --config yourai.toml --resume 会话ID
```

恢复会话会继续使用原历史；界面不会重新展示旧消息。退出时尚未处理的输入由 close 交还，打印在终端，由调用者决定是否再次提交。

TUI 复用 Harness → SessionHost → DefaultLoop，默认仅包含历史工具结果读取能力。顶层 `extensions = true` 才安装 tasks、subagent、工作区、记忆和技能扩展。不含通用 read、附件上传或 shell 工具，这些留到工具模块实现。请求错误显示在界面，修改配置后退出重启即可。真实模型连通性由你配置服务后验证。

离线终端冒烟测试：`cargo build -p yourai-tui && python3 yourai-tui/tests/smoke.py`。使用本地模拟 HTTP 服务和伪终端，覆盖自定义地址、认证、工具审批、流式响应、手动 compact、SQLite 摘要提交、重启恢复与终端模式恢复，不调用外部模型。

## SQLite 会话存储

默认使用 `<session_dir>/sessions.sqlite3`（sessions、messages、usage_events 三张表）。`--resume` 从数据库恢复。宿主队列和扩展配置仍在会话子目录。

旧 JSON 会话先关闭再显式导入；命令不调用模型，保留原文件：

```sh
cargo run -p yourai-tui -- --config yourai.toml --import-json-sessions
```

导入范围和边界见 [会话存储](./session-storage-design.md)。
