# YourAI Harness

一个运行在终端里的编程 Agent，底层是可扩展的 Rust Agent runtime。连接模型服务，在项目目录启动，就可以让它读代码、修改文件、执行命令。

简体中文 · [English](./README.md)

![YourAI TUI：代码 diff、测试结果和任务进度](./docs/screenshots/conversation.png)

*截图来自实际运行的 TUI，使用本地演示模型；文件修改和测试由真实工具执行。[截图复现说明](./docs/screenshots/README.md)。*

## 安装

需要较新的 Rust stable toolchain、Git，以及 macOS 或 Linux 上支持 UTF-8 的终端。

```sh
git clone https://github.com/Jackkakaya/YourAI-Harness.git
cd YourAI-Harness
cargo install --path apps/yourai-tui --locked
```

Cargo 将 `yourai` 安装到 bin 目录，通常是 `~/.cargo/bin`。也可以直接从源码运行：`cargo run -p yourai-tui -- …`。

## 配置模型

先复制示例配置：

```sh
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/yourai"
cp yourai.example.json "${XDG_CONFIG_HOME:-$HOME/.config}/yourai/yourai.json"
```

编辑这个文件，将 **三处** `YOUR_MODEL_NAME` 都替换成服务提供的模型 ID，设置 `baseURL`，并按照模型实际容量填写上下文和输出上限：

```json
{
  "model": "gateway/YOUR_MODEL_NAME",
  "provider": {
    "gateway": {
      "api": "openai-compatible",
      "options": {
        "baseURL": "https://api.openai.com/v1",
        "apiKey": "{env:OPENAI_API_KEY}"
      },
      "models": {
        "YOUR_MODEL_NAME": {
          "id": "YOUR_MODEL_NAME",
          "limit": { "context": 128000, "output": 4096 }
        }
      }
    }
  }
}
```

上面的容量只是示例。`baseURL` 填 API 基础地址，例如 `/v1`，不要填完整的 `/chat/completions` 地址。密钥放到配置引用的环境变量中：

```sh
export OPENAI_API_KEY='你的密钥'
yourai --check-config
```

`--check-config` 只验证配置，不请求模型。默认配置位置是 `$XDG_CONFIG_HOME/yourai/yourai.json`，未设置时使用 `~/.config/yourai/yourai.json`；可通过 `--config PATH` 指定其他文件。

还支持 `openai-responses`、`anthropic`、`gemini` 和 `ollama` API。多模型、思考强度和 variant 的配置见[模型配置说明](./docs/tui.md#模型配置)。

## 在项目中使用

进入想让 Agent 操作的项目目录，再启动：

```sh
cd /path/to/your/project
yourai
```

例如输入：“找到失败的测试，修复问题，然后运行相关测试。”Agent 可以读写和编辑文件、执行 Shell、搜索和读取网页、跟踪任务。在配置里设置 `"extensions": true`，可以启用子 Agent、工作区管理、记忆和技能扩展。

| 操作 | 用法 |
|---|---|
| 发送消息 | 输入后按 **Enter**；任务运行中发送的新消息默认排队，下一轮执行。 |
| 引导当前任务 | 点击队列消息上的 **Steer**。 |
| 添加文件或图片 | 输入 **@** 选择文件或目录；**Ctrl-V** 添加剪贴板图片。 |
| 多行输入 | **Ctrl-J** 或 **Alt-Enter**；**Ctrl-X** 用 `$VISUAL` / `$EDITOR` 编辑草稿。 |
| 工具审批 | **y** 允许一次；**a** 同时记住该工具在本会话的授权；**n** 拒绝。 |
| 停止当前任务 | **Esc** 或 **Ctrl-C**；后续排队消息保留。 |
| 查看结果 | 点击工具或思考块展开；**PgUp/PgDn** 滚动，**Ctrl-End** 跟随最新输出。 |
| 切换模型 | **/models**，按 **Enter** 选择思考强度，再按 **Enter** 应用；**F2** 循环切换已配置条目。 |
| 统计和任务 | **Ctrl-B** 打开统计；**Ctrl-T** 开关任务面板。 |
| 新对话与压缩 | **/new** 新建会话；**/clear** 从新会话开始，旧会话保留；**/compact** 生成历史摘要。 |
| 主题与帮助 | **/theme dark**、**/theme light**、**/help**。 |
| 退出 | **Ctrl-Q** 或 **/quit**。 |

切换模型、会话和手动压缩，需要先结束或停止当前执行。完整键位和行为见[操作说明](./docs/tui.md#操作)。

### 工具审批

![YourAI 在执行 Shell 命令前请求审批](./docs/screenshots/approval.png)

### 模型选择

![YourAI 模型选择器：配置的模型、variant 和思考强度](./docs/screenshots/models.png)

## 恢复会话

```sh
yourai --resume              # 打开已保存会话的选择器
yourai --resume SESSION_ID   # 直接恢复指定会话
```

也可以在应用内使用 **/sessions** 切换；退出时会打印会话 ID。

Agent 数据统一保存到 SQLite：消息、摘要、排队输入、运行状态、任务、用量、扩展数据和完整工具输出。全新安装默认保存到 `${XDG_DATA_HOME:-$HOME/.local/share}/yourai/sessions/sessions.sqlite3`，可通过 `session_dir` 指定目录。自定义相对路径以配置文件目录为基准；检测到已有旧存储位置时会继续沿用。

调用模型前，上下文管理器先限制大工具输出，再将符合条件的旧工具正文替换为状态和回看引用。如果请求仍接近容量上限，`Compactor` 才调用模型生成滚动历史摘要。原始消息仍在 SQLite，Agent 可以通过 `read` 找回工具输出。详见[上下文与压缩](./docs/context-manager-design.md)。

旧 JSON 会话需要先关闭旧客户端，再导入一次：

```sh
yourai --import-json-sessions
```

## 基于 runtime 开发

| 包 | 职责 |
|---|---|
| `yourai-core` | 协议与 Provider 接口、带 Hook 的公共执行模板、turn/会话生命周期、取消和 Provider 快照。 |
| `yourai-harness` | DefaultLoop、模型适配与计量、SQLite 存储、上下文投影、工具、任务和可选扩展。 |
| `yourai-tui` | 终端客户端、流式输出、审批、模型/会话选择、主题、Markdown 和 diff 渲染。 |

依赖方向：`yourai-core ← yourai-harness ← yourai-tui`。`AgentLoop` 可以替换；具体工具实现业务执行，core 的公共入口负责 Hook 流程。目前未提供 Web 前端和操作系统沙箱。

- [可运行的集成示例](./crates/yourai-harness/examples/run.rs)
- [架构设计](./docs/architecture.md) · [公共执行接口](./docs/execution.md)
- [Hook 协议](./docs/hook-protocol.md) · [SQLite 存储与云端方向](./docs/cloud-storage.md)
- [TUI 使用说明](./docs/tui.md) · [当前类型清单](./docs/current-type-inventory.md)

## 开发验证

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo build -p yourai-tui
python3 apps/yourai-tui/tests/smoke.py
```

使用 Rust 2021，workspace package metadata 声明为 `MIT OR Apache-2.0`。
