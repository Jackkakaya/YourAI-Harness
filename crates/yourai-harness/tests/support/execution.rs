//! Custom original AgentLoop examples: no hook protocol and no raw business backends.
#![allow(dead_code)]
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use yourai_core::prelude::*;
use yourai_harness::execution::{Completion, ExecutionConfig, TurnExecution};

pub struct ReverseLoop;
impl AgentLoop for ReverseLoop {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            let mut cx = TurnExecution::open(tc, ExecutionConfig::default()).await?;
            let result = async {
                if !cx.input_accepted() {
                    return Ok(());
                }
                loop {
                    let response = cx.model().exec().await?;
                    if response.calls.is_empty() {
                        if cx.complete(response.text).await? == Completion::Completed {
                            return Ok(());
                        }
                    } else {
                        for call in response.calls.iter().rev() {
                            cx.tools().exec(&call.call_id).await?;
                            assert!(cx.tools().exec(&call.call_id).await.is_err());
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
            let mut cx = TurnExecution::open(tc, ExecutionConfig::default()).await?;
            let result = async {
                if !cx.input_accepted() {
                    return Ok(());
                }
                assert!(!cx.context().messages().is_empty());
                cx.tools()
                    .call("tool", serde_json::json!({"value":1}))
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
            let mut cx = TurnExecution::open(tc, ExecutionConfig::default()).await?;
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
            let mut cx = TurnExecution::open(tc, ExecutionConfig::default()).await?;
            let result = async {
                if !cx.input_accepted() {
                    return Ok(());
                }
                cx.context().compact(CompactionTrigger::Manual).await?;
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
            let mut cx = TurnExecution::open(tc, ExecutionConfig::default()).await?;
            let result = async {
                if !cx.input_accepted() {
                    return Ok(());
                }
                assert!(!cx.inputs().accept(In::user_text("reject this")).await?);
                assert!(
                    cx.inputs()
                        .accept(In::user_text("accepted extra input"))
                        .await?
                );
                let authorized = cx
                    .permissions()
                    .authorize("tool", serde_json::json!({"value":1}))
                    .await?;
                assert_eq!(authorized, serde_json::json!({"value":2}));
                let answer = cx
                    .interaction()
                    .elicit(InteractionRequest::new(
                        "business",
                        InteractionKind::McpElicitation {
                            server_name: "test".into(),
                            elicitation_id: None,
                        },
                        serde_json::json!({"message":"choose"}),
                    ))
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
            let mut cx = TurnExecution::open(tc, ExecutionConfig::default()).await?;
            let result = async {
                if !cx.input_accepted() {
                    return Ok(());
                }
                let response = cx.model().exec().await?;
                assert_eq!(response.calls.len(), 1);
                // Documented invariants: unresolved calls block every
                // dependent operation, without consuming the model script.
                let err = cx.model().exec().await.unwrap_err();
                assert!(err.to_string().contains("resolve pending tool calls"));
                let err = cx.complete("premature").await.unwrap_err();
                assert!(err.to_string().contains("unresolved tool calls"));
                let err = cx
                    .tools()
                    .call("tool", serde_json::json!({"value":1}))
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("resolve pending calls"));
                // Settling clears the ledger; dependent operations resume.
                cx.tools().exec_pending().await?;
                loop {
                    let response = cx.model().exec().await?;
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

pub struct BoundLoop {
    pub replacement: Arc<dyn ToolHandler>,
}
impl AgentLoop for BoundLoop {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            let binding = tc.snap.tools.as_ref().expect("registry").resolve("tool")?;
            let mut cx = TurnExecution::open(tc, ExecutionConfig::default()).await?;
            let result = async {
                if !cx.input_accepted() {
                    return Ok(());
                }
                loop {
                    let response = cx.model().exec().await?;
                    if response.calls.is_empty() {
                        if cx.complete(response.text).await? == Completion::Completed {
                            return Ok(());
                        }
                    } else {
                        for call in response.calls {
                            assert!(ToolOperation::exec_bound(
                                &mut cx.tools(),
                                &binding,
                                self.replacement.clone(),
                                call.clone()
                            )
                            .await
                            .is_err());
                            let output = binding.exec(&mut cx.tools(), call.clone()).await?;
                            assert!(!output.is_error);
                            assert!(binding.exec(&mut cx.tools(), call).await.is_err());
                        }
                    }
                }
            }
            .await;
            cx.finish(result).await
        })
    }
}
