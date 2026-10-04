#[path = "support/loop.rs"]
mod support;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use yourai_core::{context::DiscardSink, prelude::*};
use yourai_harness::{model::ConfiguredModel, Harness, HarnessConfig};

fn config(dir: &TempDir) -> HarnessConfig {
    let mut config = HarnessConfig::new(dir.path().join("sessions"), dir.path().into());
    config.system_prompt = Some("frozen system prompt".into());
    config.extensions = true;
    config
}
async fn turn(h: &Harness) -> TurnResult {
    h.host.submit(In::user_text("go")).unwrap();
    h.host
        .run_next(
            TurnLimits::default(),
            &DiscardSink,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap()
        .result
}
#[tokio::test]
async fn dynamic_instructions_reach_requests_without_rewriting_frozen_prompt() {
    let dir = TempDir::new().unwrap();
    let model = Arc::new(support::Model::new(vec![support::answer("done")]));
    let h = Harness::open(config(&dir), model.clone()).await.unwrap();
    std::fs::write(dir.path().join("dynamic.md"), "DYNAMIC_RULE_49521").unwrap();
    h.workspace
        .as_ref()
        .unwrap()
        .load_instructions(std::path::Path::new("dynamic.md"), "test")
        .await
        .unwrap();
    turn(&h).await.unwrap();
    assert!(format!("{:?}", model.requests.lock().unwrap()).contains("DYNAMIC_RULE_49521"));
    let meta = h.sessions.load_session(&h.host.context().id).await.unwrap();
    assert!(!meta.system_prompt.unwrap().contains("DYNAMIC_RULE_49521"));
    h.close().await.unwrap();
}
#[tokio::test]
async fn dynamic_instructions_are_admitted_against_the_input_budget() {
    let dir = TempDir::new().unwrap();
    let raw = Arc::new(support::Model::new(vec![support::answer("unreachable")]));
    let budget = ModelTokenBudget::resolve(
        ModelLimits {
            context: Some(10000),
            output: Some(1000),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let model = ConfiguredModel::new(raw.clone(), budget, ModelTimeouts::default()).unwrap();
    let h = Harness::open(config(&dir), model).await.unwrap();
    std::fs::write(dir.path().join("large.md"), "x".repeat(100000)).unwrap();
    h.workspace
        .as_ref()
        .unwrap()
        .load_instructions(std::path::Path::new("large.md"), "test")
        .await
        .unwrap();
    assert!(turn(&h).await.is_err());
    assert!(raw.requests.lock().unwrap().is_empty());
    h.close().await.unwrap();
}
#[tokio::test]
async fn runtime_rejects_creation_settings_before_persistence() {
    let dir = TempDir::new().unwrap();
    let h = Harness::open(config(&dir), Arc::new(support::Model::new(vec![])))
        .await
        .unwrap();
    let ws = h.workspace.as_ref().unwrap();
    ws.change_config("test", serde_json::json!({"memory_search_limit": 3}))
        .await
        .unwrap();
    let before = ws.config().unwrap();
    assert!(ws
        .change_config("test", serde_json::json!({"system_prompt":"ignored"}))
        .await
        .is_err());
    assert_eq!(before, ws.config().unwrap());
    h.close().await.unwrap();
}
#[tokio::test]
async fn deletion_is_a_catalog_operation_protected_by_the_host_lease() {
    let dir = TempDir::new().unwrap();
    let h = Harness::open(config(&dir), Arc::new(support::Model::new(vec![])))
        .await
        .unwrap();
    let id = h.host.context().id;
    let catalog = yourai_harness::SessionCatalog::new(dir.path().join("sessions")).unwrap();
    assert!(catalog.delete_session(&id).await.is_err());
    assert!(h.sessions.load_session(&id).await.is_ok());
    h.close().await.unwrap();
    catalog.delete_session(&id).await.unwrap();
    assert!(catalog.load_session(&id).await.is_err());
}
struct ReadOnlySkills;
impl SkillProvider for ReadOnlySkills {
    fn list(&self) -> BoxFuture<'_, Result<Vec<SkillInfo>, YourAiError>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn load<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<SkillContent, YourAiError>> {
        Box::pin(async {
            Ok(SkillContent {
                info: SkillInfo {
                    id: "test".into(),
                    name: "test".into(),
                    description: "test".into(),
                    category: None,
                },
                instructions: "test".into(),
                tools: vec!["missing_tool".into()],
            })
        })
    }
}
struct ExternalMemory;
impl MemoryProvider for ExternalMemory {}
#[tokio::test]
async fn external_providers_do_not_expose_an_inactive_local_catalog() {
    let dir = TempDir::new().unwrap();
    let mut cfg = config(&dir);
    cfg.memory_provider = Some(Arc::new(ExternalMemory));
    cfg.skill_provider = Some(Arc::new(ReadOnlySkills));
    let model = Arc::new(support::Model::new(vec![]));
    let h = Harness::open(cfg, model.clone()).await.unwrap();
    assert!(h.local_memory.is_none());
    assert!(h.local_skills.is_none());
    h.workspace
        .as_ref()
        .unwrap()
        .change_config("test", serde_json::json!({"skill_ids":["test"]}))
        .await
        .unwrap();
    assert!(turn(&h)
        .await
        .unwrap_err()
        .error
        .to_string()
        .contains("missing_tool"));
    assert!(model.requests.lock().unwrap().is_empty());
    h.close().await.unwrap();
}
struct UncertainUsage {
    fail_count: usize,
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
            if ids.len() <= self.fail_count {
                return Err(ErrorKind::Provider {
                    name: "usage",
                    message: "uncertain commit".into(),
                }
                .into());
            }
            Ok(())
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
async fn accounting_retries_reuse_identity_and_never_repeat_the_model_call() {
    for fail_count in [2, usize::MAX] {
        let usage = Arc::new(UncertainUsage {
            fail_count,
            ids: Mutex::new(vec![]),
        });
        let model = Arc::new(support::Model::new(vec![support::answer("valid response")]));
        let agent = Agent::builder()
            .model(model.clone())
            .usage(usage.clone())
            .context_manager(Arc::new(support::History::default()))
            .agent_loop(Arc::new(
                yourai_harness::default_loop::DefaultLoop::default(),
            ))
            .build();
        let (events, result) = support::collect(agent.start(In::user_text("go")).unwrap()).await;
        assert_eq!(result.unwrap().text, "valid response");
        assert_eq!(model.requests.lock().unwrap().len(), 1);
        let ids = usage.ids.lock().unwrap();
        assert_eq!(ids.len(), 3);
        assert!(ids.iter().all(|id| id == &ids[0]));
        if fail_count == usize::MAX {
            assert!(format!("{events:?}").contains("Usage accounting incomplete"));
        }
    }
}
