//! End-to-end contracts for settings, inherited models and paid response accounting.
#[path = "support/loop.rs"]
mod support;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use yourai_core::{context::DiscardSink, prelude::*};
use yourai_harness::{model::ConfiguredModel, Harness, HarnessConfig};

struct Capture {
    name: &'static str,
    requests: Mutex<Vec<ModelRequest>>,
    responses: Mutex<VecDeque<Vec<ChatStreamEvent>>>,
}
impl Capture {
    fn new(name: &'static str, responses: Vec<Vec<ChatStreamEvent>>) -> Arc<Self> {
        Arc::new(Self {
            name,
            requests: Mutex::new(vec![]),
            responses: Mutex::new(responses.into()),
        })
    }
}
impl ModelProvider for Capture {
    fn model_iden(&self) -> &str {
        self.name
    }
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
        self.requests.lock().unwrap().push(request);
        Box::pin(async move {
            let model = genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, self.name);
            Ok(ChatResponse {
                content: MessageContent::from_text(r#"{"ok":true}"#),
                model_iden: model.clone(),
                provider_model_iden: model,
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
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
        let hook = request.source == "hook";
        self.requests.lock().unwrap().push(request);
        let events = if hook {
            vec![support::end(r#"{"ok":true}"#, vec![])]
        } else {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected model replay")
        };
        Box::pin(async move {
            Ok(
                Box::pin(futures_util::stream::iter(events.into_iter().map(Ok)))
                    as ModelEventStream,
            )
        })
    }
}
fn configured(model: Arc<dyn ModelProvider>, output: u32) -> Arc<dyn ModelProvider> {
    ConfiguredModel::new(
        model,
        ModelTokenBudget::resolve(
            ModelLimits {
                context: Some(300_000),
                output: Some(output),
                ..Default::default()
            },
            None,
        )
        .unwrap(),
        ModelTimeouts {
            headers: Duration::from_secs(7),
            read: Duration::from_secs(11),
        },
    )
    .unwrap()
}
fn spawn() -> ChatStreamEvent {
    let mut call = support::call("child", "subagent");
    call.fn_arguments = serde_json::json!({"prompt":"child task"});
    support::end("", vec![call])
}
#[tokio::test]
async fn switching_models_updates_hooks_and_inherited_children_with_one_budget() {
    let dir = tempfile::tempdir().unwrap();
    let old = Capture::new("old", vec![]);
    let new = Capture::new(
        "new",
        vec![
            vec![spawn()],
            vec![support::end("child done", vec![])],
            vec![support::end("parent done", vec![])],
        ],
    );
    let mut config = HarnessConfig::new(dir.path().join("sessions"), dir.path().into());
    config.system_prompt = Some("test".into());
    config.extensions = true;
    config.yolo = true;
    config.hooks = serde_json::from_value(serde_json::json!({"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"prompt","prompt":"check"},{"type":"agent","prompt":"check"}]}]}})).unwrap();
    let h = Harness::open(config, configured(old.clone(), 16_384))
        .await
        .unwrap();
    h.switch_model(configured(new.clone(), 65_536), ContextPolicy::default())
        .await
        .unwrap();
    h.host.submit_async(In::follow_up("go")).await.unwrap();
    let report = h
        .host
        .run_next(
            TurnLimits::default(),
            &DiscardSink,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.result.unwrap().text, "parent done");
    assert!(old.requests.lock().unwrap().is_empty());
    {
        let requests = new.requests.lock().unwrap();
        assert_eq!(requests.iter().filter(|r| r.source == "hook").count(), 4);
        assert_eq!(requests.iter().filter(|r| r.source == "main").count(), 3);
        assert!(requests
            .iter()
            .all(|r| r.options.max_tokens == Some(65_536)));
        assert!(requests
            .iter()
            .all(|r| r.options.stream_header_timeout == Some(Duration::from_secs(7))));
        assert!(requests
            .iter()
            .all(|r| r.options.stream_read_timeout == Some(Duration::from_secs(11))));
        let main: Vec<_> = requests.iter().filter(|r| r.source == "main").collect();
        assert_ne!(main[0].session_id, main[1].session_id);
    }
    assert_eq!(h.usage.total().await.unwrap().request_count, 7);
    h.close().await.unwrap();
}
#[tokio::test]
async fn an_explicitly_pinned_child_keeps_its_model_after_parent_switch() {
    let dir = tempfile::tempdir().unwrap();
    let old = Capture::new("pinned", vec![vec![support::end("pinned result", vec![])]]);
    let new = Capture::new(
        "parent",
        vec![vec![spawn()], vec![support::end("done", vec![])]],
    );
    let mut config = HarnessConfig::new(dir.path().join("sessions"), dir.path().into());
    config.system_prompt = Some("test".into());
    config.extensions = true;
    config.yolo = true;
    let h = Harness::open(config, configured(old.clone(), 16_384))
        .await
        .unwrap();
    h.tools.register(yourai_harness::collaboration::subagent(
        &h.host,
        Some(configured(old.clone(), 16_384)),
        None,
    ));
    h.switch_model(configured(new.clone(), 65_536), ContextPolicy::default())
        .await
        .unwrap();
    h.host.submit_async(In::follow_up("go")).await.unwrap();
    h.host
        .run_next(
            TurnLimits::default(),
            &DiscardSink,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap()
        .result
        .unwrap();
    assert_eq!(old.requests.lock().unwrap().len(), 1);
    assert_eq!(
        old.requests.lock().unwrap()[0].options.max_tokens,
        Some(16_384)
    );
    assert_eq!(new.requests.lock().unwrap().len(), 2);
    h.close().await.unwrap();
}
struct UncertainUsage {
    failures: usize,
    ids: Mutex<Vec<String>>,
}
impl UsageTracker for UncertainUsage {
    fn record_event<'a>(
        &'a self,
        _: &'a SessionId,
        event: &'a UsageEvent,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async move {
            let mut ids = self.ids.lock().unwrap();
            ids.push(event.id.clone());
            if ids.len() <= self.failures {
                Err(ErrorKind::Config("uncertain accounting commit".into()).into())
            } else {
                Ok(())
            }
        })
    }
    fn total(&self) -> BoxFuture<'_, Result<UsageStats, YourAiError>> {
        Box::pin(async { Ok(UsageStats::default()) })
    }
    fn session_usage<'a>(
        &'a self,
        _: &'a SessionId,
    ) -> BoxFuture<'a, Result<UsageStats, YourAiError>> {
        self.total()
    }
    fn reset_session<'a>(&'a self, _: &'a SessionId) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async { Ok(()) })
    }
}
#[tokio::test]
async fn accounting_failure_reuses_one_identity_and_never_repeats_the_model_call() {
    for failures in [2, usize::MAX] {
        let tracker = Arc::new(UncertainUsage {
            failures,
            ids: Mutex::new(vec![]),
        });
        let model = Arc::new(support::Model::new(vec![support::answer("paid answer")]));
        let agent = Agent::builder()
            .model(model.clone())
            .usage(tracker.clone())
            .context_manager(Arc::new(support::History::default()))
            .agent_loop(Arc::new(
                yourai_harness::default_loop::DefaultLoop::default(),
            ))
            .build();
        let (events, report) = support::collect(agent.start(In::follow_up("go")).unwrap()).await;
        assert_eq!(report.unwrap().text, "paid answer");
        assert_eq!(model.requests.lock().unwrap().len(), 1);
        let ids = tracker.ids.lock().unwrap();
        assert_eq!(ids.len(), 3);
        assert!(ids.iter().all(|id| id == &ids[0]));
        if failures == usize::MAX {
            assert!(format!("{events:?}").contains("Usage accounting incomplete"));
        }
    }
}
