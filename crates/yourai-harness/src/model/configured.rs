//! Build model clients from typed configuration; no configuration storage lives here.
use crate::config::{
    ConnectionSettings, ConnectionTimeout, Error, ModelDefinition, ModelSettings, ProviderSettings,
};
use crate::GenaiModel;
use genai::{
    adapter::AdapterKind,
    chat::ChatOptions,
    resolver::{AuthData, Endpoint},
    Client,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use yourai_core::prelude::{ModelProvider, ModelTimeouts, ModelTokenBudget};

pub(crate) fn build_model(
    models: &ModelSettings,
    id: &str,
    variant: Option<&str>,
    effort: Option<&str>,
) -> Result<ResolvedModel, Error> {
    let (provider_id, provider, model_key) = models.provider_entry(id)?;
    let requests = provider.options().requests().clone();
    requests.validate()?;
    let defaults = ModelDefinition::default();
    let model = provider.models().get(model_key).unwrap_or(&defaults);
    let name = model.id().as_deref().unwrap_or(model_key);
    if name.trim().is_empty() {
        return Err("Model id is empty".into());
    }
    let mut options = Value::Object(model.options().clone());
    if let Some(variant) = variant {
        let selected = model
            .variants()
            .get(variant)
            .ok_or("Unknown model variant")?;
        if selected.disabled() {
            return Err("Model variant is disabled".into());
        }
        merge(&mut options, Value::Object(selected.options().clone()));
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
    let mut headers = provider.options().headers().clone();
    headers.extend(model.headers().clone());
    let headers: Vec<(String, String)> = headers
        .into_iter()
        .map(|(k, v)| Ok((k, expand(&v)?)))
        .collect::<Result<_, Error>>()?;
    let client = provider_client(provider, provider_id, chat)?;
    let (header_timeout, chunk_timeout) = model_timeouts(provider.options())?;
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
        provider: provider_id.into(),
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
        timeout("headerTimeout", options.header_timeout())?,
        timeout("chunkTimeout", options.chunk_timeout())?,
    ))
}
pub(crate) fn provider_client(
    provider: &ProviderSettings,
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
    if let Some(key) = provider.options().api_key().as_deref() {
        let key = expand(key)?;
        if key.trim().is_empty() {
            return Err("API key is empty".into());
        }
        builder =
            builder.with_auth_resolver_fn(move |_| Ok(Some(AuthData::from_single(key.clone()))));
    }
    if let Some(base) = provider.options().base_url().as_deref() {
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
    if let Some(timeout) = provider.options().timeout().as_ref() {
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
    pub requests: crate::model::RequestPolicy,
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

impl ModelSettings {
    pub(crate) fn provider_entry<'a>(
        &'a self,
        id: &'a str,
    ) -> Result<(&'a str, &'a ProviderSettings, &'a str), Error> {
        let (provider_id, key) = model_parts(id)?;
        let provider = self
            .providers()
            .get(provider_id)
            .ok_or("Unknown provider")?;
        Ok((provider_id, provider, key))
    }
}

pub(crate) fn model_parts(id: &str) -> Result<(&str, &str), Error> {
    let (provider, model) = id.split_once('/').ok_or("model must be provider/model")?;
    if provider.trim().is_empty() || model.trim().is_empty() {
        return Err("model must contain a nonempty provider and model".into());
    }
    Ok((provider, model))
}

impl ModelSettings {
    /// Build the recorded selection without overriding or changing its values.
    pub fn resolve(&self) -> Result<ResolvedModel, Error> {
        let variant = self.variant().as_deref();
        build_model(
            self,
            self.model(),
            variant,
            self.effective_effort(self.model(), variant).as_deref(),
        )
    }
    pub fn resolve_compaction(&self) -> Result<Option<ResolvedModel>, Error> {
        self.compaction()
            .as_ref()
            .map(|id| build_model(self, id, None, self.effective_effort(id, None).as_deref()))
            .transpose()
    }
    /// Apply the coupled model choice to a detached candidate as one operation.
    pub fn select_model(
        &self,
        id: &str,
        variant: Option<String>,
        effort: Option<&EffortChoice>,
    ) -> Result<Self, Error> {
        self.validate_selection(id, variant.as_deref())?;
        let mut next = self.clone();
        if let Some(effort) = effort {
            next.set_entry_effort(id, variant.as_deref(), effort)?;
        }
        next.set_model(id.into());
        next.set_variant(variant);
        Ok(next)
    }
    pub fn with_effort(
        &self,
        id: &str,
        variant: Option<&str>,
        choice: &EffortChoice,
    ) -> Result<Self, Error> {
        let mut next = self.clone();
        next.set_entry_effort(id, variant, choice)?;
        Ok(next)
    }
    fn set_entry_effort(
        &mut self,
        id: &str,
        variant: Option<&str>,
        choice: &EffortChoice,
    ) -> Result<(), Error> {
        self.validate_entry(
            id,
            variant,
            match choice {
                EffortChoice::Set(value) => Some(value),
                EffortChoice::Config => None,
            },
        )?;
        let mut overrides = self.effort_overrides().clone();
        match choice {
            EffortChoice::Config => {
                if let Some(entries) = overrides.get_mut(id) {
                    entries.remove(variant.unwrap_or(""));
                }
                if overrides.get(id).is_some_and(BTreeMap::is_empty) {
                    overrides.remove(id);
                }
            }
            EffortChoice::Set(value) => {
                overrides
                    .entry(id.into())
                    .or_default()
                    .insert(variant.unwrap_or("").into(), value.clone());
            }
        }
        self.set_effort_overrides(overrides);
        Ok(())
    }
    fn validate_selection(&self, id: &str, variant: Option<&str>) -> Result<(), Error> {
        self.validate_entry(id, variant, None)?;
        if let Some(variant) = variant {
            let (_, provider, key) = self.provider_entry(id)?;
            if provider.models()[key].variants()[variant].disabled() {
                return Err("Model variant is disabled".into());
            }
        }
        Ok(())
    }
    fn validate_entry(
        &self,
        id: &str,
        variant: Option<&str>,
        effort: Option<&str>,
    ) -> Result<(), Error> {
        let (_, provider, key) = self.provider_entry(id)?;
        if let Some(variant) = variant {
            let model = provider.models().get(key).ok_or("Unknown model")?;
            if !model.variants().contains_key(variant) {
                return Err("Unknown model variant".into());
            }
        }
        if let Some(value) = effort {
            if !EFFORT_CHOICES
                .iter()
                .flatten()
                .any(|allowed| *allowed == value)
            {
                return Err(format!("Invalid reasoningEffort: {value}").into());
            }
        }
        Ok(())
    }
    /// Returns the pricing for the currently selected model, if configured.
    pub fn pricing(&self) -> (Option<f64>, Option<f64>) {
        let model = self
            .provider_entry(self.model())
            .ok()
            .and_then(|(_, provider, key)| provider.models().get(key));
        model.map_or((None, None), |model| {
            (model.pricing().input(), model.pricing().output())
        })
    }
    /// Variant overrides/defaults precede model overrides/defaults.
    pub fn effective_effort(&self, id: &str, variant: Option<&str>) -> Option<String> {
        let overrides = self.effort_overrides().get(id);
        if let Some(value) = variant.and_then(|name| overrides?.get(name)) {
            return Some(value.clone());
        }
        let model = self
            .provider_entry(id)
            .ok()
            .and_then(|(_, provider, key)| provider.models().get(key));
        variant
            .and_then(|name| {
                model?
                    .variants()
                    .get(name)?
                    .options()
                    .get("reasoningEffort")
            })
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| overrides?.get("").cloned())
            .or_else(|| {
                model?
                    .options()
                    .get("reasoningEffort")?
                    .as_str()
                    .map(str::to_owned)
            })
    }
    pub(crate) fn validate(&self, loaded: bool) -> Result<(), Error> {
        if loaded || !self.model().is_empty() {
            model_parts(self.model())?;
        }
        // Embedders may inject a ModelProvider without declaring any clients.
        // Only validate a configured choice when this document defines providers;
        // actual client construction always checks the provider independently.
        if !self.providers().is_empty() {
            self.validate_selection(self.model(), self.variant().as_deref())?;
        }
        for provider in self.providers().values() {
            provider.options().requests().validate()?;
        }
        for (id, variants) in self.effort_overrides() {
            for (variant, effort) in variants {
                self.validate_entry(id, (!variant.is_empty()).then_some(variant), Some(effort))?;
            }
        }
        Ok(())
    }
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

impl ModelSettings {
    /// Project selectable models, variants and efforts from the same data version.
    pub fn choices(&self) -> Vec<ModelChoice> {
        let mut choices = Vec::new();
        for (provider_id, provider) in self.providers() {
            for (model_key, model) in provider.models() {
                let id = format!("{provider_id}/{model_key}");
                choices.push(ModelChoice {
                    id: id.clone(),
                    variant: None,
                    label: id.clone(),
                    effort: self.effective_effort(&id, None),
                });
                for (name, variant) in model.variants() {
                    if variant.disabled() {
                        continue;
                    }
                    choices.push(ModelChoice {
                        id: id.clone(),
                        variant: Some(name.clone()),
                        label: format!("{id} · {name}"),
                        effort: self.effective_effort(&id, Some(name)),
                    });
                }
            }
        }
        if !choices
            .iter()
            .any(|choice| choice.id == *self.model() && choice.variant.is_none())
        {
            choices.insert(
                0,
                ModelChoice {
                    id: self.model().clone(),
                    variant: None,
                    label: self.model().clone(),
                    effort: self.effective_effort(self.model(), None),
                },
            );
        }
        choices
    }
}
