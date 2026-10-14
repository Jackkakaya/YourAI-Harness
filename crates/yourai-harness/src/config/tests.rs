#[allow(clippy::wildcard_imports)]
use super::*;
use crate::model::{configured::provider_client, ResolvedModel};
use genai::{adapter::AdapterKind, chat::ChatOptions};
#[test]
fn current_model_without_a_declared_entry_can_override_and_restore_effort() {
    let cfg = config();
    let mut models = cfg.values().models().clone().clone();
    models.providers.get_mut("gateway").unwrap().models.clear();
    cfg.set_providers(models.providers);
    let id = cfg.values().models().clone().model.clone();
    cfg.set_models(
        cfg.values()
            .models()
            .clone()
            .with_effort(&id, None, &crate::model::EffortChoice::Set("high".into()))
            .unwrap(),
    );
    assert_eq!(
        cfg.values()
            .models()
            .clone()
            .effective_effort(&id, None)
            .as_deref(),
        Some("high")
    );
    assert!(cfg.values().models().clone().resolve().is_ok());
    cfg.set_models(
        cfg.values()
            .models()
            .clone()
            .with_effort(&id, None, &crate::model::EffortChoice::Config)
            .unwrap(),
    );
    assert_eq!(
        cfg.values().models().clone().effective_effort(&id, None),
        None
    );
    assert!(cfg.values().models().clone().resolve().is_ok());
    assert!(cfg
        .values()
        .models()
        .clone()
        .with_effort(
            "gateway/unknown",
            Some("missing"),
            &crate::model::EffortChoice::Set("high".into())
        )
        .is_err());
    assert!(!cfg.values().models().clone().providers["gateway"]
        .models
        .contains_key("unknown"));
}
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
fn compaction_selection_is_optional_independent_and_validated() {
    let cfg = config();
    assert!(cfg
        .values()
        .models()
        .clone()
        .resolve_compaction()
        .unwrap()
        .is_none());
    cfg.set_compaction(Some("gateway/alias/with/slash".into()));
    cfg.set_models(
        cfg.values()
            .models()
            .clone()
            .select_model("unused/main", None, None)
            .unwrap(),
    );
    let summary = cfg
        .values()
        .models()
        .clone()
        .resolve_compaction()
        .unwrap()
        .unwrap();
    assert_eq!(summary.model.model_iden(), "actual-api-model");
    assert_eq!(
        summary.model.token_budget().limits().context(),
        Some(64_000)
    );
    assert_eq!(summary.model.token_budget().max_output_tokens(), 8000);
    assert_eq!(summary.provider, "gateway");
    assert_eq!(cfg.values().models().clone().model, "unused/main");
    cfg.set_compaction(Some("missing/model".into()));
    assert!(cfg.values().models().clone().resolve_compaction().is_err());
}
#[test]
fn config_effort_restores_original_values_after_repeated_overrides() {
    use crate::model::EffortChoice;
    let cfg = config();
    let id = "gateway/alias/with/slash";
    // Choosing Config without an override must preserve the configured variant.
    cfg.set_models(
        cfg.values()
            .models()
            .clone()
            .with_effort(id, Some("short"), &EffortChoice::Config)
            .unwrap(),
    );
    assert_eq!(
        cfg.values()
            .models()
            .clone()
            .effective_effort(id, Some("short"))
            .as_deref(),
        Some("high")
    );
    for keyword in ["low", "medium"] {
        cfg.set_models(
            cfg.values()
                .models()
                .clone()
                .with_effort(id, Some("short"), &EffortChoice::Set(keyword.into()))
                .unwrap(),
        );
    }
    cfg.set_models(
        cfg.values()
            .models()
            .clone()
            .with_effort(id, Some("short"), &EffortChoice::Config)
            .unwrap(),
    );
    assert_eq!(
        cfg.values()
            .models()
            .clone()
            .effective_effort(id, Some("short"))
            .as_deref(),
        Some("high")
    );
    // Entries without an original value restore inheritance instead of erasing it.
    cfg.set_models(
        cfg.values()
            .models()
            .clone()
            .with_effort(id, None, &EffortChoice::Set("low".into()))
            .unwrap(),
    );
    cfg.set_models(
        cfg.values()
            .models()
            .clone()
            .with_effort(id, Some("off"), &EffortChoice::Set("medium".into()))
            .unwrap(),
    );
    cfg.set_models(
        cfg.values()
            .models()
            .clone()
            .with_effort(id, Some("off"), &EffortChoice::Config)
            .unwrap(),
    );
    assert_eq!(
        cfg.values()
            .models()
            .clone()
            .effective_effort(id, Some("off"))
            .as_deref(),
        Some("low")
    );
    cfg.set_models(
        cfg.values()
            .models()
            .clone()
            .with_effort(id, None, &EffortChoice::Config)
            .unwrap(),
    );
    assert_eq!(
        cfg.values().models().clone().effective_effort(id, None),
        None
    );
    assert_eq!(
        cfg.values()
            .models()
            .clone()
            .effective_effort(id, Some("short"))
            .as_deref(),
        Some("high")
    );
}
#[tokio::test]
async fn minimal_gateway_config_uses_model_name_and_default_prompt() {
    let config: Config = serde_json::from_value(json!({
        "models":{"model":"gateway/glm",
        "providers":{"gateway":{"options":{"baseURL":"http://localhost:8080/v1","apiKey":"test-only"}}}}
    })).unwrap();
    assert!(config.values().prompt().clone().system_prompt.is_none());
    assert_eq!(
        config.values().session().clone().directory,
        PathBuf::from(".yourai/sessions")
    );
    assert_eq!(
        config
            .values()
            .models()
            .clone()
            .resolve()
            .unwrap()
            .model
            .model_iden(),
        "glm"
    );
    let client = provider_client(
        &config.values().models().clone().providers["gateway"],
        "gateway",
        ChatOptions::default(),
    )
    .unwrap();
    let target = client
        .resolve_service_target(genai::ModelIden::new(AdapterKind::OpenAI, "glm"))
        .await
        .unwrap();
    assert_eq!(target.model.adapter_kind, AdapterKind::OpenAI);
    assert_eq!(target.endpoint.base_url(), "http://localhost:8080/v1/");
    let explicit: ProviderSettings =
        serde_json::from_value(json!({"api":"anthropic","options":{"apiKey":"test-only"}}))
            .unwrap();
    assert!(provider_client(&explicit, "custom", ChatOptions::default()).is_ok());
    assert!(serde_json::from_value::<ProviderSettings>(
        json!({"api":"anthropic","npm":"@ai-sdk/openai"})
    )
    .is_err());
}
#[test]
fn selects_model_and_variant_without_resolving_unused_credentials() {
    let config = config();
    let id = config.values().models().clone().model.clone();
    config.set_models(
        config
            .values()
            .models()
            .clone()
            .select_model(&id, Some("short".into()), None)
            .unwrap(),
    );
    let ResolvedModel { model, .. } = config.values().models().clone().resolve().unwrap();
    assert_eq!(model.model_iden(), "actual-api-model");
    // Capacity and the variant's output budget belong to the resolved model.
    assert_eq!(model.token_budget().limits().context(), Some(64000));
    assert_eq!(model.token_budget().max_output_tokens(), 4000);
    for variant in ["off", "missing"] {
        assert!(config
            .values()
            .models()
            .clone()
            .select_model(&id, Some(variant.into()), None)
            .is_err());
        let mut document = serde_json::to_value(&config).unwrap();
        document["models"]["variant"] = json!(variant);
        let invalid: Config = serde_json::from_value(document).unwrap();
        assert!(invalid.values().models().clone().resolve().is_err());
    }
    let mut document = serde_json::to_value(&config).unwrap();
    document["models"]["model"] = json!("absent/model");
    let invalid: Config = serde_json::from_value(document).unwrap();
    assert!(invalid.values().models().clone().resolve().is_err());
}
#[test]
fn validates_output_and_protects_runtime_fields() {
    let config = config();
    let mut models = config.values().models().clone().clone();
    models
        .providers
        .get_mut("gateway")
        .unwrap()
        .models
        .get_mut("alias/with/slash")
        .unwrap()
        .options
        .insert("maxOutputTokens".into(), json!(20000));
    config.set_providers(models.providers);
    assert!(config.values().models().clone().resolve().is_err());
    let config = self::config();
    let mut models = config.values().models().clone().clone();
    models
        .providers
        .get_mut("gateway")
        .unwrap()
        .models
        .get_mut("alias/with/slash")
        .unwrap()
        .options
        .insert("messages".into(), json!([]));
    config.set_providers(models.providers);
    assert!(config.values().models().clone().resolve().is_err());
}
#[test]
fn json_example_and_prompt_fields_parse_and_unknown_fields_fail() {
    let config = config();
    assert!(config.values().security().clone().yolo);
    assert_eq!(
        config.values().prompt().clone().soul_file,
        Some(PathBuf::from("./SOUL.md"))
    );
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
