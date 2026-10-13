//! Configuration ownership across assembly, turn limits and persisted updates.
#[path = "support/loop.rs"]
mod support;

use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use yourai_core::{context::DiscardSink, prelude::*};
use yourai_harness::{Harness, SessionOptions};

struct Probe(Arc<Mutex<Vec<ExecutionOptions>>>);
impl AgentLoop for Probe {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            let mut turn = Turn::open(tc).await?;
            self.0.lock().unwrap().push(turn.config().clone());
            let result = turn.complete("done").await.map(|_| ());
            turn.finish(result).await
        })
    }
}

async fn run(harness: &Harness, limits: TurnLimits) {
    harness
        .host
        .submit_async(In::user_text("first committed title"))
        .await
        .unwrap();
    harness
        .host
        .run_next(limits, &DiscardSink, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap()
        .result
        .unwrap();
}

#[tokio::test]
async fn shared_config_updates_the_next_turn_and_reload_without_session_overrides() {
    use yourai_harness::config::{self, Config};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("yourai.json");
    std::fs::write(
        &path,
        r#"{"model":"mock/a","steps":5,"max_model_retries":2,"provider":{}}"#,
    )
    .unwrap();
    let shared = Arc::new(Mutex::new(Config::load(&path).unwrap()));
    let seen = Arc::new(Mutex::new(vec![]));
    let mut options = SessionOptions::new(dir.path().join("sessions"), dir.path().into());
    options.config.prompt.system_prompt = Some("test".into());
    options.agent_loop = Some(Arc::new(Probe(seen.clone())));
    options.config = shared.lock().unwrap().clone();
    let h = Harness::open(options.clone(), Arc::new(support::Model::new(vec![])))
        .await
        .unwrap();
    let source = shared.clone();
    h.host
        .bind_config_source(Arc::new(move || source.lock().unwrap().execution_options()))
        .unwrap();
    let mut limits = TurnLimits::default();
    limits.steps = Some(3);
    run(&h, limits).await;
    config::update(&shared, |config| {
        config.execution.memory_search_limit = 7;
        Ok(())
    })
    .unwrap();
    run(&h, TurnLimits::default()).await;
    let configs = seen.lock().unwrap().clone();
    assert_eq!(configs[0].steps, Some(3));
    assert_eq!(configs[0].max_model_retries, 2);
    assert_eq!(configs[0].memory_search_limit, 0);
    assert_eq!(configs[1].steps, Some(5));
    assert_eq!(configs[1].memory_search_limit, 7);
    assert!(h
        .host
        .store()
        .read_state(&h.host.environment().id, "config")
        .unwrap()
        .is_none());
    options.resume = Some(h.host.environment().id);
    h.close().await.unwrap();
    options.config = Config::load(&path).unwrap();
    let resumed = Harness::open(options, Arc::new(support::Model::new(vec![])))
        .await
        .unwrap();
    assert_eq!(resumed.host.config().memory_search_limit, 7);
    resumed.close().await.unwrap();
}

#[tokio::test]
async fn child_inherits_execution_limits_without_activating_parent_skills() {
    let dir = tempfile::tempdir().unwrap();
    let mut options = SessionOptions::new(dir.path().join("sessions"), dir.path().into());
    options.config.prompt.system_prompt = Some("test".into());
    options.config.session.extensions = true;
    options.config.execution.steps = Some(1);
    options.config.execution.skill_ids = vec!["parent-only".into()];
    let h = Harness::open(options, Arc::new(support::Model::new(vec![])))
        .await
        .unwrap();
    h.skills
        .as_ref()
        .unwrap()
        .register(SkillContent {
            info: SkillInfo {
                id: "parent-only".into(),
                name: "Parent".into(),
                description: "Parent-only instructions".into(),
                category: None,
            },
            instructions: "PARENT_ONLY_SKILL".into(),
            tools: vec![],
        })
        .await
        .unwrap();
    let model = Arc::new(support::Model::new(vec![support::answer("child done")]));
    let child = yourai_harness::collaboration::subagent(&h.host, Some(model.clone()), None);
    let cancel = CancellationToken::new();
    let tc = ToolContext {
        providers: None,
        call_id: "child".into(),
        cwd: None,
        emit: &DiscardSink,
        cancel: &cancel,
        security: None,
        sandbox: None,
        interaction: None,
    };
    let (_, answer) = child.exec(tc, "work".into()).await.unwrap();
    assert_eq!(answer, "child done");
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].request.tools.as_ref().is_none_or(Vec::is_empty));
        assert!(!format!("{:?}", requests[0].request).contains("PARENT_ONLY_SKILL"));
    }
    assert_eq!(h.host.config().skill_ids, vec!["parent-only"]);
    h.close().await.unwrap();
}

#[tokio::test]
async fn title_backfill_failure_does_not_prevent_resume() {
    let dir = tempfile::tempdir().unwrap();
    let mut options = SessionOptions::new(dir.path().join("sessions"), dir.path().into());
    options.config.prompt.system_prompt = Some("test".into());
    options.agent_loop = Some(Arc::new(Probe(Arc::new(Mutex::new(vec![])))));
    let h = Harness::open(options.clone(), Arc::new(support::Model::new(vec![])))
        .await
        .unwrap();
    run(&h, TurnLimits::default()).await;
    options.resume = Some(h.host.environment().id);
    h.close().await.unwrap();
    let db = rusqlite::Connection::open(options.root.join("sessions.sqlite3")).unwrap();
    db.execute_batch("UPDATE sessions SET title=NULL; CREATE TRIGGER fail_title BEFORE UPDATE OF title ON sessions WHEN NEW.title IS NOT NULL BEGIN SELECT RAISE(ABORT, 'title unavailable'); END;").unwrap();
    drop(db);
    let resumed = Harness::open(options, Arc::new(support::Model::new(vec![])))
        .await
        .unwrap();
    assert_eq!(resumed.host.title().await.unwrap(), None);
    run(&resumed, TurnLimits::default()).await;
    assert_eq!(resumed.host.status(), SessionStatus::Idle);
    resumed.close().await.unwrap();
}

#[tokio::test]
async fn session_factories_preserve_bindings_separately_from_runtime_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let seen = Arc::new(Mutex::new(vec![]));
    let probe: Arc<dyn AgentLoop> = Arc::new(Probe(seen.clone()));
    let model: Arc<dyn ModelProvider> = Arc::new(support::Model::new(vec![]));
    let summary: Arc<dyn ModelProvider> = Arc::new(support::Model::new(vec![]));
    let catalog = yourai_harness::SessionCatalog::new(&root).unwrap();
    let usage: Arc<dyn UsageTracker> = Arc::new(yourai_harness::storage::LocalUsage(
        (*catalog.store).clone(),
    ));
    let hooks = Arc::new(support::Hooks::new(|_, _| {}));
    let bindings = Agent::builder()
        .model(model.clone())
        .compaction_model(summary.clone())
        .agent_loop(probe.clone())
        .usage(usage.clone())
        .hooks(hooks.clone());
    let first = yourai_harness::runtime::create(&root, dir.path().into(), bindings)
        .await
        .unwrap();
    let id = first.environment().id;
    assert!(Arc::ptr_eq(&first.agent().usage().unwrap(), &usage));
    assert!(Arc::ptr_eq(
        &first.agent().compaction_model().unwrap(),
        &summary
    ));
    first.submit_async(In::user_text("first")).await.unwrap();
    first
        .run_next(
            TurnLimits::default(),
            &DiscardSink,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap()
        .result
        .unwrap();
    first.close(None).await.unwrap();

    let settings = ExecutionOptions {
        steps: Some(3),
        ..Default::default()
    };
    let restored = yourai_harness::runtime::restore(
        &root,
        id,
        dir.path().into(),
        Agent::builder()
            .model(model)
            .agent_loop(probe)
            .usage(usage.clone())
            .hooks(hooks.clone()),
        settings,
        "resume",
    )
    .await
    .unwrap();
    assert!(Arc::ptr_eq(&restored.agent().usage().unwrap(), &usage));
    restored
        .submit_async(In::user_text("second"))
        .await
        .unwrap();
    restored
        .run_next(
            TurnLimits::default(),
            &DiscardSink,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap()
        .result
        .unwrap();
    assert_eq!(seen.lock().unwrap()[0].steps, None);
    assert_eq!(seen.lock().unwrap()[1].steps, Some(3));
    assert_eq!(
        hooks
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|kind| **kind == HookEventKind::SessionStart)
            .count(),
        2
    );
    restored.close(None).await.unwrap();
}

#[tokio::test]
async fn session_factory_rejects_missing_model_before_creating_storage() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("not-created");
    let result = yourai_harness::runtime::create(&root, dir.path().into(), Agent::builder()).await;
    assert!(matches!(
        result,
        Err(YourAiError::Error(ErrorKind::Config(_)))
    ));
    assert!(!root.exists());
}

fn config_files(directory: &std::path::Path) -> yourai_harness::config::Config {
    let path = directory.join("yourai.json");
    std::fs::write(&path, serde_json::to_vec(&serde_json::json!({
        "model":"mock/a", "provider":{"mock":{
            "options":{"baseURL":"http://127.0.0.1:1/v1","apiKey":"{env:YOURAI_TEST_UNUSED_KEY}"},
            "models":{"a":{"options":{"reasoningEffort":"low"}},"b":{"variants":{"short":{"reasoningEffort":"medium"}}}}
        }}
    })).unwrap()).unwrap();
    std::fs::write(
        directory.join("prompts.json"),
        r#"{"system_prompt":"original prompt","soul_file":"soul.md"}"#,
    )
    .unwrap();
    // Tests use an ordinary placeholder credential, without asking any real service.
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["provider"]["mock"]["options"]["apiKey"] = "test-only".into();
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    yourai_harness::config::Config::load(&path).unwrap()
}

#[test]
fn ordinary_and_prompt_files_load_once_and_save_independently() {
    use yourai_harness::config::{self, Config};
    let dir = tempfile::tempdir().unwrap();
    let shared = Arc::new(Mutex::new(config_files(dir.path())));
    let prompt_path = dir.path().join("prompts.json");
    let original_prompt = std::fs::read(&prompt_path).unwrap();
    config::update(&shared, |config| {
        config.security.yolo = true;
        config.ui.theme = "nord".into();
        Ok(())
    })
    .unwrap();
    assert_eq!(std::fs::read(&prompt_path).unwrap(), original_prompt);
    let main_path = dir.path().join("yourai.json");
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&main_path).unwrap()).unwrap();
    assert_eq!(saved["security"]["yolo"], true);
    assert!(saved.get("system_prompt").is_none());
    assert!(saved.get("soul_file").is_none());
    assert!(saved.get("prompt").is_none());
    assert_eq!(shared.lock().unwrap().ui.theme, "nord");
    // Out-of-process edits do not affect the cached values.
    let mut edited = saved.clone();
    edited["ui"]["theme"] = "light".into();
    std::fs::write(&main_path, serde_json::to_vec(&edited).unwrap()).unwrap();
    std::fs::write(&prompt_path, r#"{"system_prompt":"externally changed"}"#).unwrap();
    assert_eq!(shared.lock().unwrap().ui.theme, "nord");
    assert_eq!(
        shared.lock().unwrap().prompt.system_prompt.as_deref(),
        Some("original prompt")
    );
    let reloaded = Config::load(&main_path).unwrap();
    assert_eq!(reloaded.ui.theme, "light");
    assert_eq!(
        reloaded.prompt.system_prompt.as_deref(),
        Some("externally changed")
    );
    // An in-app prompt edit touches its own file, never the ordinary config.
    let main_before = std::fs::read(&main_path).unwrap();
    config::update(&shared, |prompt| {
        prompt.prompt.system_prompt = Some("edited in app".into());
        Ok(())
    })
    .unwrap();
    assert_eq!(std::fs::read(&main_path).unwrap(), main_before);
    assert_eq!(
        shared.lock().unwrap().prompt.system_prompt.as_deref(),
        Some("edited in app")
    );
    assert_eq!(
        Config::load(&main_path)
            .unwrap()
            .prompt
            .system_prompt
            .as_deref(),
        Some("edited in app")
    );
}

#[test]
fn legacy_inline_prompts_are_preserved_when_the_main_config_is_saved() {
    use yourai_harness::config::{self, Config};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("yourai.json");
    std::fs::write(&path, r#"{"model":"mock/a","provider":{},"system_prompt":"legacy prompt","append_system_prompt":"additional","profile_file":"profile.md"}"#).unwrap();
    let shared = Arc::new(Mutex::new(Config::load(&path).unwrap()));
    assert!(
        !dir.path().join("prompts.json").exists(),
        "load must not migrate files"
    );
    config::update(&shared, |config| {
        config.security.yolo = true;
        Ok(())
    })
    .unwrap();
    let main: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(main.get("system_prompt").is_none());
    let prompt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("prompts.json")).unwrap()).unwrap();
    assert_eq!(prompt["system_prompt"], "legacy prompt");
    assert_eq!(prompt["append_system_prompt"], "additional");
    assert_eq!(prompt["profile_file"], "profile.md");
    assert_eq!(
        Config::load(&path).unwrap().prompt.system_prompt.as_deref(),
        Some("legacy prompt")
    );
}

#[test]
fn failed_file_save_does_not_publish_new_values() {
    use yourai_harness::config;
    let dir = tempfile::tempdir().unwrap();
    let shared = Arc::new(Mutex::new(config_files(dir.path())));
    let path = dir.path().join("yourai.json");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let prompt_before = std::fs::read(dir.path().join("prompts.json")).unwrap();
    assert!(config::update(&shared, |config| {
        config.security.yolo = true;
        Ok(())
    })
    .is_err());
    assert!(!shared.lock().unwrap().security.yolo);
    assert_eq!(
        std::fs::read(dir.path().join("prompts.json")).unwrap(),
        prompt_before
    );
    std::fs::remove_file(dir.path().join("prompts.json")).unwrap();
    std::fs::create_dir(dir.path().join("prompts.json")).unwrap();
    assert!(config::update(&shared, |prompt| {
        prompt.prompt.system_prompt = Some("must not publish".into());
        Ok(())
    })
    .is_err());
    assert_eq!(
        shared.lock().unwrap().prompt.system_prompt.as_deref(),
        Some("original prompt")
    );
}

#[tokio::test]
async fn model_switch_saves_global_choices_and_file_failure_keeps_live_bindings() {
    use yourai_harness::{
        config::{Config, EffortChoice},
        controller,
    };
    let dir = tempfile::tempdir().unwrap();
    let mut initial = config_files(dir.path());
    initial.prompt.soul_file = None;
    let mut options = SessionOptions::new(dir.path().join("sessions"), dir.path().into());
    initial.apply_to(&mut options);
    let model = initial.resolve(None).unwrap().model;
    let h = Harness::open(options.clone(), model).await.unwrap();
    let shared = Arc::new(Mutex::new(initial));
    let path = dir.path().join("yourai.json");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(
        controller::switch_model(&shared, &h, "mock/b", Some("short".into()), None)
            .await
            .is_err()
    );
    assert_eq!(shared.lock().unwrap().models.selected, "mock/a");
    assert_eq!(h.host.agent().model().unwrap().model_iden(), "a");
    std::fs::remove_dir(&path).unwrap();
    controller::switch_model(
        &shared,
        &h,
        "mock/b",
        Some("short".into()),
        Some(EffortChoice::Set("high".into())),
    )
    .await
    .unwrap();
    assert_eq!(h.host.agent().model().unwrap().model_iden(), "b");
    let reloaded = Config::load(&path).unwrap();
    assert_eq!(reloaded.models.selected, "mock/b");
    assert_eq!(reloaded.models.variant.as_deref(), Some("short"));
    assert_eq!(
        reloaded
            .effective_effort("mock/b", Some("short"))
            .as_deref(),
        Some("high")
    );
    assert!(h
        .host
        .store()
        .read_state(&h.host.environment().id, "model_preferences")
        .unwrap()
        .is_none());
    // Resuming a different session never restores a competing model preference.
    let loaded = Arc::new(Mutex::new(reloaded));
    let (_, model) =
        controller::prepare_session(&loaded, &options, Some(h.host.environment().id)).unwrap();
    assert_eq!(model.model_iden(), "b");
    h.close().await.unwrap();
}

#[test]
fn hooks_can_stay_inline_or_use_a_separate_cached_json_source() {
    use yourai_harness::config::{self, Config};
    let dir = tempfile::tempdir().unwrap();
    config_files(dir.path());
    let main = dir.path().join("yourai.json");
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&main).unwrap()).unwrap();
    let hooks = serde_json::json!({"PreToolUse":[{"hooks":[{"type":"command","command":"echo original"}]}]});
    document["hooks"] = hooks.clone();
    std::fs::write(&main, serde_json::to_vec(&document).unwrap()).unwrap();
    let inline = Arc::new(std::sync::Mutex::new(Config::load(&main).unwrap()));
    config::update(&inline, |value| {
        value.hooks.events = serde_json::json!({});
        Ok(())
    })
    .unwrap();
    assert_eq!(
        Config::load(&main).unwrap().hooks.events,
        serde_json::json!({})
    );

    std::fs::create_dir(dir.path().join("settings")).unwrap();
    let external = dir.path().join("settings/hooks.json");
    std::fs::write(
        &external,
        serde_json::to_vec(&serde_json::json!({"hooks":hooks})).unwrap(),
    )
    .unwrap();
    document.as_object_mut().unwrap().remove("hooks");
    document["hooks_file"] = "settings/hooks.json".into();
    std::fs::write(&main, serde_json::to_vec(&document).unwrap()).unwrap();
    let shared = Arc::new(std::sync::Mutex::new(Config::load(&main).unwrap()));
    let original = std::fs::read(&external).unwrap();
    config::update(&shared, |value| {
        value.security.yolo = true;
        Ok(())
    })
    .unwrap();
    assert_eq!(std::fs::read(&external).unwrap(), original);
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&main).unwrap()).unwrap();
    assert!(saved["hooks"].get("events").is_none());
    assert_eq!(saved["hooks"]["file"], "settings/hooks.json");
    std::fs::write(&external, r#"{"hooks":{}}"#).unwrap();
    assert_eq!(shared.lock().unwrap().hooks.events, hooks);
    assert_eq!(
        Config::load(&main).unwrap().hooks.events,
        serde_json::json!({})
    );
    let main_before = std::fs::read(&main).unwrap();
    config::update(&shared, |value| {
        value.hooks.events = serde_json::json!({});
        Ok(())
    })
    .unwrap();
    assert_eq!(std::fs::read(&main).unwrap(), main_before);
    assert_eq!(shared.lock().unwrap().hooks.events, serde_json::json!({}));
    std::fs::remove_file(&external).unwrap();
    std::fs::create_dir(&external).unwrap();
    assert!(config::update(&shared, |value| {
        value.hooks.events["Stop"] = serde_json::json!([]);
        Ok(())
    })
    .is_err());
    assert_eq!(shared.lock().unwrap().hooks.events, serde_json::json!({}));
}

#[test]
fn external_hooks_reject_conflicts_missing_and_malformed_files() {
    use yourai_harness::config::Config;
    let dir = tempfile::tempdir().unwrap();
    config_files(dir.path());
    let main = dir.path().join("yourai.json");
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&main).unwrap()).unwrap();
    document["hooks_file"] = "hooks.json".into();
    std::fs::write(&main, serde_json::to_vec(&document).unwrap()).unwrap();
    assert!(Config::load(&main).is_err());
    for invalid in ["{", "{}", r#"{"hook":{}}"#, r#"{"hooks":[]}"#] {
        std::fs::write(dir.path().join("hooks.json"), invalid).unwrap();
        assert!(Config::load(&main).is_err());
    }
    std::fs::write(dir.path().join("hooks.json"), r#"{"hooks":{"events":{}}}"#).unwrap();
    assert_eq!(
        Config::load(&main).unwrap().hooks.events,
        serde_json::json!({})
    );
    document["hooks"] = serde_json::json!({"Stop":[]});
    std::fs::write(&main, serde_json::to_vec(&document).unwrap()).unwrap();
    assert!(Config::load(&main).is_err());
}

#[test]
fn one_update_edits_cached_fields_and_saves_without_reloading_any_file() {
    use yourai_harness::config::{self, Config};
    let dir = tempfile::tempdir().unwrap();
    let shared = Arc::new(Mutex::new(config_files(dir.path())));
    let main = dir.path().join("yourai.json");
    let prompt = dir.path().join("prompts.json");
    // A corrupt external edit cannot influence in-app edits of the cached Config.
    std::fs::write(&main, "invalid external edit").unwrap();
    std::fs::write(&prompt, "invalid external edit").unwrap();
    config::update(&shared, |value| {
        value.execution.steps = Some(3);
        value.security.yolo = true;
        value.ui.theme = "nord".into();
        value.prompt.system_prompt = Some("live cached prompt".into());
        value.hooks.file = Some("hooks.json".into());
        value.hooks.events = serde_json::json!({"PreToolUse":[{"hooks":[{"type":"command","command":"echo cached"}]}]});
        Ok(())
    }).unwrap();
    {
        let live = shared.lock().unwrap();
        assert_eq!(live.execution.steps, Some(3));
        assert!(live.security.yolo);
        assert_eq!(live.ui.theme, "nord");
        assert_eq!(
            live.prompt.system_prompt.as_deref(),
            Some("live cached prompt")
        );
        assert_eq!(live.execution_options().steps, Some(3));
    }
    let reloaded = Config::load(&main).unwrap();
    assert_eq!(reloaded.execution.steps, Some(3));
    assert_eq!(
        reloaded.prompt.system_prompt.as_deref(),
        Some("live cached prompt")
    );
    assert_eq!(reloaded.hooks.events, shared.lock().unwrap().hooks.events);
    let separate = std::fs::read(dir.path().join("hooks.json")).unwrap();
    config::update(&shared, |value| {
        value.hooks.file = None;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("hooks.json")).unwrap(),
        separate
    );
    let inline = Config::load(&main).unwrap();
    assert!(inline.hooks.file.is_none());
    assert_eq!(inline.hooks.events, shared.lock().unwrap().hooks.events);
}

#[test]
fn grouped_configuration_roundtrips_and_rejects_ambiguous_or_unknown_fields() {
    use yourai_harness::config::{self, Config};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("yourai.json");
    let document = serde_json::json!({
        "models":{"selected":"mock/a", "providers":{}},
        "execution":{"steps":4,"context":{"summary_tokens":1234},"attachment_image":{"max_width":1200}},
        "security":{"yolo":false},"ui":{"theme":"nord"},
        "session":{"directory":"sessions","extensions":true},
        "prompt":{"system_prompt":"inline grouped prompt"},"hooks":{"events":{}}
    });
    std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    let shared = Arc::new(Mutex::new(Config::load(&path).unwrap()));
    assert_eq!(shared.lock().unwrap().execution.steps, Some(4));
    assert_eq!(
        shared.lock().unwrap().execution.attachment_image.max_width,
        1200
    );
    config::update(&shared, |value| {
        value.security.yolo = true;
        Ok(())
    })
    .unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["execution"]["context"]["summary_tokens"], 1234);
    assert_eq!(saved["models"]["selected"], "mock/a");
    assert!(saved.get("model").is_none());
    assert!(saved.get("steps").is_none());
    assert!(saved.get("prompt").is_none());
    assert_eq!(
        Config::load(&path).unwrap().prompt.system_prompt.as_deref(),
        Some("inline grouped prompt")
    );
    for (category, key) in [
        ("models", "unknown"),
        ("execution", "unknown"),
        ("security", "unknown"),
        ("ui", "unknown"),
        ("session", "unknown"),
        ("prompt", "unknown"),
        ("hooks", "unknown"),
    ] {
        let mut invalid = document.clone();
        invalid[category][key] = true.into();
        assert!(
            serde_json::from_value::<Config>(invalid).is_err(),
            "{category}.{key}"
        );
    }
    for (legacy, category, key) in [
        ("model", "models", "selected"),
        ("steps", "execution", "steps"),
        ("theme", "ui", "theme"),
        ("yolo", "security", "yolo"),
    ] {
        let mut invalid = document.clone();
        invalid[legacy] = invalid[category][key].clone();
        assert!(
            serde_json::from_value::<Config>(invalid).is_err(),
            "duplicate {legacy}"
        );
    }
}

#[test]
fn hook_source_aliases_are_rejected_before_any_category_is_saved() {
    use yourai_harness::config;
    let dir = tempfile::tempdir().unwrap();
    let shared = Arc::new(Mutex::new(config_files(dir.path())));
    let main = dir.path().join("yourai.json");
    let prompt = dir.path().join("prompts.json");
    let main_before = std::fs::read(&main).unwrap();
    let prompt_before = std::fs::read(&prompt).unwrap();
    std::fs::create_dir(dir.path().join("nested")).unwrap();
    let aliases = vec![
        std::path::PathBuf::from("yourai.json"),
        "./yourai.json".into(),
        "nested/../yourai.json".into(),
        "prompts.json".into(),
        main.clone(),
        prompt.clone(),
    ];
    #[cfg(unix)]
    let aliases = {
        let mut aliases = aliases;
        std::os::unix::fs::symlink(&main, dir.path().join("main-link.json")).unwrap();
        aliases.push("main-link.json".into());
        aliases
    };
    for alias in aliases {
        assert!(config::update(&shared, |value| {
            value.hooks.file = Some(alias);
            value.ui.theme = "must not publish".into();
            value.prompt.system_prompt = Some("must not save".into());
            Ok(())
        })
        .is_err());
        assert_eq!(std::fs::read(&main).unwrap(), main_before);
        assert_eq!(std::fs::read(&prompt).unwrap(), prompt_before);
        assert!(shared.lock().unwrap().hooks.file.is_none());
        assert_eq!(
            shared.lock().unwrap().prompt.system_prompt.as_deref(),
            Some("original prompt")
        );
    }
}
