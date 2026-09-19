<p align="center"><strong>YourAI Harness</strong> is a modular Rust foundation for building pluggable AI agents.</p>

<p align="center">
  English · <a href="./README.zh-CN.md">简体中文</a>
</p>

---

YourAI separates an agent harness into three responsibilities:

- the frontend owns **pace** — when a turn starts and how events are rendered;
- `yourai-core` owns **transport** — turn lifecycle, channels, cancellation, and provider snapshots;
- `AgentLoop` owns **interpretation** — how messages, models, tools, approvals, memory, and hooks compose.

Everything behind the core seams is replaceable. The loop itself is a provider, not a hard-coded framework policy.

> [!IMPORTANT]
> YourAI implements the core contracts, hooks, DefaultLoop, session hosting, durable history, model-backed compaction and optional extensions. A simple TUI and model configuration are included; a Web frontend and OS sandbox are not included.

## Highlights

- **Pluggable by design** — model, context, session, memory, tools, skills, sandbox, security, usage, observability, hooks, and the agent loop are trait-based providers.
- **Turn-scoped transport** — `Agent::start` returns a `TurnHandle` with inbox, outbox, cancellation, and join semantics.
- **Stable provider snapshots** — providers can be hot-swapped without changing the implementation seen by an in-flight turn.
- **Typed interaction vocabulary** — `yourai-protocol` defines the complete `In` / `Out` language shared by loops and frontends.
- **Claude-compatible Hooks** — 27 hook events, typed outcomes, command and HTTP transports, parallel execution, deterministic aggregation, async completion, and runtime registration.
- **Small object-safe interfaces** — boxed futures keep provider traits usable behind `Arc<dyn Trait>` without `async_trait`.

## Workspace

| Crate | Responsibility |
|---|---|
| `yourai-protocol` | Leaf crate containing the external `In`, `Out`, and usage vocabulary. |
| `yourai-core` | Provider interfaces plus turn transport, lifecycle, cancellation, and snapshots. |
| `yourai-hooks` | Claude-compatible Hook wire protocol and command/HTTP/native runtime adapters. |
| `yourai-loop` | Default single-turn loop: streaming, hooks, compaction coordination, tools, approvals, interaction, limits, and cleanup. |
| `yourai-runtime` | Harness assembly, session host, durable providers, workspace, subagent and task extensions. |
| `yourai-tui` | Simple terminal chat, streaming output, approvals and TOML model configuration. |

Dependency direction stays one-way:

```text
yourai-protocol  ←  yourai-core  ←  yourai-hooks
                               ←  yourai-loop
```

## Quick start

To test a real model, copy `yourai.example.toml` to `yourai.toml`, configure the model, endpoint and key environment variable, then run `cargo run -p yourai-tui`. See [TUI usage](./docs/tui.md).

See [Runtime implementation and verification](./docs/runtime-implementation.md) for all five flow diagrams and the runnable example.

See [DefaultLoop implementation and integration](./docs/default-loop-implementation.md) for configuration and assembly.

Install a recent stable Rust toolchain, then clone and verify the workspace:

```shell
git clone https://github.com/Jackkakaya/YourAI-Harness.git
cd YourAI-Harness
cargo test --workspace
```

The smallest custom loop receives one `TurnContext`, consumes the turn inbox, and emits typed output:

```rust
use std::sync::Arc;
use yourai_core::prelude::*;

struct EchoLoop;

impl AgentLoop for EchoLoop {
    fn run_turn<'a>(
        &'a self,
        tc: TurnContext<'a>,
    ) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            match tc.inbox.recv().await {
                Some(In::UserText { text, .. }) => {
                    if !tc.outbox.send(Out::Chunk { text: text.clone() }) {
                        return Err(YourAiError::Aborted(AbortReason::Disconnected).into());
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

For Hook configuration and dispatch examples, see the [Hook protocol reference](./docs/hook-protocol.md).

## Architecture

```text
Frontend
  │  In / Out
  ▼
Agent + TurnHandle ── transport, cancellation, lifecycle
  │
  ▼
AgentLoop ─────────── interpretation and orchestration
  │
  ├── model / context / session / memory
  ├── tools / skills / sandbox / security
  └── usage / observability / hooks
```

The core deliberately does not choose a model vendor, persistence engine, tool set, UI, or ReAct policy. See the [architecture document](./docs/architecture.md) for the complete rationale and turn model.

## Documentation

- [Architecture and design decisions](./docs/architecture.md)
- [DefaultLoop flow and component design baseline (Chinese)](./docs/default-loop-flow.md)
- [Core component contracts and API migration (Chinese)](./docs/core-contracts.md)
- [Hook protocol and runtime contract](./docs/hook-protocol.md)
- [Claude Code Hook compatibility research](./docs/claude-code-hook-protocol.md)
- [Historical Hook design](./docs/hooks.md)

## Development

```shell
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

The repository uses Rust 2021 and declares `MIT OR Apache-2.0` in workspace package metadata.
