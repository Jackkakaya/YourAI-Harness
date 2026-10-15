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
use yourai_core::prelude::{ModelLimits, ModelProvider, ModelTimeouts, ModelTokenBudget};

/// A configured model/variant and its effective settings, independent of UI labels.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelEntry {
    pub id: String,
    pub variant: Option<String>,
    pub effort: Option<String>,
    pub available: bool,
    pub input_price: Option<f64>,
    pub output_price: Option<f64>,
}

/// List configured models and variants. Availability means the provider binding
/// exists and the variant is enabled; credentials and connectivity are not tested.
pub fn list_models(config: &crate::config::Config, available_only: bool) -> Vec<ModelEntry> {
    list_data(&config.write(), available_only)
}

pub(crate) fn list_data(data: &Value, available_only: bool) -> Vec<ModelEntry> {
    let mut ids: Vec<_> = data["models"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    let current = data["model"].as_str().unwrap();
    if !current.is_empty() && !ids.iter().any(|id| id == current) {
        ids.push(current.to_owned());
    }
    let mut entries = Vec::new();
    for id in ids {
        let binding = configured_model(data, &id);
        let bound = binding.is_ok();
        let settings = match binding {
            Ok((_, _, _, settings)) => settings,
            Err(_) => data["models"]
                .get(&id)
                .map(|value| crate::config::decode::<ModelConfig>(value.clone(), "model").unwrap())
                .unwrap_or_default(),
        };
        for variant in
            std::iter::once(None).chain(settings.variants().keys().map(|name| Some(name.as_str())))
        {
            let available = bound && options_for(&settings, variant).is_ok();
            if available_only && !available {
                continue;
            }
            entries.push(ModelEntry {
                id: id.clone(),
                variant: variant.map(str::to_owned),
                effort: effective_effort_data(data, &id, variant),
                available,
                input_price: settings.input_price(),
                output_price: settings.output_price(),
            });
        }
    }
    entries
}

pub(crate) fn resolve_data(
    data: &Value,
    id: &str,
    variant: Option<&str>,
) -> Result<ResolvedModel, Error> {
    let effort = effective_effort_data(data, id, variant);
    let (provider_id, provider, model_key, model) = configured_model(data, id)?;
    let requests = (provider.rpm(), provider.cooldown_seconds());
    super::control::validate(requests)?;
    let name = model.id().as_deref().unwrap_or(&model_key);
    if name.trim().is_empty() {
        return Err("Model id is empty".into());
    }
    let mut options = Value::Object(options_for(&model, variant)?);
    if let Some(effort) = &effort {
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
    let mut limits = ModelLimits::default();
    limits.set_context(model.context());
    limits.set_input(model.input());
    limits.set_output(model.output());
    let budget = ModelTokenBudget::resolve(limits, generation.max_output_tokens)?;
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
    let (header_timeout, chunk_timeout) = model_timeouts(
        model.header_timeout().or(provider.header_timeout()),
        model.chunk_timeout().or(provider.chunk_timeout()),
    )?;
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
fn model_timeouts(
    header_timeout: Option<u64>,
    chunk_timeout: Option<u64>,
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
        timeout("header_timeout", header_timeout)?,
        timeout("chunk_timeout", chunk_timeout)?,
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
/// Resolve a configured binding (including implicit models), without creating clients.
fn configured_model(
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
/// Resolve inherited generation parameters without constructing a client.
/// A disabled or missing variant cannot be used by any caller.
fn options_for(model: &ModelConfig, variant: Option<&str>) -> Result<Map<String, Value>, Error> {
    let mut options = Value::Object(model.options().clone());
    if let Some(name) = variant {
        let selected = model.variants().get(name).ok_or("Unknown model variant")?;
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
    match options {
        Value::Object(options) => Ok(options),
        _ => unreachable!("object options merge into an object"),
    }
}
pub(crate) fn select_data(
    data: &Value,
    id: &str,
    variant: Option<String>,
    effort: Option<&EffortChoice>,
) -> Result<Value, Error> {
    let (_, _, _, model) = configured_model(data, id)?;
    options_for(&model, variant.as_deref())?;
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
    if let EffortChoice::Set(value) = effort {
        if !EFFORT_CHOICES
            .iter()
            .flatten()
            .any(|allowed| *allowed == value)
        {
            return Err("Invalid reasoningEffort".into());
        }
    }
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
fn effective_effort_data(data: &Value, id: &str, variant: Option<&str>) -> Option<String> {
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
        let (_, _, _, model) = configured_model(data, &id)?;
        options_for(&model, variant.as_deref())?;
    }
    for provider in data["providers"].as_object().unwrap().values() {
        let provider: ProviderConfig = crate::config::decode(provider.clone(), "provider")?;
        super::control::validate((provider.rpm(), provider.cooldown_seconds()))?;
    }
    for (id, entries) in data["effort_overrides"].as_object().unwrap() {
        let entries = entries.as_object().unwrap();
        if entries.is_empty() {
            continue;
        }
        let (_, _, _, model) = configured_model(data, id)?;
        for (name, effort) in entries {
            if !name.is_empty() && !model.variants().contains_key(name) {
                return Err("Unknown model variant".into());
            }
            if !EFFORT_CHOICES
                .iter()
                .flatten()
                .any(|allowed| Some(*allowed) == effort.as_str())
            {
                return Err("Invalid reasoningEffort".into());
            }
        }
    }
    Ok(())
}

/// Construct the specified model without changing the current selection.
pub fn resolve(
    config: &crate::config::Config,
    id: &str,
    variant: Option<&str>,
) -> Result<ResolvedModel, Error> {
    resolve_data(&config.values(), id, variant)
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

#[cfg(test)]
mod list_tests {
    #[test]
    fn all_includes_disabled_and_missing_provider_but_available_filters_them() {
        let config: crate::config::Config = serde_json::from_value(serde_json::json!({
            "model": "mock/a",
            "providers": {"mock": {}},
            "models": {
                "mock/a": {"variants": {"enabled": {}, "disabled": {"disabled": true}}},
                "missing/b": {}
            }
        }))
        .unwrap();
        let all = super::list_models(&config, false);
        assert_eq!(all.len(), 4);
        assert_eq!(all.iter().filter(|entry| entry.available).count(), 2);
        let available = super::list_models(&config, true);
        assert_eq!(
            available,
            all.into_iter()
                .filter(|entry| entry.available)
                .collect::<Vec<_>>()
        );
        assert!(available.iter().all(|entry| entry.id == "mock/a"));
    }
}
