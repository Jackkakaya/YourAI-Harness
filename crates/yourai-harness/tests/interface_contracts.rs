#[path = "support/loop.rs"]
mod support;
use std::sync::{Arc, Mutex};
use yourai_core::prelude::*;
use yourai_harness::{
    execution::{ExecutionConfig, ModelOptions, TurnExecution},
    model::ConfiguredModel,
    Harness, HarnessConfig, MemoryContext,
};
fn budget(output: u32) -> ModelTokenBudget {
    ModelTokenBudget::resolve(
        ModelLimits {
            context: Some(300000),
            output: Some(output),
            ..Default::default()
        },
        None,
    )
    .unwrap()
}
#[tokio::test]
async fn conflicting_nested_capacity_is_rejected_at_construction() {
    let raw = Arc::new(support::Model::new(vec![support::answer("ok")]));
    let small = ConfiguredModel::new(raw.clone(), budget(16000), ModelTimeouts::default()).unwrap();
    assert!(ConfiguredModel::new(small, budget(131072), ModelTimeouts::default()).is_err());
    assert!(raw.requests.lock().unwrap().is_empty());
}
struct PrefillLoop;
impl AgentLoop for PrefillLoop {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            let mut cx = TurnExecution::open(tc, ExecutionConfig::default()).await?;
            let result = async {
                let response = cx
                    .model()
                    .exec_with(ModelOptions {
                        prefill: Some("x".repeat(60000)),
                        tools_enabled: false,
                    })
                    .await?;
                cx.complete(response.text).await?;
                Ok(())
            }
            .await;
            cx.finish(result).await
        })
    }
}
#[tokio::test]
async fn prefill_is_included_in_input_admission() {
    let raw = Arc::new(support::Model::new(vec![support::answer("ok")]));
    let tokens = ModelTokenBudget::resolve(
        ModelLimits {
            context: Some(10000),
            output: Some(1000),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let model = ConfiguredModel::new(raw.clone(), tokens, ModelTimeouts::default()).unwrap();
    let history = MemoryContext::new(
        SessionId::new(),
        yourai_harness::context::ContextServices {
            policy: ContextPolicy {
                safety_margin: 100,
                advance_tokens: 100,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let agent = Agent::builder()
        .model(model)
        .context_manager(history)
        .agent_loop(Arc::new(PrefillLoop))
        .build();
    assert!(agent.run(In::user_text("go")).await.is_err());
    let requests = raw.requests.lock().unwrap();
    assert!(
        requests.is_empty(),
        "over-budget request must not reach transport"
    );
}
struct HookCapture(Mutex<Vec<ModelRequest>>);
impl ModelProvider for HookCapture {
    fn model_iden(&self) -> &str {
        "capture"
    }
    fn complete<'a>(&'a self, r: ModelRequest) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
        self.0.lock().unwrap().push(r);
        Box::pin(async {
            let id = genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "capture");
            Ok(ChatResponse {
                content: MessageContent::from_text("{\"ok\":true}"),
                model_iden: id.clone(),
                provider_model_iden: id,
                reasoning_content: None,
                stop_reason: None,
                usage: GenaiUsage::default(),
                captured_raw_body: None,
                response_id: None,
            })
        })
    }
    fn stream_events<'a>(
        &'a self,
        r: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
        self.0.lock().unwrap().push(r);
        Box::pin(async { Ok(support::answer("done")) })
    }
}
#[tokio::test]
async fn inherited_hook_uses_the_model_selected_for_the_new_turn() {
    let dir = tempfile::TempDir::new().unwrap();
    let old = Arc::new(HookCapture(Mutex::new(vec![])));
    let new = Arc::new(HookCapture(Mutex::new(vec![])));
    let mut config = HarnessConfig::new(dir.path().join("sessions"), dir.path().into());
    config.system_prompt = Some("test".into());
    config.hooks=serde_json::from_value(serde_json::json!({"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"prompt","prompt":"check"}]}]}})).unwrap();
    let h = Harness::open(
        config,
        ConfiguredModel::new(old.clone(), budget(16000), ModelTimeouts::default()).unwrap(),
    )
    .await
    .unwrap();
    h.switch_model(
        ConfiguredModel::new(new.clone(), budget(131072), ModelTimeouts::default()).unwrap(),
        ContextPolicy::default(),
    )
    .await
    .unwrap();
    h.host.submit(In::user_text("go")).unwrap();
    h.host
        .run_next(
            TurnLimits::default(),
            &yourai_core::context::DiscardSink,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap()
        .result
        .unwrap();
    assert!(old.0.lock().unwrap().is_empty());
    {
        let requests = new.0.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].source, "hook");
        assert_eq!(requests[0].options.max_tokens, Some(131072));
    }
    h.close().await.unwrap();
}
#[tokio::test]
async fn changing_only_timeout_preserves_output_default() {
    let raw = Arc::new(HookCapture(Mutex::new(vec![])));
    let unchanged = {
        let provider: Arc<dyn ModelProvider> = raw.clone();
        let budget = provider.token_budget();
        let defaults = provider.timeouts();
        ConfiguredModel::new(
            provider,
            budget,
            ModelTimeouts {
                headers: defaults.headers,
                read: defaults.read,
            },
        )
    }
    .unwrap();
    let timed = {
        let provider: Arc<dyn ModelProvider> = raw.clone();
        let budget = provider.token_budget();
        let defaults = provider.timeouts();
        ConfiguredModel::new(
            provider,
            budget,
            ModelTimeouts {
                headers: std::time::Duration::from_secs(7),
                read: defaults.read,
            },
        )
    }
    .unwrap();
    let request = || ModelRequest::new(ChatRequest::from_user("go"), ChatOptions::default());
    unchanged.complete(request()).await.unwrap();
    timed.complete(request()).await.unwrap();
    let requests = raw.0.lock().unwrap();
    assert_eq!(unchanged.token_budget(), timed.token_budget());
    assert_eq!(requests[0].options.max_tokens, Some(32000));
    assert_eq!(requests[1].options.max_tokens, Some(32000));
}

#[test]
fn provider_transactions_publish_consistent_snapshots() {
    let a: Arc<dyn ModelProvider> = Arc::new(support::Model::new(vec![]));
    let b: Arc<dyn ModelProvider> = Arc::new(support::Model::new(vec![]));
    let ah: Arc<dyn ContextManager> = MemoryContext::memory(SessionId::new());
    let bh: Arc<dyn ContextManager> = MemoryContext::memory(SessionId::new());
    let agent = Agent::builder()
        .model(a.clone())
        .context_manager(ah.clone())
        .agent_loop(Arc::new(
            yourai_harness::default_loop::DefaultLoop::default(),
        ))
        .build();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for i in 0..10000 {
                agent.ctx().update(|p| {
                    p.model = Some(if i % 2 == 0 { b.clone() } else { a.clone() });
                    p.context_manager = Some(if i % 2 == 0 { bh.clone() } else { ah.clone() });
                });
            }
        });
        for _ in 0..10000 {
            let snapshot = agent.ctx().snapshot().unwrap();
            assert_eq!(
                Arc::ptr_eq(snapshot.model.as_ref().unwrap(), &a),
                Arc::ptr_eq(snapshot.context_manager.as_ref().unwrap(), &ah)
            );
        }
    });
}

#[tokio::test]
async fn pinned_hook_ignores_the_inherited_selection() {
    use yourai_harness::{
        assembly::model_hooks::DefaultHookModelExecutor,
        hooks::{HookModelExecutor, HookModelRequest},
    };
    let pinned = Arc::new(HookCapture(Mutex::new(vec![])));
    let inherited = Arc::new(HookCapture(Mutex::new(vec![])));
    let executor = DefaultHookModelExecutor {
        selection: ModelSelection::Pinned(
            ConfiguredModel::new(pinned.clone(), budget(16000), ModelTimeouts::default()).unwrap(),
        ),
        tools: None,
        usage: None,
        timeout: std::time::Duration::from_secs(5),
        steps: 1,
    };
    executor
        .evaluate(HookModelRequest {
            prompt: "check".into(),
            model: None,
            agentic: false,
            invocation: HookInvocation::new(
                BaseInput::new("test", ""),
                HookEvent::UserPromptSubmit {
                    prompt: "go".into(),
                },
            )
            .with_model(Some(
                ConfiguredModel::new(inherited.clone(), budget(131072), ModelTimeouts::default())
                    .unwrap(),
            )),
        })
        .await
        .unwrap();
    assert_eq!(pinned.0.lock().unwrap()[0].options.max_tokens, Some(16000));
    assert!(inherited.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn resolved_settings_apply_to_complete_and_stream_requests() {
    {
        let raw = Arc::new(HookCapture(Mutex::new(vec![])));
        let timeout = std::time::Duration::from_secs(7);
        let model = ConfiguredModel::new(
            raw.clone(),
            budget(131072),
            ModelTimeouts {
                headers: timeout,
                ..Default::default()
            },
        )
        .unwrap();
        for explicit in [None, Some(2000)] {
            let request = ModelRequest::new(
                ChatRequest::from_user("go"),
                ChatOptions {
                    max_tokens: explicit,
                    ..Default::default()
                },
            );
            model.complete(request.clone()).await.unwrap();
            let _ = model.stream_events(request).await.unwrap();
        }
        let requests = raw.0.lock().unwrap();
        for (request, expected) in requests.iter().zip([131072, 131072, 2000, 2000]) {
            assert_eq!(request.options.max_tokens, Some(expected));
            assert_eq!(request.options.stream_header_timeout, Some(timeout));
        }
        assert_eq!(requests.len(), 4);
    }
}

#[test]
fn failed_provider_transaction_preserves_the_previous_snapshot() {
    let old: Arc<dyn ModelProvider> = Arc::new(support::Model::new(vec![]));
    let new: Arc<dyn ModelProvider> = Arc::new(support::Model::new(vec![]));
    let agent = Agent::builder()
        .model(old.clone())
        .agent_loop(Arc::new(
            yourai_harness::default_loop::DefaultLoop::default(),
        ))
        .build();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        agent.ctx().update(|providers| {
            providers.model = Some(new);
            panic!("abort staged update");
        });
    }));
    assert!(result.is_err());
    assert!(Arc::ptr_eq(
        agent.ctx().snapshot().unwrap().model.as_ref().unwrap(),
        &old
    ));
}

#[test]
fn context_and_hooks_share_validated_execution_bindings() {
    assert!(ContextExecution::new(
        ExecutionBindings::default(),
        None,
        BaseInput::new("test", "")
    )
    .is_err());
    let old: Arc<dyn ModelProvider> = Arc::new(support::Model::new(vec![]));
    let new: Arc<dyn ModelProvider> = Arc::new(support::Model::new(vec![]));
    let agent = Agent::builder()
        .model(old.clone())
        .agent_loop(Arc::new(
            yourai_harness::default_loop::DefaultLoop::default(),
        ))
        .build();
    let before = ContextExecution::from_snapshot(
        &agent.ctx().snapshot().unwrap(),
        &SessionId::from("test"),
        None,
    )
    .unwrap();
    agent.ctx().set_model(new.clone());
    let after = ContextExecution::from_snapshot(
        &agent.ctx().snapshot().unwrap(),
        &SessionId::from("test"),
        None,
    )
    .unwrap();
    let hook = HookInvocation::new(
        BaseInput::new("test", ""),
        HookEvent::SessionEnd {
            reason: "test".into(),
        },
    )
    .with_execution(before.bindings().clone());
    assert!(Arc::ptr_eq(before.model(), &old));
    assert!(Arc::ptr_eq(
        hook.execution.model.as_ref().unwrap(),
        before.model()
    ));
    assert!(Arc::ptr_eq(after.model(), &new));
    // Editing a caller-owned copy cannot invalidate the validated context.
    let mut detached = before.bindings().clone();
    detached.model = None;
    assert!(ContextExecution::new(detached, None, BaseInput::new("test", "")).is_err());
    assert!(Arc::ptr_eq(before.model(), &old));
}
