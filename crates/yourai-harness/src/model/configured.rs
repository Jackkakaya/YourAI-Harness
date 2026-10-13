//! Build model clients from typed configuration; no configuration storage lives here.
use crate::config::decode;
use crate::{
    config::{
        Config, ConnectionSettings, ConnectionTimeout, Error, ModelDefinition, ProviderSettings,
        ResolvedModel,
    },
    GenaiModel,
};
use genai::{
    adapter::AdapterKind,
    chat::ChatOptions,
    resolver::{AuthData, Endpoint},
    Client,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::{sync::Arc, time::Duration};
use yourai_core::prelude::{ModelTimeouts, ModelTokenBudget};

impl Config {
    /// Resolve one model/variant with its capacity, output budget and timeouts.
    /// Context maintenance policy is independent of model selection.
    pub fn resolve(&self, variant: Option<&str>) -> Result<ResolvedModel, Error> {
        self.resolve_selection(
            &self.models.selected,
            variant,
            self.effective_effort(&self.models.selected, variant)
                .as_deref(),
        )
    }
    /// Resolve a selection without changing the loaded file configuration.
    pub fn resolve_selection(
        &self,
        id: &str,
        variant: Option<&str>,
        effort: Option<&str>,
    ) -> Result<ResolvedModel, Error> {
        if self.execution.steps == Some(0) {
            return Err("steps must be a positive integer".into());
        }
        let (provider_id, model_key) = id
            .split_once('/')
            .ok_or("model must be provider_id/model_id")?;
        let provider = self
            .models
            .providers
            .get(provider_id)
            .ok_or("Unknown provider in model selection")?;
        let requests = provider.options.requests.clone();
        requests.validate()?;
        let defaults = ModelDefinition::default();
        let model = provider.models.get(model_key).unwrap_or(&defaults);
        let name = model.id.as_deref().unwrap_or(model_key);
        if name.trim().is_empty() {
            return Err("Model id is empty".into());
        }
        let mut options = Value::Object(model.options.clone());
        if let Some(variant) = variant {
            let selected = model.variants.get(variant).ok_or("Unknown model variant")?;
            if selected.disabled {
                return Err("Model variant is disabled".into());
            }
            merge(&mut options, Value::Object(selected.options.clone()));
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
        let generation: Generation = decode(options, "model options")?;
        let budget = ModelTokenBudget::resolve(model.limit, generation.max_output_tokens)?;
        self.execution.context_policy.validate_for(budget)?;
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
        let mut headers = provider.options.headers.clone();
        headers.extend(model.headers.clone());
        let headers: Vec<(String, String)> = headers
            .into_iter()
            .map(|(k, v)| Ok((k, expand(&v)?)))
            .collect::<Result<_, Error>>()?;
        let client = provider_client(provider, provider_id, chat)?;
        let (header_timeout, chunk_timeout) = model_timeouts(&provider.options)?;
        let defaults = ModelTimeouts::default();
        let model = Arc::new(
            GenaiModel::new(client, name)
                .with_headers(headers.into())
                .with_settings(
                    budget,
                    ModelTimeouts {
                        headers: header_timeout.unwrap_or(defaults.headers),
                        read: chunk_timeout.unwrap_or(defaults.read),
                    },
                )?,
        );
        Ok(ResolvedModel {
            model,
            provider: provider_id.into(),
            requests,
        })
    }

    pub fn resolve_compaction(&self) -> Result<Option<ResolvedModel>, Error> {
        self.models
            .compaction
            .as_ref()
            .map(|id| {
                let mut selected = self.clone();
                selected.models.selected = id.clone();
                selected.resolve(None)
            })
            .transpose()
    }
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
    options: &ConnectionSettings,
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
        timeout("headerTimeout", options.header_timeout)?,
        timeout("chunkTimeout", options.chunk_timeout)?,
    ))
}
pub(crate) fn provider_client(
    provider: &ProviderSettings,
    provider_id: &str,
    chat: ChatOptions,
) -> Result<Client, Error> {
    let adapter = match provider.api.as_deref().unwrap_or(match provider_id {
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
    if let Some(key) = provider.options.api_key.as_deref() {
        let key = expand(key)?;
        if key.trim().is_empty() {
            return Err("API key is empty".into());
        }
        builder =
            builder.with_auth_resolver_fn(move |_| Ok(Some(AuthData::from_single(key.clone()))));
    }
    if let Some(base) = provider.options.base_url.as_deref() {
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
    if let Some(timeout) = provider.options.timeout.as_ref() {
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
