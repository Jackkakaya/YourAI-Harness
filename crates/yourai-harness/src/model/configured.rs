//! Build model clients from typed configuration; no configuration storage lives here.
use crate::config::{ConnectionTimeout, Error, ModelConfig, ProviderConfig};
use crate::GenaiModel;
use genai::{
    adapter::AdapterKind,
    chat::ChatOptions,
    resolver::{AuthData, Endpoint},
    Client,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::{sync::Arc, time::Duration};
use yourai_core::prelude::{ModelProvider, ModelTimeouts, ModelTokenBudget};

pub(crate) fn build_model(
    data: &Value,
    id: &str,
    variant: Option<&str>,
    effort: Option<&str>,
) -> Result<ResolvedModel, Error> {
    let (provider_id, provider, model_key, model) = provider_entry(data, id)?;
    let requests = (provider.rpm(), provider.cooldown_seconds());
    super::control::validate(requests)?;
    let name = model.id().as_deref().unwrap_or(&model_key);
    if name.trim().is_empty() {
        return Err("Model id is empty".into());
    }
    let mut options = Value::Object(model.options().clone());
    if let Some(variant) = variant {
        let selected = model
            .variants()
            .get(variant)
            .ok_or("Unknown model variant")?;
        if selected
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err("Model variant is disabled".into());
        }
        let mut selected = selected.clone();
        selected.remove("disabled");
        merge(&mut options, Value::Object(selected));
    }
    if let Some(effort) = effort {
        options["reasoningEffort"] = Value::String(effort.into());
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Generation {
        max_output_tokens: Option<u32>,
        temperature: Option<f64>,
        top_p: Option<f64>,
        reasoning_effort: Option<String>,
        #[serde(flatten)]
        extra: Map<String, Value>,
    }
    let generation: Generation =
        serde_json::from_value(options).map_err(|_| "Invalid model options value type")?;
    let budget = ModelTokenBudget::resolve(model.limit(), generation.max_output_tokens)?;
    if generation
        .temperature
        .is_some_and(|v| !v.is_finite() || v < 0.0)
        || generation.top_p.is_some_and(|v| !(0.0..=1.0).contains(&v))
    {
        return Err("Invalid temperature or topP".into());
    }
    use genai::chat::ReasoningEffort;
    let reasoning_effort = generation
        .reasoning_effort
        .map(|v| {
            Ok(match v.as_str() {
                "none" => ReasoningEffort::Zero,
                "minimal" => ReasoningEffort::Minimal,
                "low" => ReasoningEffort::Low,
                "medium" => ReasoningEffort::Medium,
                "high" => ReasoningEffort::High,
                "xhigh" => ReasoningEffort::XHigh,
                "max" => ReasoningEffort::Max,
                _ => return Err("Invalid reasoningEffort"),
            })
        })
        .transpose()?;
    let mut chat = ChatOptions {
        temperature: generation.temperature,
        top_p: generation.top_p,
        reasoning_effort,
        ..Default::default()
    };
    let options = generation.extra;
    // Other options are provider-native request body fields. Structural fields remain runtime-owned.
    for reserved in [
        "model",
        "messages",
        "input",
        "system",
        "tools",
        "stream",
        "max_tokens",
        "max_completion_tokens",
        "max_output_tokens",
    ] {
        if options.contains_key(reserved) {
            return Err(format!(
                "Reserved model option: {reserved}; use maxOutputTokens for output limits"
            )
            .into());
        }
    }
    if !options.is_empty() {
        chat.extra_body = Some(Value::Object(options));
    }
    let mut headers = provider.headers().clone();
    headers.extend(model.headers().clone());
    let headers: Vec<(String, String)> = headers
        .into_iter()
        .map(|(k, v)| Ok((k, expand(&v)?)))
        .collect::<Result<_, Error>>()?;
    let client = provider_client(&provider, &provider_id, chat)?;
    let (header_timeout, chunk_timeout) = model_timeouts(&provider)?;
    let defaults = ModelTimeouts::default();
    let model = Arc::new(
        GenaiModel::new(client, name)
            .with_headers(headers.into())
            .with_settings(budget, {
                let mut settings = ModelTimeouts::default();
                settings.set_headers(header_timeout.unwrap_or(defaults.headers()));
                settings.set_read(chunk_timeout.unwrap_or(defaults.read()));
                settings
            })?,
    );
    Ok(ResolvedModel {
        model,
        provider: provider_id,
        requests,
    })
}

fn merge(base: &mut Value, other: Value) {
    match (base, other) {
        (Value::Object(base), Value::Object(other)) => {
            for (k, v) in other {
                merge(base.entry(k).or_insert(Value::Null), v);
            }
        }
        (base, other) => *base = other,
    }
}
fn expand(value: &str) -> Result<String, Error> {
    if let Some(key) = value
        .strip_prefix("{env:")
        .and_then(|s| s.strip_suffix('}'))
    {
        return std::env::var(key)
            .map_err(|_| format!("Missing environment variable: {key}").into());
    }
    Ok(value.into())
}
pub(crate) fn model_timeouts(
    options: &ProviderConfig,
) -> Result<(Option<Duration>, Option<Duration>), Error> {
    let timeout = |key: &str, value: Option<u64>| -> Result<Option<Duration>, Error> {
        value
            .map(|n| {
                if n == 0 {
                    Err(format!("{key} must be positive milliseconds").into())
                } else {
                    Ok(Duration::from_millis(n))
                }
            })
            .transpose()
    };
    Ok((
        timeout("headerTimeout", options.header_timeout())?,
        timeout("chunkTimeout", options.chunk_timeout())?,
    ))
}
pub(crate) fn provider_client(
    provider: &ProviderConfig,
    provider_id: &str,
    chat: ChatOptions,
) -> Result<Client, Error> {
    let adapter = match provider.api().as_deref().unwrap_or(match provider_id {
        "openai" => "openai-responses",
        "anthropic" => "anthropic",
        "google" => "gemini",
        "ollama" => "ollama",
        _ => "openai-compatible",
    }) {
        "openai-compatible" | "@ai-sdk/openai-compatible" => AdapterKind::OpenAI,
        "openai-responses" | "@ai-sdk/openai" | "openai" => AdapterKind::OpenAIResp,
        "@ai-sdk/anthropic" | "anthropic" => AdapterKind::Anthropic,
        "gemini" | "@ai-sdk/google" | "google" => AdapterKind::Gemini,
        "ollama-ai-provider" | "ollama" => AdapterKind::Ollama,
        _ => return Err("Unsupported provider api; see docs/tui.md#模型配置".into()),
    };
    if chat.extra_body.is_some()
        && !matches!(adapter, AdapterKind::OpenAI | AdapterKind::OpenAIResp)
    {
        return Err("Extra model options are currently supported only for OpenAI-compatible/Responses adapters".into());
    }
    let mut builder = Client::builder()
        .with_adapter_kind(adapter)
        .with_chat_options(chat);
    if let Some(key) = provider.api_key().as_deref() {
        let key = expand(key)?;
        if key.trim().is_empty() {
            return Err("API key is empty".into());
        }
        builder =
            builder.with_auth_resolver_fn(move |_| Ok(Some(AuthData::from_single(key.clone()))));
    }
    if let Some(base) = provider.base_url().as_deref() {
        let mut url = reqwest::Url::parse(&expand(base)?).map_err(|_| "Invalid baseURL")?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("baseURL must be HTTP(S), without credentials, query or fragment".into());
        }
        if !url.path().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }
        let endpoint = Endpoint::from_owned(url.to_string());
        builder =
            builder.with_service_target_resolver_fn(move |mut target: genai::ServiceTarget| {
                target.endpoint = endpoint.clone();
                Ok(target)
            });
    }
    if let Some(timeout) = provider.timeout().as_ref() {
        let duration = match timeout {
            ConnectionTimeout::Disabled(false) => None,
            ConnectionTimeout::Milliseconds(n) if *n > 0 => Some(Duration::from_millis(*n)),
            _ => return Err("timeout must be positive milliseconds or false".into()),
        };
        builder = builder.with_web_config(genai::WebConfig {
            timeout: duration,
            ..Default::default()
        });
    }
    Ok(builder.build()?)
}

#[derive(Clone)]
pub struct ResolvedModel {
    pub model: Arc<dyn ModelProvider>,
    pub provider: String,
    pub requests: (Option<u32>, u64),
}

pub const EFFORT_CHOICES: [Option<&str>; 8] = [
    None,
    Some("none"),
    Some("minimal"),
    Some("low"),
    Some("medium"),
    Some("high"),
    Some("xhigh"),
    Some("max"),
];

/// Runtime thinking-effort selection for one `/models` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffortChoice {
    /// Keep what the (merged) config options specify; removes any runtime
    /// override previously applied through the picker.
    Config,
    /// Set the effort keyword for this entry.
    Set(String),
}

pub(crate) fn model_parts(id: &str) -> Result<(&str, &str), Error> {
    let (provider, model) = id.split_once('/').ok_or("model must be provider/model")?;
    if provider.trim().is_empty() || model.trim().is_empty() {
        return Err("model must contain a nonempty provider and model".into());
    }
    Ok((provider, model))
}
fn provider_entry(
    data: &Value,
    id: &str,
) -> Result<(String, ProviderConfig, String, ModelConfig), Error> {
    let (fallback, key) = model_parts(id)?;
    let model: ModelConfig = match data["models"].get(id) {
        Some(value) => crate::config::decode(value.clone(), "model")?,
        None => ModelConfig::default(),
    };
    let provider_id = if model.provider().is_empty() {
        fallback
    } else {
        model.provider()
    };
    let provider = data["providers"]
        .get(provider_id)
        .ok_or("Unknown provider")?;
    Ok((
        provider_id.into(),
        crate::config::decode(provider.clone(), "provider")?,
        key.into(),
        model,
    ))
}
pub(crate) fn resolve_data(data: &Value) -> Result<ResolvedModel, Error> {
    let id: String = crate::config::read(data, "model");
    let variant: Option<String> = crate::config::read(data, "variant");
    build_model(
        data,
        &id,
        variant.as_deref(),
        effective_effort_data(data, &id, variant.as_deref()).as_deref(),
    )
}
pub(crate) fn resolve_compaction_data(data: &Value) -> Result<Option<ResolvedModel>, Error> {
    let id: Option<String> = crate::config::read(data, "compaction_model");
    id.map(|id| {
        build_model(
            data,
            &id,
            None,
            effective_effort_data(data, &id, None).as_deref(),
        )
    })
    .transpose()
}
fn validate_entry(
    data: &Value,
    id: &str,
    variant: Option<&str>,
    effort: Option<&str>,
) -> Result<(), Error> {
    let (_, _, _, model) = provider_entry(data, id)?;
    if let Some(variant) = variant {
        if !model.variants().contains_key(variant) {
            return Err("Unknown model variant".into());
        }
    }
    if effort.is_some_and(|v| !EFFORT_CHOICES.iter().flatten().any(|allowed| *allowed == v)) {
        return Err("Invalid reasoningEffort".into());
    }
    Ok(())
}
pub(crate) fn select_data(
    data: &Value,
    id: &str,
    variant: Option<String>,
    effort: Option<&EffortChoice>,
) -> Result<Value, Error> {
    validate_entry(
        data,
        id,
        variant.as_deref(),
        match effort {
            Some(EffortChoice::Set(value)) => Some(value),
            _ => None,
        },
    )?;
    if let Some(variant) = &variant {
        if data["models"][id]["variants"][variant]["disabled"]
            .as_bool()
            .unwrap_or(false)
        {
            return Err("Model variant is disabled".into());
        }
    }
    let mut next = data.clone();
    if let Some(effort) = effort {
        set_effort(&mut next, id, variant.as_deref(), effort)?;
    }
    crate::config::put(&mut next, "model", id);
    crate::config::put(&mut next, "variant", variant);
    Ok(next)
}
fn set_effort(
    data: &mut Value,
    id: &str,
    variant: Option<&str>,
    effort: &EffortChoice,
) -> Result<(), Error> {
    validate_entry(
        data,
        id,
        variant,
        match effort {
            EffortChoice::Set(v) => Some(v),
            EffortChoice::Config => None,
        },
    )?;
    let overrides = data["effort_overrides"].as_object_mut().unwrap();
    match effort {
        EffortChoice::Set(value) => {
            overrides
                .entry(id.to_owned())
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
                .unwrap()
                .insert(variant.unwrap_or("").into(), Value::String(value.clone()));
        }
        EffortChoice::Config => {
            if let Some(entries) = overrides.get_mut(id).and_then(Value::as_object_mut) {
                entries.remove(variant.unwrap_or(""));
                if entries.is_empty() {
                    overrides.remove(id);
                }
            }
        }
    }
    Ok(())
}
pub(crate) fn apply_selection(data: &mut Value, next: &Value) {
    for key in ["model", "variant", "effort_overrides"] {
        crate::config::put(data, key, &next[key]);
    }
}
pub(crate) fn dependencies(data: &Value) -> Value {
    serde_json::json!([
        data["model"],
        data["variant"],
        data["compaction_model"],
        data["effort_overrides"],
        data["providers"],
        data["models"],
        data["execution"]["context"]
    ])
}
pub(crate) fn pricing_data(data: &Value) -> (Option<f64>, Option<f64>) {
    let id: String = crate::config::read(data, "model");
    provider_entry(data, &id).map_or((None, None), |(_, _, _, model)| {
        (model.input_price(), model.output_price())
    })
}
pub(crate) fn effective_effort_data(
    data: &Value,
    id: &str,
    variant: Option<&str>,
) -> Option<String> {
    let overrides = &data["effort_overrides"][id];
    variant
        .and_then(|name| overrides[name].as_str())
        .map(str::to_owned)
        .or_else(|| {
            variant
                .and_then(|name| data["models"][id]["variants"][name]["reasoningEffort"].as_str())
                .map(str::to_owned)
        })
        .or_else(|| overrides[""].as_str().map(str::to_owned))
        .or_else(|| {
            data["models"][id]["options"]["reasoningEffort"]
                .as_str()
                .map(str::to_owned)
        })
}
pub(crate) fn validate_data(data: &Value, loaded: bool) -> Result<(), Error> {
    let id: String = crate::config::read(data, "model");
    let variant: Option<String> = crate::config::read(data, "variant");
    if loaded || !id.is_empty() {
        model_parts(&id)?;
    }
    if !data["providers"].as_object().unwrap().is_empty() {
        validate_entry(data, &id, variant.as_deref(), None)?;
        if variant.as_ref().is_some_and(|v| {
            data["models"][&id]["variants"][v]["disabled"]
                .as_bool()
                .unwrap_or(false)
        }) {
            return Err("Model variant is disabled".into());
        }
    }
    for provider in data["providers"].as_object().unwrap().values() {
        let provider: ProviderConfig = crate::config::decode(provider.clone(), "provider")?;
        super::control::validate((provider.rpm(), provider.cooldown_seconds()))?;
    }
    for (id, entries) in data["effort_overrides"].as_object().unwrap() {
        for (variant, effort) in entries.as_object().unwrap() {
            validate_entry(
                data,
                id,
                (!variant.is_empty()).then_some(variant),
                effort.as_str(),
            )?;
        }
    }
    Ok(())
}

/// A selectable model entry in the `/models` picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelChoice {
    /// "provider/model"
    pub id: String,
    /// Variant name; None for the default (no-variant) entry.
    pub variant: Option<String>,
    /// Display label: "provider/model" or "provider/model · variant".
    pub label: String,
    /// Effective reasoning effort for this entry: the variant's
    /// `reasoningEffort` when set, else the model's. None = no hint.
    pub effort: Option<String>,
}

fn choices_data(data: &Value) -> Vec<ModelChoice> {
    let mut choices = Vec::new();
    for (id, model) in data["models"].as_object().unwrap() {
        choices.push(ModelChoice {
            id: id.clone(),
            variant: None,
            label: id.clone(),
            effort: effective_effort_data(data, id, None),
        });
        for (name, variant) in model["variants"].as_object().unwrap() {
            if variant["disabled"].as_bool().unwrap_or(false) {
                continue;
            }
            choices.push(ModelChoice {
                id: id.clone(),
                variant: Some(name.clone()),
                label: format!("{id} · {name}"),
                effort: effective_effort_data(data, id, Some(name)),
            });
        }
    }
    let id: String = crate::config::read(data, "model");
    if !choices.iter().any(|c| c.id == id && c.variant.is_none()) {
        choices.insert(
            0,
            ModelChoice {
                id: id.clone(),
                variant: None,
                label: id.clone(),
                effort: effective_effort_data(data, &id, None),
            },
        );
    }
    choices
}
pub fn resolve(config: &crate::config::Config) -> Result<ResolvedModel, Error> {
    resolve_data(&config.values())
}
pub fn resolve_compaction(config: &crate::config::Config) -> Result<Option<ResolvedModel>, Error> {
    resolve_compaction_data(&config.values())
}
pub fn choices(config: &crate::config::Config) -> Vec<ModelChoice> {
    choices_data(&config.write())
}
pub fn pricing(config: &crate::config::Config) -> (Option<f64>, Option<f64>) {
    pricing_data(&config.write())
}
pub fn effective_effort(
    config: &crate::config::Config,
    id: impl AsRef<str>,
    variant: Option<&str>,
) -> Option<String> {
    effective_effort_data(&config.write(), id.as_ref(), variant)
}
pub fn select(
    config: &crate::config::Config,
    id: &str,
    variant: Option<String>,
    effort: Option<&EffortChoice>,
) -> Result<(), Error> {
    let mut data = config.write();
    let next = select_data(&data, id, variant, effort)?;
    apply_selection(&mut data, &next);
    Ok(())
}
