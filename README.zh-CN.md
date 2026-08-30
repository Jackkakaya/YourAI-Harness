<p align="center"><strong>YourAI Harness</strong> 是一个用于构建可插拔 AI Agent 的模块化 Rust 基础框架。</p>

<p align="center">
  简体中文 · <a href="./README.md">English</a>
</p>

---

YourAI 将 Agent Harness 拆成三种职责：

- 前端拥有**节奏权**——决定何时开始 turn，以及如何渲染事件；
- `yourai-core` 拥有**运输权**——管理 turn 生命周期、通道、取消和 provider 快照；
- `AgentLoop` 拥有**解释权**——决定消息、模型、工具、审批、记忆和 Hook 如何协作。

Core seam 后面的所有能力都可以替换。Loop 本身也是 provider，而不是框架写死的策略。

> [!IMPORTANT]
> YourAI 目前仍处于基础能力阶段。类型化协议、turn 运输、provider interface 和 Hook runtime 已经实现；生产级 DefaultLoop、CLI、TUI 和内置 provider adapter 仍在规划中。

## 主要特性

- **一切皆可插拔**——模型、上下文、会话、记忆、工具、技能、沙箱、安全、用量、可观测性、Hook 和 Agent Loop 都由 trait provider 表达。
- **Turn 级运输机制**——`Agent::start` 返回 `TurnHandle`，统一提供 inbox、outbox、取消与 join 语义。
- **稳定的 provider 快照**——运行时可以热替换 provider，同时保证正在执行的 turn 继续使用启动时的实现。
- **类型化交互词汇**——`yourai-protocol` 定义 Loop 与前端共享的全部 `In` / `Out` 消息。
- **兼容 Claude 的 Hook**——支持 27 种事件、类型化 outcome、Command/HTTP transport、并行执行、确定性聚合、异步完成和运行时注册。
- **精简且对象安全的 interface**——使用 boxed future，让 provider trait 无需 `async_trait` 也能放入 `Arc<dyn Trait>`。

## Workspace

| Crate | 职责 |
|---|---|
| `yourai-protocol` | 叶子 crate，定义外部 `In`、`Out` 和 usage 词汇。 |
| `yourai-core` | Provider interface，以及 turn 运输、生命周期、取消和快照机制。 |
| `yourai-hooks` | Claude 兼容的 Hook wire 协议与 Command/HTTP/Native runtime adapter。 |

依赖方向保持单向：

```text
yourai-protocol  ←  yourai-core  ←  yourai-hooks
       词汇              seam            adapter
```

## 快速开始

安装较新的 Rust stable toolchain，然后克隆并验证 workspace：

```shell
git clone https://github.com/Jackkakaya/YourAI-Harness.git
cd YourAI-Harness
cargo test --workspace
```

最小的自定义 Loop 只接收一个 `TurnContext`，消费 turn inbox，并发送类型化输出：

```rust
use std::sync::Arc;
use yourai_core::prelude::*;

struct EchoLoop;

impl AgentLoop for EchoLoop {
    fn run_turn<'a>(
        &'a self,
        tc: TurnContext<'a>,
    ) -> BoxFuture<'a, Result<TurnOutput, YourAiError>> {
        Box::pin(async move {
            match tc.inbox.recv().await {
                Some(In::UserText { text }) => {
                    if !tc.outbox.send(Out::Chunk { text: text.clone() }) {
                        return Err(YourAiError::Aborted(AbortReason::Disconnected));
                    }
                    Ok(TurnOutput::new(text))
                }
                _ => Err(ErrorKind::Loop("expected user text".into()).into()),
            }
        })
    }
}

let agent = Agent::builder()
    .agent_loop(Arc::new(EchoLoop))
    .build();
```

Hook 配置与 dispatch 示例请参阅 [Hook 协议参考](./docs/hook-protocol.md)。

## 架构

```text
前端
  │  In / Out
  ▼
Agent + TurnHandle ── 运输、取消、生命周期
  │
  ▼
AgentLoop ─────────── 解释与编排
  │
  ├── model / context / session / memory
  ├── tools / skills / sandbox / security
  └── usage / observability / hooks
```

Core 刻意不选择模型厂商、持久化引擎、工具集合、UI 或 ReAct 策略。完整设计理由和 turn 模型请参阅[架构文档](./docs/architecture.md)。

## 文档

- [架构与设计决策](./docs/architecture.md)
- [Hook 协议与 Runtime 契约](./docs/hook-protocol.md)
- [Claude Code Hook 兼容性调研](./docs/claude-code-hook-protocol.md)
- [历史 Hook 设计稿](./docs/hooks.md)

## 开发

```shell
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

仓库使用 Rust 2021，workspace package metadata 声明为 `MIT OR Apache-2.0`。
