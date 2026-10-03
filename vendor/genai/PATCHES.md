# genai 0.6.5 transport patch

Vendored from the crates.io `genai` 0.6.5 package:

- Upstream: https://github.com/jeremychone/rust-genai
- Registry package SHA-256: `1d12aba7e9dc2c4d54654566dc3dc8383b5cb52e0cfc5754989afe0480d933e3`
- Licenses: MIT OR Apache-2.0; both upstream license files are retained.
- Cargo.toml is upstream Cargo.toml.orig without development-only dependencies,
  with the Tokio `time` feature enabled. Source, README, and formatting config
  are retained. This directory is excluded from workspace membership and patched
  in through the root Cargo.toml, with the version pinned to `=0.6.5`.

## Why this patch is necessary

Upstream emits a synthetic `Start` before sending HTTP. A timer around parsed
model events therefore cannot distinguish waiting for headers from reading a
heartbeat or incomplete event. The adapter API does not expose raw read progress.

## Local changes

`ChatOptions` carries two optional, serde-skipped transport bounds. Per-request
settings override client defaults and are not included in provider JSON or headers.
`Client::exec_chat_stream` places them on a `StreamRequest`, forwarded through the
existing adapter interfaces. The transport changes in adapter interfaces are import substitutions.

The transport wrapper times `RequestBuilder::send()` until the response headers
arrive, then times each `bytes_stream().next()` independently. Every raw chunk
resets the read wait, before SSE/NDJSON/Bedrock parsing. It also bounds error-body
reads. Streaming HTTP failures retain status, body and response headers as a
webc::Error::ResponseFailedStatus, so callers can honor Retry-After. No overall response deadline is added; configured reqwest total timeouts
continue to apply. Dropping the future or stream releases its HTTP request/body.

The OpenAI-compatible streamer also accepts clean EOF after a recognized
`finish_reason` when a provider omits `[DONE]`. It waits for EOF so usage tails
are preserved and transport errors still fail. EOF without a recognized finish
reason remains incomplete. Successful EOF completions validate captured tool
arguments; length/filter reasons remain available to callers, which must reject
truncated/filtered tool calls.

`transport.patch` records the exact source/manifest delta against the registry
package (using Cargo.toml.orig as the manifest baseline). Regenerate it when
updating this patch; do not edit the Cargo registry cache. Remove the local patch
when upstream provides equivalent transport timing.

## Verification

From the repository root:

```
cargo test -p genai --lib
cargo test -p yourai-harness --test model_timeouts --test stream_termination_diagnostics
```

The integration tests exercise delayed headers, a stalled/empty body, independent
header/read budgets, SSE heartbeats, incomplete SSE frames, a progressing summary,
main-loop metering, per-turn overrides, total deadlines, and connection cancellation.
