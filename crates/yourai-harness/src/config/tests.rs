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
        model::effective_effort(&cfg, &id, None).as_deref(),
        Some("high")
    );
    assert!(model::resolve(&cfg).is_ok());
    model::select(&cfg, &id, None, Some(&EffortChoice::Config)).unwrap();
    assert_eq!(model::effective_effort(&cfg, &id, None), None);
    assert!(model::resolve(&cfg).is_ok());
    assert!(model::select(&cfg, "gateway/unknown", Some("missing".into()), None).is_err());
    assert!(!cfg.model_ids().contains(&"gateway/unknown".into()));
}
#[test]
fn compaction_selection_is_optional_independent_and_validated() {
    let cfg = config();
    assert!(model::resolve_compaction(&cfg).unwrap().is_none());
    cfg.set_compaction_model(Some("gateway/alias/with/slash".into()));
    model::select(&cfg, "unused/main", None, None).unwrap();
    let summary = model::resolve_compaction(&cfg).unwrap().unwrap();
    assert_eq!(summary.model.model_iden(), "actual-api-model");
    assert_eq!(summary.model.token_budget().limits().context(), Some(64000));
    assert_eq!(summary.model.token_budget().max_output_tokens(), 8000);
    assert_eq!(summary.provider, "gateway");
    assert_eq!(cfg.model(), "unused/main");
    cfg.set_compaction_model(Some("missing/model".into()));
    assert!(model::resolve_compaction(&cfg).is_err());
}
#[test]
fn config_effort_restores_original_values_after_repeated_overrides() {
    let cfg = config();
    let id = cfg.model();
    model::select(&cfg, &id, Some("short".into()), Some(&EffortChoice::Config)).unwrap();
    assert_eq!(
        model::effective_effort(&cfg, &id, Some("short")).as_deref(),
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
        model::effective_effort(&cfg, &id, Some("short")).as_deref(),
        Some("high")
    );
    model::select(&cfg, &id, None, Some(&EffortChoice::Set("low".into()))).unwrap();
    cfg.set_variant(Some("short".into()));
    assert_eq!(
        model::effective_effort(&cfg, &id, None).as_deref(),
        Some("low")
    );
    model::select(&cfg, &id, None, Some(&EffortChoice::Config)).unwrap();
    assert_eq!(model::effective_effort(&cfg, &id, None), None);
    assert_eq!(
        model::effective_effort(&cfg, &id, Some("short")).as_deref(),
        Some("high")
    );
}
#[tokio::test]
async fn minimal_gateway_config_uses_model_name_and_default_prompt() {
    let cfg: Config = serde_json::from_value(json!({"model":"gateway/glm","providers":{"gateway":{"baseURL":"http://localhost:8080/v1","apiKey":"test-only"}}})).unwrap();
    assert!(cfg.system_prompt().is_none());
    assert_eq!(cfg.directory(), PathBuf::from(".yourai/sessions"));
    assert_eq!(model::resolve(&cfg).unwrap().model.model_iden(), "glm");
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
    let resolved = model::resolve(&cfg).unwrap();
    assert_eq!(resolved.model.model_iden(), "actual-api-model");
    assert_eq!(
        resolved.model.token_budget().limits().context(),
        Some(64000)
    );
    assert_eq!(resolved.model.token_budget().max_output_tokens(), 4000);
    for variant in ["off", "missing"] {
        assert!(model::select(&cfg, &id, Some(variant.into()), None).is_err());
        cfg.set_variant(Some(variant.into()));
        assert!(model::resolve(&cfg).is_err());
    }
    cfg.set_model("absent/model".into());
    assert!(model::resolve(&cfg).is_err());
}
#[test]
fn validates_output_and_protects_runtime_fields() {
    for (key, value) in [("maxOutputTokens", json!(20000)), ("messages", json!([]))] {
        let cfg = config();
        cfg.set_model_option(&cfg.model(), key.into(), value)
            .unwrap();
        assert!(model::resolve(&cfg).is_err());
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
    assert_eq!(model::resolve(&cfg).unwrap().requests, (Some(7), 9));
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
