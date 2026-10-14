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
    use yourai_harness::config::Config;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("yourai.json");
    std::fs::write(
        &path,
        r#"{"model":"mock/a","steps":5,"max_model_retries":2,"provider":{}}"#,
    )
    .unwrap();
    let shared = Arc::new(Config::init(&path).unwrap());
    let seen = Arc::new(Mutex::new(vec![]));
    let mut options = SessionOptions::new(dir.path().join("sessions"), dir.path().into());
    options.config.set_system_prompt(Some("test".into()));
    options.agent_loop = Some(Arc::new(Probe(seen.clone())));
    options.config = Arc::clone(&shared);
    let h = Harness::open(options.clone(), Arc::new(support::Model::new(vec![])))
        .await
        .unwrap();
    let source = Arc::clone(&shared);
    h.host
        .bind_config_source(Arc::new(move || {
            source.execution_options().resolve(&TurnLimits::default())
        }))
        .unwrap();
    let mut limits = TurnLimits::default();
    limits.steps = Some(3);
    run(&h, limits).await;
    {
        let config = &shared;
        config.set_memory_search_limit(7);
        config.save()
    }
    .unwrap();
    run(&h, TurnLimits::default()).await;
    let configs = seen.lock().unwrap().clone();
    assert_eq!(configs[0].steps(), Some(3));
    assert_eq!(configs[0].max_model_retries(), 2);
    assert_eq!(configs[0].memory_search_limit(), 0);
    assert_eq!(configs[1].steps(), Some(5));
    assert_eq!(configs[1].memory_search_limit(), 7);
    assert!(h
        .host
        .store()
        .read_state(&h.host.environment().id, "config")
        .unwrap()
        .is_none());
    options.resume = Some(h.host.environment().id);
    h.close().await.unwrap();
    options.config = Arc::new(Config::init(&path).unwrap());
    let resumed = Harness::open(options, Arc::new(support::Model::new(vec![])))
        .await
        .unwrap();
    assert_eq!(resumed.host.config().memory_search_limit(), 7);
    resumed.close().await.unwrap();
}

#[tokio::test]
async fn child_inherits_execution_limits_without_activating_parent_skills() {
    let dir = tempfile::tempdir().unwrap();
    let options = SessionOptions::new(dir.path().join("sessions"), dir.path().into());
    options.config.set_system_prompt(Some("test".into()));
    options.config.set_extensions(true);
    options.config.set_steps(Some(1));
    options.config.set_skill_ids(vec!["parent-only".into()]);
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
    assert_eq!(*h.host.config().skill_ids(), vec!["parent-only"]);
    h.close().await.unwrap();
}

#[tokio::test]
async fn title_backfill_failure_does_not_prevent_resume() {
    let dir = tempfile::tempdir().unwrap();
    let mut options = SessionOptions::new(dir.path().join("sessions"), dir.path().into());
    options.config.set_system_prompt(Some("test".into()));
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

    let settings = {
        let mut settings: ExecutionOptions = Default::default();
        settings.set_steps(Some(3));
        settings
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
    assert_eq!(seen.lock().unwrap()[0].steps(), None);
    assert_eq!(seen.lock().unwrap()[1].steps(), Some(3));
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

fn config_files(directory: &std::path::Path) -> Arc<yourai_harness::config::Config> {
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
    Arc::new(yourai_harness::config::Config::init(&path).unwrap())
}

#[test]
fn split_files_migrate_once_and_main_json_becomes_authoritative() {
    use yourai_harness::config::Config;
    let dir = tempfile::tempdir().unwrap();
    let shared = config_files(dir.path());
    let legacy = dir.path().join("prompts.json");
    let legacy_before = std::fs::read(&legacy).unwrap();
    {
        let config = &shared;
        config.set_yolo(true);
        config.set_theme("nord".into());
        config.save()
    }
    .unwrap();
    let main = dir.path().join("yourai.json");
    let mut saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&main).unwrap()).unwrap();
    assert_eq!(saved["yolo"], true);
    assert_eq!(saved["prompt"]["system_prompt"], "original prompt");
    assert_eq!(std::fs::read(&legacy).unwrap(), legacy_before);
    saved["theme"] = "light".into();
    saved["prompt"]["system_prompt"] = "external main edit".into();
    std::fs::write(&main, serde_json::to_vec(&saved).unwrap()).unwrap();
    // Old split data is no longer read once prompt is in the main JSON.
    std::fs::write(&legacy, "corrupt obsolete file").unwrap();
    assert_eq!(shared.theme(), "nord");
    let reloaded = Config::init(&main).unwrap();
    assert_eq!(reloaded.theme(), "light");
    assert_eq!(
        reloaded.system_prompt().as_deref(),
        Some("external main edit")
    );
    {
        let config = &shared;
        config.set_system_prompt(Some("edited in app".into()));
        config.save()
    }
    .unwrap();
    assert_eq!(
        Config::init(&main).unwrap().system_prompt().as_deref(),
        Some("edited in app")
    );
    assert_eq!(
        std::fs::read_to_string(&legacy).unwrap(),
        "corrupt obsolete file"
    );
}

#[test]
fn legacy_inline_prompts_save_in_the_same_json_without_split_files() {
    use yourai_harness::config::Config;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("yourai.json");
    std::fs::write(&path, r#"{"model":"mock/a","provider":{},"system_prompt":"legacy prompt","append_system_prompt":"additional","profile_file":"profile.md"}"#).unwrap();
    let config = Config::init(&path).unwrap();
    config.set_theme("nord".into());
    config.save().unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(saved.get("system_prompt").is_none());
    assert_eq!(saved["prompt"]["system_prompt"], "legacy prompt");
    assert_eq!(saved["prompt"]["append_system_prompt"], "additional");
    assert_eq!(saved["prompt"]["profile_file"], "profile.md");
    assert!(!dir.path().join("prompts.json").exists());
    assert!(!dir.path().join("hooks.json").exists());
    assert_eq!(
        Config::init(&path).unwrap().system_prompt().as_deref(),
        Some("legacy prompt")
    );
}

#[test]
fn failed_save_keeps_local_edits_and_leaves_legacy_files_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let shared = config_files(dir.path());
    let path = dir.path().join("yourai.json");
    let before = serde_json::to_value(&shared).unwrap();
    let legacy_before = std::fs::read(dir.path().join("prompts.json")).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!({
        let config = &shared;
        config.set_yolo(true);
        config.set_theme("nord".into());
        config.set_system_prompt(Some("local edit".into()));
        config.save()
    }
    .is_err());
    assert_ne!(serde_json::to_value(&shared).unwrap(), before);
    assert_eq!(
        std::fs::read(dir.path().join("prompts.json")).unwrap(),
        legacy_before
    );
    assert!(path.is_dir());
}

#[tokio::test]
async fn model_switch_saves_global_choices_and_file_failure_keeps_live_bindings() {
    use yourai_harness::{config::Config, controller, model::EffortChoice};
    let dir = tempfile::tempdir().unwrap();
    let initial = config_files(dir.path());
    initial.set_soul_file(None);
    let mut options = SessionOptions::new(dir.path().join("sessions"), dir.path().into());
    options.config = Arc::clone(&initial);
    let model = yourai_harness::model::resolve(&initial).unwrap().model;
    let h = Harness::open(options.clone(), model).await.unwrap();
    let shared = initial;
    let (backend, mut events) = controller::SessionController::new(
        h,
        Arc::clone(&shared),
        options.clone(),
        TurnLimits::default(),
    );
    let h = backend.harness();
    async fn model_result(
        events: &mut controller::SessionSubscription,
    ) -> Result<controller::ModelSelection, String> {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match events.recv().await {
                    Some(controller::SessionEvent::Model(result)) => return result,
                    Some(_) => {}
                    None => panic!("backend closed before model result"),
                }
            }
        })
        .await
        .unwrap()
    }
    let path = dir.path().join("yourai.json");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    backend
        .client()
        .model("mock/b".into(), Some("short".into()), None)
        .unwrap();
    assert!(model_result(&mut events).await.is_err());
    assert_eq!(shared.model(), "mock/a");
    assert_eq!(h.host.agent().model().unwrap().model_iden(), "a");
    std::fs::remove_dir(&path).unwrap();
    backend
        .client()
        .model(
            "mock/b".into(),
            Some("short".into()),
            Some(EffortChoice::Set("high".into())),
        )
        .unwrap();
    model_result(&mut events).await.unwrap();
    assert_eq!(h.host.agent().model().unwrap().model_iden(), "b");
    let reloaded = Config::init(&path).unwrap();
    assert_eq!(reloaded.model(), "mock/b");
    assert_eq!(reloaded.variant().as_deref(), Some("short"));
    assert_eq!(
        yourai_harness::model::effective_effort(&reloaded, "mock/b", Some("short")).as_deref(),
        Some("high")
    );
    assert!(h
        .host
        .store()
        .read_state(&h.host.environment().id, "model_preferences")
        .unwrap()
        .is_none());
    // Resuming a different session never restores a competing model preference.
    let loaded = Arc::new(reloaded);
    let (_, model) = yourai_harness::assembly::resolve_session(
        &loaded.clone(),
        &options,
        Some(h.host.environment().id),
    )
    .unwrap();
    assert_eq!(model.model_iden(), "b");
    backend.close().await.unwrap();
}

#[test]
fn legacy_external_hooks_migrate_without_modifying_the_backup() {
    use yourai_harness::config::{Config, HookSettings};
    let dir = tempfile::tempdir().unwrap();
    config_files(dir.path());
    let main = dir.path().join("yourai.json");
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&main).unwrap()).unwrap();
    let hooks = serde_json::json!({"PreToolUse":[{"hooks":[{"type":"command","command":"echo original"}]}]});
    let external = dir.path().join("hooks.json");
    std::fs::write(
        &external,
        serde_json::to_vec(&serde_json::json!({"hooks":hooks})).unwrap(),
    )
    .unwrap();
    document["hooks"] = serde_json::json!({"file":"hooks.json"});
    std::fs::write(&main, serde_json::to_vec(&document).unwrap()).unwrap();
    let config = Config::init(&main).unwrap();
    let backup = std::fs::read(&external).unwrap();
    assert_eq!(config.events(), hooks);
    config.set_theme("nord".into());
    config.save().unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&main).unwrap()).unwrap();
    assert_eq!(saved["hooks"]["events"], hooks);
    assert!(saved["hooks"].get("file").is_none());
    assert_eq!(std::fs::read(&external).unwrap(), backup);
    std::fs::remove_file(&external).unwrap();
    std::fs::create_dir(&external).unwrap();
    assert_eq!(Config::init(&main).unwrap().events(), hooks);
    config.set_hooks({
        let mut settings = HookSettings::default();
        settings.set_events(serde_json::json!({}));
        settings
    });
    config.save().unwrap();
    assert_eq!(Config::init(&main).unwrap().events(), serde_json::json!({}));
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
    assert!(Config::init(&main).is_err());
    for invalid in [
        "{",
        "{}",
        r#"{"hook":{}}"#,
        r#"{"hooks":[]}"#,
        r#"{"hooks":{},"extra":true}"#,
        r#"{"hooks":{"events":{},"file":"other.json"}}"#,
    ] {
        std::fs::write(dir.path().join("hooks.json"), invalid).unwrap();
        assert!(Config::init(&main)
            .and_then(|config| {
                yourai_harness::assembly::validate_config(&config)?;
                Ok(config)
            })
            .is_err());
    }
    std::fs::write(dir.path().join("hooks.json"), r#"{"hooks":{"events":{}}}"#).unwrap();
    assert_eq!(Config::init(&main).unwrap().events(), serde_json::json!({}));
    document["hooks"] = serde_json::json!({"Stop":[]});
    std::fs::write(&main, serde_json::to_vec(&document).unwrap()).unwrap();
    assert!(Config::init(&main).is_err());
}

#[test]
fn one_save_writes_every_category_to_one_json_without_reloading() {
    use yourai_harness::config::{Config, HookSettings};
    let dir = tempfile::tempdir().unwrap();
    let shared = config_files(dir.path());
    let main = dir.path().join("yourai.json");
    let legacy = dir.path().join("prompts.json");
    std::fs::write(&main, "invalid external edit").unwrap();
    std::fs::write(&legacy, "obsolete external edit").unwrap();
    {
        let config = &shared;
        config.set_model("mock/b".into());
        config.set_variant(Some("short".into()));
        config.set_steps(Some(3));
        config.set_yolo(true);
        config.set_trusted_shell(true);
        config.set_theme("nord".into());
        config.set_directory("custom-sessions".into());
        config.set_extensions(true);
        config.set_system_prompt(Some("cached prompt".into()));
        config.set_hooks({
            let mut settings = HookSettings::default();
            settings.set_events(serde_json :: json ! ({ "Stop" : [] }));
            settings
        });
        config.save()
    }
    .unwrap();
    let reloaded = Config::init(&main).unwrap();
    assert_eq!(reloaded.model(), "mock/b");
    assert_eq!(reloaded.variant().as_deref(), Some("short"));
    assert_eq!(reloaded.steps(), Some(3));
    assert!(reloaded.yolo() && reloaded.trusted_shell());
    assert_eq!(reloaded.theme(), "nord");
    assert_eq!(
        *reloaded.directory(),
        std::path::PathBuf::from("custom-sessions")
    );
    assert!(reloaded.extensions());
    assert_eq!(reloaded.system_prompt().as_deref(), Some("cached prompt"));
    assert_eq!(reloaded.events(), serde_json::json!({"Stop":[]}));
    assert_eq!(
        std::fs::read_to_string(&legacy).unwrap(),
        "obsolete external edit"
    );
    assert!(!dir.path().join("hooks.json").exists());
}

#[test]
fn grouped_configuration_roundtrips_and_rejects_ambiguous_or_unknown_fields() {
    use yourai_harness::config::Config;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("yourai.json");
    let document = serde_json::json!({
        "models":{"model":"mock/a", "providers":{}},
        "execution":{"steps":4,"context":{"summary_tokens":1234},"attachment_image":{"max_width":1200}},
        "security":{"yolo":false},"ui":{"theme":"nord"},
        "session":{"directory":"sessions","extensions":true},
        "prompt":{"system_prompt":"inline grouped prompt"},"hooks":{"events":{}}
    });
    std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    let shared = Arc::new(Config::init(&path).unwrap());
    assert_eq!(shared.steps(), Some(4));
    assert_eq!(shared.attachment_image().max_width(), 1200);
    {
        let value = &shared;
        value.set_yolo(true);
        value.save()
    }
    .unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["execution"]["context"]["summary_tokens"], 1234);
    assert_eq!(saved["model"], "mock/a");
    assert!(saved["models"].get("model").is_none());
    for wrapper in ["ui", "security", "session"] {
        assert!(saved.get(wrapper).is_none());
    }
    assert!(saved.get("steps").is_none());
    assert_eq!(saved["prompt"]["system_prompt"], "inline grouped prompt");
    assert_eq!(
        Config::init(&path).unwrap().system_prompt().as_deref(),
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
        ("model", "models", "model"),
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
fn legacy_hook_references_cannot_recursively_read_the_main_config() {
    use yourai_harness::config::Config;
    let dir = tempfile::tempdir().unwrap();
    config_files(dir.path());
    let main = dir.path().join("yourai.json");
    std::fs::create_dir(dir.path().join("nested")).unwrap();
    let mut aliases = vec![
        std::path::PathBuf::from("yourai.json"),
        "./yourai.json".into(),
        "nested/../yourai.json".into(),
        main.clone(),
    ];
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&main, dir.path().join("main-link.json")).unwrap();
        aliases.push("main-link.json".into());
    }
    for alias in aliases {
        let document =
            serde_json::json!({"models":{"model":"mock/a","providers":{}}, "hooks":{"file":alias}});
        std::fs::write(&main, serde_json::to_vec(&document).unwrap()).unwrap();
        let before = std::fs::read(&main).unwrap();
        assert!(Config::init(&main).is_err());
        assert_eq!(std::fs::read(&main).unwrap(), before);
    }
}

#[test]
fn selecting_a_model_updates_memory_without_implicitly_saving() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_files(dir.path());
    let path = dir.path().join("yourai.json");
    let before = std::fs::read(&path).unwrap();
    yourai_harness::model::select(&config, "mock/b", Some("short".into()), None).unwrap();
    assert_eq!(config.model(), "mock/b");
    assert_eq!(config.variant().as_deref(), Some("short"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn invalid_model_choice_does_not_partially_modify_configuration() {
    use yourai_harness::model::EffortChoice;
    let dir = tempfile::tempdir().unwrap();
    let config = config_files(dir.path());
    let before = serde_json::to_value(&config).unwrap();
    for (id, variant, effort) in [
        (
            "mock/b",
            Some("short".into()),
            Some(EffortChoice::Set("invalid".into())),
        ),
        ("mock/b", Some("missing".into()), None),
        ("mock/", None, None),
    ] {
        assert!(yourai_harness::model::select(&config, id, variant, effort.as_ref()).is_err());
        assert_eq!(serde_json::to_value(&config).unwrap(), before);
    }
}

#[test]
fn explicit_validation_reports_invalid_edits_without_saving() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_files(dir.path());
    let main = dir.path().join("yourai.json");
    let before = std::fs::read(&main).unwrap();
    config.set_steps(Some(0));
    config.set_theme("nord".into());
    assert!(yourai_harness::assembly::validate_config(&config).is_err());
    assert_eq!(config.steps(), Some(0));
    assert_eq!(config.theme(), "nord");
    assert_eq!(std::fs::read(&main).unwrap(), before);
}

#[test]
fn json_loading_and_saving_do_not_apply_business_rules() {
    use yourai_harness::config::Config;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("yourai.json");
    std::fs::write(
        &path,
        r#"{
        "models":{"model":"not-a-provider-selection","providers":{}},
        "execution":{"steps":0},
        "hooks":{"events":{"UnknownEvent":[]}}
    }"#,
    )
    .unwrap();
    let config = Config::init(&path).unwrap();
    assert_eq!(config.steps(), Some(0));
    config.set_theme("nord".into());
    config.save().unwrap();
    let reloaded = Config::init(&path).unwrap();
    assert_eq!(reloaded.theme(), "nord");
    assert_eq!(reloaded.model(), "not-a-provider-selection");
    assert_eq!(reloaded.steps(), Some(0));
    assert_eq!(reloaded.events(), serde_json::json!({"UnknownEvent":[]}));
    assert!(yourai_harness::assembly::validate_config(&reloaded).is_err());
}

#[tokio::test]
async fn model_switch_preserves_unrelated_edits_and_rejects_stale_resources() {
    use yourai_harness::controller;
    struct Gate {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    impl HookHandler for Gate {
        fn execute<'a>(
            &'a self,
            _: &'a HookInvocation,
        ) -> BoxFuture<'a, Result<HookOutput, YourAiError>> {
            Box::pin(async move {
                self.entered.notify_one();
                self.release.notified().await;
                Ok(HookOutput::Parsed(serde_json::json!({})))
            })
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let initial = config_files(dir.path());
    initial.set_soul_file(None);
    let mut options = SessionOptions::new(dir.path().join("sessions"), dir.path().into());
    options.config = Arc::clone(&initial);
    let h = Harness::open(
        options.clone(),
        yourai_harness::model::resolve(&initial).unwrap().model,
    )
    .await
    .unwrap();
    let gate = Arc::new(Gate {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    h.hooks
        .register(NativeHookRegistration {
            id: "config-gate".into(),
            event: HookEventKind::ConfigChange,
            matcher: None,
            handler: gate.clone(),
            timeout: Some(std::time::Duration::from_secs(10)),
            source: HookSource::Session,
            failure_policy: FailurePolicy::Closed,
            once: false,
        })
        .await
        .unwrap();
    let shared = initial;
    let (backend, mut events) =
        controller::SessionController::new(h, Arc::clone(&shared), options, TurnLimits::default());
    for stale in [false, true] {
        backend
            .client()
            .model(if stale { "mock/a" } else { "mock/b" }.into(), None, None)
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), gate.entered.notified())
            .await
            .unwrap();
        {
            let config = &shared;
            if stale {
                config.set_context_safety_margin(config.context_safety_margin() + 1);
            } else {
                config.set_theme("nord".into());
            }
            config.save()
        }
        .unwrap();
        gate.release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Some(controller::SessionEvent::Model(result)) = events.recv().await {
                    break result;
                }
            }
        })
        .await
        .unwrap();
        if stale {
            assert!(result.unwrap_err().contains("changed while preparing"));
        } else {
            assert_eq!(result.unwrap().label, "mock/b");
        }
        assert_eq!(shared.model(), "mock/b");
        assert_eq!(
            backend.harness().host.agent().model().unwrap().model_iden(),
            "b"
        );
        let saved = yourai_harness::config::Config::init(&dir.path().join("yourai.json")).unwrap();
        assert_eq!(saved.model(), "mock/b");
        assert_eq!(saved.theme(), "nord");
    }
    backend.close().await.unwrap();
}

#[test]
fn legacy_selected_loads_but_saves_as_model_and_rejects_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("yourai.json");
    std::fs::write(&path, r#"{"models":{"selected":"mock/a"}}"#).unwrap();
    let config = yourai_harness::config::Config::init(&path).unwrap();
    assert_eq!(config.model(), "mock/a");
    config.save().unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["model"], "mock/a");
    assert!(saved.get("selected").is_none());
    for document in [
        r#"{"models":{"model":"mock/a","selected":"mock/b"}}"#,
        r#"{"model":"mock/a","models":{"selected":"mock/b"}}"#,
    ] {
        std::fs::write(&path, document).unwrap();
        assert!(yourai_harness::config::Config::init(&path).is_err());
    }
}

#[test]
fn direct_setters_update_shared_data_and_only_save_when_requested() {
    use yourai_harness::config::Config;
    let dir = tempfile::tempdir().unwrap();
    let config = config_files(dir.path());
    let path = dir.path().join("yourai.json");
    let before = std::fs::read(&path).unwrap();
    config.set_model("mock/b".into());
    config
        .set_provider_base_url("mock", Some("http://localhost:8081/v1".into()))
        .unwrap();
    config.set_steps(Some(7));
    config.set_context_summary_tokens(1500);
    config.set_yolo(true);
    config.set_theme("nord".into());
    config.set_extensions(true);
    config.set_system_prompt(Some("local prompt".into()));
    config.set_events(serde_json::json!({"SessionStart":[]}));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(config.model(), "mock/b");
    assert_eq!(config.steps(), Some(7));
    config.save().unwrap();
    let saved = Config::init(&path).unwrap();
    assert_eq!(
        serde_json::to_value(&config).unwrap(),
        serde_json::to_value(saved).unwrap()
    );
}

#[test]
fn arc_config_serializes_concurrent_setters() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_files(dir.path());
    let workers: Vec<_> = (0..3)
        .map(|worker| {
            let config = Arc::clone(&config);
            std::thread::spawn(move || {
                for step in 1..=20 {
                    match worker {
                        0 => config.set_theme(format!("theme-{step}")),
                        1 => config.set_steps(Some(step)),
                        _ => config.set_model(format!("mock/model-{step}")),
                    }
                    config.save().unwrap();
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(config.theme(), "theme-20");
    assert_eq!(config.steps(), Some(20));
    assert_eq!(config.model(), "mock/model-20");
    let saved = yourai_harness::config::Config::init(&dir.path().join("yourai.json")).unwrap();
    assert_eq!(
        serde_json::to_value(config.as_ref()).unwrap(),
        serde_json::to_value(saved).unwrap()
    );
}

#[test]
fn indexed_setters_modify_shared_data_and_reject_missing_entries() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_files(dir.path());
    config
        .set_provider_base_url("mock", Some("http://localhost:8081/v1".into()))
        .unwrap();
    config.set_model_context("mock/a", Some(12345)).unwrap();
    assert_eq!(
        config.provider_base_url("mock").unwrap().as_deref(),
        Some("http://localhost:8081/v1")
    );
    assert_eq!(config.model_context("mock/a").unwrap(), Some(12345));
    config.save().unwrap();
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("yourai.json")).unwrap()).unwrap();
    assert_eq!(json["models"]["mock/a"]["limit"]["context"], 12345);
    config.remove_provider("mock").unwrap();
    assert!(config
        .set_provider_api_key("mock", Some("removed".into()))
        .is_err());
    assert!(config.provider_api_key("mock").is_err());
    assert!(config.set_model_context("missing/a", Some(123)).is_err());
}

#[test]
fn reading_a_field_never_keeps_the_global_lock() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_files(dir.path());
    let old_theme = config.theme();
    let (tx, rx) = std::sync::mpsc::channel();
    let writer = Arc::clone(&config);
    let task = std::thread::spawn(move || {
        writer.set_theme("nord".into());
        writer.set_steps(Some(20));
        writer.save().unwrap();
        tx.send(()).unwrap();
    });
    rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    task.join().unwrap();
    assert_ne!(old_theme, config.theme());
    assert_eq!(config.theme(), "nord");
    assert_eq!(config.steps(), Some(20));
}

#[test]
fn session_options_clone_shares_the_same_config() {
    let options = SessionOptions::new("sessions".into(), ".".into());
    let other = options.clone();
    assert!(Arc::ptr_eq(&options.config, &other.config));
    other.config.set_theme("nord".into());
    assert_eq!(options.config.theme(), "nord");
}

#[test]
fn concurrent_nested_setters_preserve_other_fields() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_files(dir.path());
    let tasks: Vec<_> = (0..3)
        .map(|worker| {
            let config = Arc::clone(&config);
            std::thread::spawn(move || {
                for step in 0..20 {
                    config
                        .set_provider_header("mock", format!("worker-{worker}"), step.to_string())
                        .unwrap();
                }
            })
        })
        .collect();
    for task in tasks {
        task.join().unwrap();
    }
    config.save().unwrap();
    let headers = config.provider_headers("mock").unwrap();
    for worker in 0..3 {
        assert_eq!(headers[&format!("worker-{worker}")], "19");
    }
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("yourai.json")).unwrap()).unwrap();
    assert_eq!(json["providers"]["mock"]["headers"]["worker-2"], "19");
}

#[tokio::test]
async fn opening_a_session_uses_current_defaults_without_overwriting_them() {
    use yourai_harness::controller::{SessionController, SessionEvent};
    let dir = tempfile::tempdir().unwrap();
    let config = config_files(dir.path());
    config.set_soul_file(None);
    config.set_yolo(true);
    let mut options = SessionOptions::new(dir.path().join("sessions"), dir.path().into());
    options.config = Arc::clone(&config);
    let harness = Harness::open(
        options.clone(),
        yourai_harness::model::resolve(&config).unwrap().model,
    )
    .await
    .unwrap();
    assert!(harness.yolo());
    let (backend, mut events) =
        SessionController::new(harness, Arc::clone(&config), options, TurnLimits::default());
    config.set_yolo(false);
    config.save().unwrap();
    backend.client().switch(None).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some(SessionEvent::Opened(result)) = events.recv().await {
                result.unwrap();
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(!config.yolo());
    assert!(!backend.harness().yolo());
    assert!(
        !yourai_harness::config::Config::init(&dir.path().join("yourai.json"))
            .unwrap()
            .yolo()
    );
    backend.close().await.unwrap();
}

#[test]
fn model_menu_projects_one_consistent_configuration_version() {
    use yourai_harness::config::{Config, ModelConfig, ProviderConfig};
    let config = Arc::new(Config::default());
    config.set_provider("mock".into(), ProviderConfig::default());
    config.set_model("mock/main".into());
    let versions: Vec<ModelConfig> = ["a", "b"]
        .into_iter()
        .map(|id| {
            serde_json::from_value(serde_json::json!({
                "provider":"mock","variants":{
                    format!("{id}-fast"):{"reasoningEffort":"high"},
                    format!("{id}-slow"):{"reasoningEffort":"low"}
                }
            }))
            .unwrap()
        })
        .collect();
    config
        .set_model_config("mock/main".into(), versions[0].clone())
        .unwrap();
    let writer = Arc::clone(&config);
    let task = std::thread::spawn(move || {
        for iteration in 0..1000 {
            writer
                .set_model_config("mock/main".into(), versions[iteration % 2].clone())
                .unwrap();
        }
    });
    for _ in 0..1000 {
        let choices = yourai_harness::model::choices(&config);
        assert_eq!(choices.len(), 3);
        assert!(choices.iter().all(|choice| choice.id == "mock/main"));
        let prefix = choices[1].variant.as_ref().unwrap().chars().next().unwrap();
        assert_eq!(
            choices[2].variant.as_ref().unwrap().chars().next().unwrap(),
            prefix
        );
        assert_eq!(choices[1].effort.as_deref(), Some("high"));
        assert_eq!(choices[2].effort.as_deref(), Some("low"));
    }
    task.join().unwrap();
}
