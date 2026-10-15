use super::*;
use crate::model::{self, configured::provider_client, EffortChoice};
use genai::{adapter::AdapterKind, chat::ChatOptions};
use serde_json::json;
fn config() -> Config {
    let config: Config = serde_json::from_value(json!({
        "models": {"model": "gateway/alias/with/slash",
        "providers": {
            "gateway": {"npm": "@ai-sdk/openai-compatible", "options": {"baseURL": "http://localhost:8080/v1", "apiKey": "test-only"},
                "models": {"alias/with/slash": {"id": "actual-api-model", "limit": {"context": 64000, "output": 16000}, "options": {"maxOutputTokens": 8000}, "variants": {"short": {"maxOutputTokens": 4000, "reasoningEffort": "high"}, "off": {"disabled": true}}}}},
            "unused": {"npm": "@ai-sdk/anthropic", "options": {"apiKey": "{env:YOURAI_UNUSED_PROVIDER_MISSING}"}, "models": {}}
        }},
        "security": {"yolo": true}
    })).unwrap();
    config.set_soul_file(Some("./SOUL.md".into()));
    config
}

#[test]
fn current_model_without_a_declared_entry_can_override_and_restore_effort() {
    let cfg = config();
    cfg.remove_model("gateway/alias/with/slash").unwrap();
    let id = cfg.model();
    model::select(&cfg, &id, None, Some(&EffortChoice::Set("high".into()))).unwrap();
    assert_eq!(
        model::list_models(&cfg, false)
            .into_iter()
            .find(|entry| entry.id == id && entry.variant.is_none())
            .and_then(|entry| entry.effort)
            .as_deref(),
        Some("high")
    );
    assert!(model::resolve(&cfg, &cfg.model(), cfg.variant().as_deref()).is_ok());
    model::select(&cfg, &id, None, Some(&EffortChoice::Config)).unwrap();
    assert_eq!(
        model::list_models(&cfg, false)
            .into_iter()
            .find(|entry| entry.id == id && entry.variant.is_none())
            .and_then(|entry| entry.effort),
        None
    );
    assert!(model::resolve(&cfg, &cfg.model(), cfg.variant().as_deref()).is_ok());
    assert!(model::select(&cfg, "gateway/unknown", Some("missing".into()), None).is_err());
    assert!(!cfg.model_ids().contains(&"gateway/unknown".into()));
}
#[test]
fn compaction_selection_is_optional_independent_and_validated() {
    let cfg = config();
    assert!(cfg
        .compaction_model()
        .map(|id| model::resolve(&cfg, &id, None))
        .transpose()
        .unwrap()
        .is_none());
    cfg.set_compaction_model(Some("gateway/alias/with/slash".into()));
    model::select(&cfg, "unused/main", None, None).unwrap();
    let summary = cfg
        .compaction_model()
        .map(|id| model::resolve(&cfg, &id, None))
        .transpose()
        .unwrap()
        .unwrap();
    assert_eq!(summary.model.model_iden(), "actual-api-model");
    assert_eq!(summary.model.token_budget().limits().context(), Some(64000));
    assert_eq!(summary.model.token_budget().max_output_tokens(), 8000);
    assert_eq!(summary.provider, "gateway");
    assert_eq!(cfg.model(), "unused/main");
    cfg.set_compaction_model(Some("missing/model".into()));
    assert!(cfg
        .compaction_model()
        .map(|id| model::resolve(&cfg, &id, None))
        .transpose()
        .is_err());
}
#[test]
fn config_effort_restores_original_values_after_repeated_overrides() {
    let cfg = config();
    let id = cfg.model();
    model::select(&cfg, &id, Some("short".into()), Some(&EffortChoice::Config)).unwrap();
    assert_eq!(
        model::list_models(&cfg, false)
            .into_iter()
            .find(|entry| entry.id == id && entry.variant.as_deref() == Some("short"))
            .and_then(|entry| entry.effort)
            .as_deref(),
        Some("high")
    );
    for effort in ["low", "medium"] {
        model::select(
            &cfg,
            &id,
            Some("short".into()),
            Some(&EffortChoice::Set(effort.into())),
        )
        .unwrap();
    }
    model::select(&cfg, &id, Some("short".into()), Some(&EffortChoice::Config)).unwrap();
    assert_eq!(
        model::list_models(&cfg, false)
            .into_iter()
            .find(|entry| entry.id == id && entry.variant.as_deref() == Some("short"))
            .and_then(|entry| entry.effort)
            .as_deref(),
        Some("high")
    );
    model::select(&cfg, &id, None, Some(&EffortChoice::Set("low".into()))).unwrap();
    cfg.set_variant(Some("short".into()));
    assert_eq!(
        model::list_models(&cfg, false)
            .into_iter()
            .find(|entry| entry.id == id && entry.variant.is_none())
            .and_then(|entry| entry.effort)
            .as_deref(),
        Some("low")
    );
    model::select(&cfg, &id, None, Some(&EffortChoice::Config)).unwrap();
    assert_eq!(
        model::list_models(&cfg, false)
            .into_iter()
            .find(|entry| entry.id == id && entry.variant.is_none())
            .and_then(|entry| entry.effort),
        None
    );
    assert_eq!(
        model::list_models(&cfg, false)
            .into_iter()
            .find(|entry| entry.id == id && entry.variant.as_deref() == Some("short"))
            .and_then(|entry| entry.effort)
            .as_deref(),
        Some("high")
    );
}
#[tokio::test]
async fn minimal_gateway_config_uses_model_name_and_default_prompt() {
    let cfg: Config = serde_json::from_value(json!({"model":"gateway/glm","providers":{"gateway":{"baseURL":"http://localhost:8080/v1","apiKey":"test-only"}}})).unwrap();
    assert!(cfg.system_prompt().is_none());
    assert_eq!(cfg.directory(), PathBuf::from(".yourai/sessions"));
    assert_eq!(
        model::resolve(&cfg, &cfg.model(), cfg.variant().as_deref())
            .unwrap()
            .model
            .model_iden(),
        "glm"
    );
    let provider: ProviderConfig =
        decode(cfg.values()["providers"]["gateway"].clone(), "provider").unwrap();
    let client = provider_client(&provider, "gateway", ChatOptions::default()).unwrap();
    let target = client
        .resolve_service_target(genai::ModelIden::new(AdapterKind::OpenAI, "glm"))
        .await
        .unwrap();
    assert_eq!(target.model.adapter_kind, AdapterKind::OpenAI);
    assert_eq!(target.endpoint.base_url(), "http://localhost:8080/v1/");
    let explicit: ProviderConfig =
        serde_json::from_value(json!({"api":"anthropic","apiKey":"test-only"})).unwrap();
    assert!(provider_client(&explicit, "custom", ChatOptions::default()).is_ok());
    assert!(serde_json::from_value::<ProviderConfig>(
        json!({"api":"anthropic","npm":"@ai-sdk/openai"})
    )
    .is_err());
}
#[test]
fn selects_model_and_variant_without_resolving_unused_credentials() {
    let cfg = config();
    let id = cfg.model();
    model::select(&cfg, &id, Some("short".into()), None).unwrap();
    let resolved = model::resolve(&cfg, &cfg.model(), cfg.variant().as_deref()).unwrap();
    assert_eq!(resolved.model.model_iden(), "actual-api-model");
    assert_eq!(
        resolved.model.token_budget().limits().context(),
        Some(64000)
    );
    assert_eq!(resolved.model.token_budget().max_output_tokens(), 4000);
    for variant in ["off", "missing"] {
        assert!(model::select(&cfg, &id, Some(variant.into()), None).is_err());
        cfg.set_variant(Some(variant.into()));
        assert!(model::resolve(&cfg, &cfg.model(), cfg.variant().as_deref()).is_err());
    }
    cfg.set_model("absent/model".into());
    assert!(model::resolve(&cfg, &cfg.model(), cfg.variant().as_deref()).is_err());
}
#[test]
fn validates_output_and_protects_runtime_fields() {
    for (key, value) in [("maxOutputTokens", json!(20000)), ("messages", json!([]))] {
        let cfg = config();
        cfg.set_model_option(&cfg.model(), key.into(), value)
            .unwrap();
        assert!(model::resolve(&cfg, &cfg.model(), cfg.variant().as_deref()).is_err());
    }
}
#[test]
fn json_example_and_prompt_fields_parse_and_unknown_fields_fail() {
    let cfg = config();
    assert!(cfg.yolo());
    assert_eq!(cfg.soul_file(), Some(PathBuf::from("./SOUL.md")));
    let mut example: Value =
        serde_json::from_str(include_str!("../../../../yourai.example.json")).unwrap();
    assert!(serde_json::from_value::<Config>(example.clone()).is_ok());
    example["typo"] = json!(true);
    assert!(serde_json::from_value::<Config>(example).is_err());
}
#[test]
fn config_directory_uses_absolute_xdg_or_home() {
    for (xdg, home, expected) in [
        (Some("/tmp/xdg"), None, Some("/tmp/xdg")),
        (Some("rel"), Some("/home/u"), Some("/home/u/.config")),
        (Some(""), Some("/home/u"), Some("/home/u/.config")),
        (None, Some("/home/u"), Some("/home/u/.config")),
        (None, None, None),
    ] {
        let result = config_dir(xdg.map(Path::new), home.map(Path::new));
        match expected {
            Some(path) => assert_eq!(result.unwrap(), PathBuf::from(path)),
            None => assert!(result.is_err()),
        }
    }
}

#[test]
fn legacy_model_details_migrate_once_without_losing_provider_or_price_values() {
    let old = json!({
        "models":{"model":"gateway/a","providers":{"gateway":{
            "options":{"baseURL":"http://localhost/v1","requests":{"rpm":7,"cooldown_seconds":9}},
            "models":{"a":{"pricing":{"input":1.25,"output":2.5},"variants":{"fast":{"temperature":0.3}}}}
        }}},"ui":{"theme":"nord"},"security":{"yolo":true},"session":{"extensions":true}
    });
    let cfg: Config = serde_json::from_value(old).unwrap();
    assert_eq!(cfg.provider_rpm("gateway").unwrap(), Some(7));
    assert_eq!(cfg.provider_cooldown_seconds("gateway").unwrap(), 9);
    assert_eq!(cfg.model_input_price("gateway/a").unwrap(), Some(1.25));
    assert_eq!(cfg.model_output_price("gateway/a").unwrap(), Some(2.5));
    assert_eq!(
        model::resolve(&cfg, &cfg.model(), cfg.variant().as_deref())
            .unwrap()
            .requests,
        (Some(7), 9)
    );
    let canonical = serde_json::to_value(&cfg).unwrap();
    assert_eq!(canonical["model"], "gateway/a");
    assert_eq!(canonical["models"]["gateway/a"]["provider"], "gateway");
    assert!(canonical["providers"]["gateway"].get("options").is_none());
    for wrapper in ["ui", "security", "session"] {
        assert!(canonical.get(wrapper).is_none());
    }
    let again: Config = serde_json::from_value(canonical.clone()).unwrap();
    assert_eq!(serde_json::to_value(again).unwrap(), canonical);
}
#[test]
fn migration_rejects_unknown_or_conflicting_provider_fields() {
    for value in [
        json!({"providers":{"gateway":{"options":{"api":"anthropic"}}}}),
        json!({"providers":{"gateway":{"baseURL":"http://a/v1","options":{"baseURL":"http://b/v1"}}}}),
        json!({"providers":{"gateway":{"options":{"requests":{"name":"oops"}}}}}),
        json!({"providers":{"gateway":{"models":{"a":{"provider":"other"}}}}}),
        json!({"providers":{"gateway":{"models":{"a":{}}}},"models":{"gateway/a":{}}}),
    ] {
        assert!(serde_json::from_value::<Config>(value).is_err());
    }
}

#[test]
fn flat_model_fields_migrate_and_save_without_limit_wrapper() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({
            "model": "mock/a",
            "providers": {"mock": {}},
            "models": {"mock/a": {"limit": {"context": 64000, "input": 48000, "output": 8000}}}
        }))
        .unwrap(),
    )
    .unwrap();
    let config = Config::init(&path).unwrap();
    config.set_model_header_timeout("mock/a", Some(25)).unwrap();
    config.set_model_chunk_timeout("mock/a", Some(40)).unwrap();
    config.save().unwrap();
    let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let entry = &saved["models"]["mock/a"];
    assert!(entry.get("limit").is_none());
    assert_eq!(entry["context"], 64000);
    assert_eq!(entry["input"], 48000);
    assert_eq!(entry["output"], 8000);
    assert_eq!(entry["header_timeout"], 25);
    assert_eq!(entry["chunk_timeout"], 40);
    let again = Config::init(&path).unwrap();
    assert_eq!(serde_json::to_value(&again).unwrap(), saved);
    let model = model::resolve(&again, "mock/a", None).unwrap().model;
    assert_eq!(model.token_budget().limits().context(), Some(64000));
    assert_eq!(model.token_budget().limits().input(), Some(48000));
    assert_eq!(model.token_budget().max_output_tokens(), 8000);
    for invalid in [
        json!({"limit":{"context":64000},"context":32000}),
        json!({"limit":{"typo":1}}),
        json!({"limit":null}),
    ] {
        assert!(serde_json::from_value::<Config>(json!({"models":{"mock/a":invalid}})).is_err());
    }
}

#[test]
fn model_timeouts_override_provider_defaults_and_fall_back_when_unset() {
    let cfg = config();
    let id = cfg.model();
    cfg.set_provider_header_timeout("gateway", Some(500))
        .unwrap();
    cfg.set_provider_chunk_timeout("gateway", Some(700))
        .unwrap();
    cfg.set_model_header_timeout(&id, Some(25)).unwrap();
    cfg.set_model_chunk_timeout(&id, Some(40)).unwrap();
    let model = model::resolve(&cfg, &id, None).unwrap().model;
    assert_eq!(model.timeouts().headers(), Duration::from_millis(25));
    assert_eq!(model.timeouts().read(), Duration::from_millis(40));
    cfg.set_model_header_timeout(&id, None).unwrap();
    let model = model::resolve(&cfg, &id, None).unwrap().model;
    assert_eq!(model.timeouts().headers(), Duration::from_millis(500));
    assert_eq!(model.timeouts().read(), Duration::from_millis(40));
    cfg.set_model_chunk_timeout(&id, None).unwrap();
    assert_eq!(
        model::resolve(&cfg, &id, None)
            .unwrap()
            .model
            .timeouts()
            .read(),
        Duration::from_millis(700)
    );
    cfg.set_model_chunk_timeout(&id, Some(0)).unwrap();
    assert!(model::resolve(&cfg, &id, None).is_err());
}

#[test]
fn list_metadata_and_explicit_resolution_share_effort_without_changing_selection() {
    let cfg = config();
    let id = cfg.model();
    cfg.set_model_input_price(&id, Some(1.25)).unwrap();
    cfg.set_model_output_price(&id, Some(2.5)).unwrap();
    model::select(
        &cfg,
        &id,
        Some("short".into()),
        Some(&EffortChoice::Set("low".into())),
    )
    .unwrap();
    model::select(&cfg, "unused/main", None, None).unwrap();
    let before = serde_json::to_value(&cfg).unwrap();
    let entry = model::list_models(&cfg, true)
        .into_iter()
        .find(|entry| entry.id == id && entry.variant.as_deref() == Some("short"))
        .unwrap();
    assert_eq!(entry.effort.as_deref(), Some("low"));
    assert_eq!(entry.input_price.zip(entry.output_price), Some((1.25, 2.5)));
    let resolved = model::resolve(&cfg, &id, Some("short")).unwrap();
    assert_eq!(resolved.model.model_iden(), "actual-api-model");
    assert_eq!(resolved.model.token_budget().max_output_tokens(), 4000);
    assert_eq!(serde_json::to_value(&cfg).unwrap(), before);
}
