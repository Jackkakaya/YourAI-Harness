//! Custom original AgentLoop examples: no hook protocol and no raw business backends.
#![allow(dead_code)]
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use yourai_core::execution::{Completion, ExecutionConfig, Turn};
use yourai_core::prelude::*;

pub struct ReverseLoop;
impl AgentLoop for ReverseLoop {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            let mut cx = Turn::open(tc, ExecutionConfig::default()).await?;
            let result = async {
                if !cx.input_accepted() {
                    return Ok(());
                }
                loop {
                    let response = cx.model()?.exec(&mut cx, ModelOptions::default()).await?;
                    if response.calls.is_empty() {
                        if cx.complete(response.text).await? == Completion::Completed {
                            return Ok(());
                        }
                    } else {
                        for call in response.calls.iter().rev() {
                            async {
                                let tool = cx.tool(&call.call_id)?;
                                tool.exec(&mut cx, &call.call_id).await
                            }
                            .await?;
                            assert!(async {
                                let tool = cx.tool(&call.call_id)?;
                                tool.exec(&mut cx, &call.call_id).await
                            }
                            .await
                            .is_err());
                        }
                    }
                }
            }
            .await;
            cx.finish(result).await
        })
    }
}
pub struct DirectLoop {
    pub starts: Arc<AtomicUsize>,
    pub runs: Arc<AtomicUsize>,
}
impl AgentLoop for DirectLoop {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            self.starts.fetch_add(1, Ordering::SeqCst);
            let mut cx = Turn::open(tc, ExecutionConfig::default()).await?;
            let result = async {
                if !cx.input_accepted() {
                    return Ok(());
                }
                assert!(!cx.messages().is_empty());
                async {
                    let call = cx.enqueue("tool", serde_json::json!({"value":1})).await?;
                    let tool = cx.tool(&call.call_id)?;
                    tool.exec(&mut cx, &call.call_id).await
                }
                .await?;
                loop {
                    self.runs.fetch_add(1, Ordering::SeqCst);
                    cx.checkpoint().await?;
                    if cx.complete("business complete").await? == Completion::Completed {
                        return Ok(());
                    }
                }
            }
            .await;
            cx.finish(result).await
        })
    }
}
pub struct WaitingLoop(pub Arc<AtomicUsize>);
impl AgentLoop for WaitingLoop {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            let mut cx = Turn::open(tc, ExecutionConfig::default()).await?;
            self.0.fetch_add(1, Ordering::SeqCst);
            let result = cx.wait(std::future::pending()).await;
            cx.finish(result).await
        })
    }
}
pub struct CompactLoop;
impl AgentLoop for CompactLoop {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            let mut cx = Turn::open(tc, ExecutionConfig::default()).await?;
            let result = async {
                if !cx.input_accepted() {
                    return Ok(());
                }
                cx.compact(CompactionTrigger::Manual).await?;
                loop {
                    cx.checkpoint().await?;
                    if cx.complete("compacted").await? == Completion::Completed {
                        return Ok(());
                    }
                }
            }
            .await;
            cx.finish(result).await
        })
    }
}

pub struct OperationsLoop;
impl AgentLoop for OperationsLoop {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            let registry = tc.snap.tools.clone().unwrap();
            let mut cx = Turn::open(tc, ExecutionConfig::default()).await?;
            let result = async {
                if !cx.input_accepted() {
                    return Ok(());
                }
                assert!(!cx.accept_input(In::user_text("reject this")).await?);
                assert!(
                    cx.accept_input(In::user_text("accepted extra input"))
                        .await?
                );
                let tool = registry.resolve("tool")?;
                let mut call = ToolCall {
                    call_id: "authorization".into(),
                    fn_name: "tool".into(),
                    fn_arguments: serde_json::json!({"value":1}),
                    thought_signatures: None,
                };
                yourai_core::permission::authorize(&mut cx, &mut call, &tool, HookPermission::Pass)
                    .await?;
                assert_eq!(call.fn_arguments, serde_json::json!({"value":2}));
                let answer = yourai_core::interaction::elicit(
                    &mut cx,
                    InteractionRequest::new(
                        "business",
                        InteractionKind::McpElicitation {
                            server_name: "test".into(),
                            elicitation_id: None,
                        },
                        serde_json::json!({"message":"choose"}),
                    ),
                )
                .await?;
                assert_eq!(
                    answer,
                    serde_json::json!({"action":"accept","content":{"choice":"after"}})
                );
                loop {
                    cx.checkpoint().await?;
                    if cx.complete("operations complete").await? == Completion::Completed {
                        return Ok(());
                    }
                }
            }
            .await;
            cx.finish(result).await
        })
    }
}

pub struct GuardLoop;
impl AgentLoop for GuardLoop {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            let mut cx = Turn::open(tc, ExecutionConfig::default()).await?;
            let result = async {
                if !cx.input_accepted() {
                    return Ok(());
                }
                let response = cx.model()?.exec(&mut cx, ModelOptions::default()).await?;
                assert_eq!(response.calls.len(), 1);
                // Documented invariants: unresolved calls block every
                // dependent operation, without consuming the model script.
                let err = cx
                    .model()?
                    .exec(&mut cx, ModelOptions::default())
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("resolve pending tool calls"));
                let err = cx.complete("premature").await.unwrap_err();
                assert!(err.to_string().contains("unresolved tool calls"));
                let err = async {
                    let call = cx.enqueue("tool", serde_json::json!({"value":1})).await?;
                    let tool = cx.tool(&call.call_id)?;
                    tool.exec(&mut cx, &call.call_id).await
                }
                .await
                .unwrap_err();
                assert!(err.to_string().contains("resolve pending calls"));
                // Settling clears the ledger; dependent operations resume.
                async {
                    for call in cx.pending_tools() {
                        cx.tool(&call.call_id)?.exec(&mut cx, &call.call_id).await?;
                    }
                    Ok::<_, YourAiError>(())
                }
                .await?;
                loop {
                    let response = cx.model()?.exec(&mut cx, ModelOptions::default()).await?;
                    if response.calls.is_empty()
                        && cx.complete(response.text).await? == Completion::Completed
                    {
                        return Ok(());
                    }
                }
            }
            .await;
            cx.finish(result).await
        })
    }
}

pub struct BoundLoop;
impl AgentLoop for BoundLoop {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            let mut cx = Turn::open(tc, ExecutionConfig::default()).await?;
            let result = async {
                if !cx.input_accepted() {
                    return Ok(());
                }
                loop {
                    let response = cx.model()?.exec(&mut cx, ModelOptions::default()).await?;
                    if response.calls.is_empty() {
                        if cx.complete(response.text).await? == Completion::Completed {
                            return Ok(());
                        }
                    } else {
                        for call in response.calls {
                            let output = async {
                                let tool = cx.tool(&call.call_id)?;
                                tool.exec(&mut cx, &call.call_id).await
                            }
                            .await?;
                            assert!(!output.is_error);
                            assert!(async {
                                let tool = cx.tool(&call.call_id)?;
                                tool.exec(&mut cx, &call.call_id).await
                            }
                            .await
                            .is_err());
                        }
                    }
                }
            }
            .await;
            cx.finish(result).await
        })
    }
}
