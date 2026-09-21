use genai::{
    adapter::AdapterKind,
    chat::ChatOptions,
    resolver::{AuthData, Endpoint},
    Client,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use yourai_harness::GenaiModel;
pub type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub theme: crate::ui::theme::Theme,
    #[serde(rename = "$schema")]
    pub _schema: Option<String>,
    pub model: String,
    /// Runtime selection; persisted configuration still uses CLI / picker variants.
    #[serde(skip)]
    pub selected_variant: Option<String>,
    pub max_model_calls: Option<u32>,
    pub provider: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub extensions: bool,
    #[serde(default)]
    pub trusted_shell: bool,
    #[serde(default)]
    pub yolo: bool,
    #[serde(default)]
    pub context: yourai_core::prelude::ContextPolicy,
    #[serde(default = "sessions")]
    pub session_dir: PathBuf,
    pub system_prompt: Option<String>,
    #[serde(flatten)]
    pub prompt: yourai_harness::PromptConfig,
    #[serde(default)]
    pub memory_search_limit: usize,
}
fn sessions() -> PathBuf {
    ".yourai/sessions".into()
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    #[serde(alias = "npm")]
    pub api: Option<String>,
    #[serde(rename = "name")]
    pub _name: Option<String>,
    #[serde(default)]
    pub options: ConnectionOptions,
    #[serde(default)]
    pub models: BTreeMap<String, ModelConfig>,
}
#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConnectionOptions {
    #[serde(rename = "baseURL")]
    pub base_url: Option<String>,
    #[serde(rename = "apiKey")]
    pub api_key: Option<String>,
    pub timeout: Option<Value>,
    #[serde(rename = "headerTimeout")]
    pub header_timeout_ms: Option<u64>,
    #[serde(rename = "chunkTimeout")]
    pub chunk_timeout_ms: Option<u64>,
    pub requests: yourai_harness::model::RequestPolicy,
    pub headers: BTreeMap<String, String>,
}
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    pub id: Option<String>,
    #[serde(rename = "name")]
    pub _name: Option<String>,
    #[serde(default)]
    pub limit: Limits,
    #[serde(default)]
    pub options: Map<String, Value>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub variants: BTreeMap<String, Map<String, Value>>,
    /// Optional pricing: dollars per million tokens.
    #[serde(default)]
    pub pricing: Pricing,
}
/// Per-model pricing for cost display. Prices are in USD per million tokens.
#[derive(Default, Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Pricing {
    pub input: Option<f64>,
    pub output: Option<f64>,
}
#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub context: Option<u64>,
    pub input: Option<u64>,
    pub output: Option<u32>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path).map_err(|_| {
            format!(
                "Cannot read config at {}; copy yourai.example.json to yourai.json",
                path.display()
            )
        })?;
        // Never include source or serde values in errors: configuration can contain credentials.
        let raw: Value = serde_json::from_str(&text)
            .map_err(|_| "Invalid JSON config; see yourai.example.json")?;
        if let Some(context) = raw.get("context") {
            for key in ["context_window", "input_limit", "output_reserve"] {
                if context.get(key).is_some() {
                    return Err(format!("context.{key} moved to provider models: use limit.context/input/output and options.maxOutputTokens").into());
                }
            }
        }
        let mut config: Self = serde_json::from_value(raw)
            .map_err(|_| "Invalid config fields; see yourai.example.json")?;
        let base = path.canonicalize()?.parent().unwrap().to_owned();
        if config.session_dir.is_relative() {
            config.session_dir = base.join(&config.session_dir);
        }
        for file in [
            &mut config.prompt.soul_file,
            &mut config.prompt.memory_file,
            &mut config.prompt.profile_file,
        ]
        .into_iter()
        .flatten()
        {
            if file.is_relative() {
                *file = base.join(&*file);
            }
        }
        Ok(config)
    }
    fn selected_provider(&self) -> Result<&ProviderConfig, Error> {
        let (provider, _) = self
            .model
            .split_once('/')
            .ok_or("model must be provider/model")?;
        self.provider.get(provider).ok_or("Unknown provider".into())
    }
    /// Returns the pricing for the currently selected model, if configured.
    pub fn pricing(&self) -> Pricing {
        let Ok(provider) = self.selected_provider() else {
            return Pricing::default();
        };
        let (_, model_key) = match self.model.split_once('/') {
            Some(pair) => pair,
            None => return Pricing::default(),
        };
        let model = provider.models.get(model_key);
        model.map(|m| m.pricing.clone()).unwrap_or_default()
    }
    pub fn request_policy(&self) -> Result<yourai_harness::model::RequestPolicy, Error> {
        let policy = self.selected_provider()?.options.requests.clone();
        policy.validate()?;
        Ok(policy)
    }
    pub fn model_timeouts(&self) -> Result<(Option<Duration>, Option<Duration>), Error> {
        let options = &self.selected_provider()?.options;
        let parse = |name: &str, ms: Option<u64>| -> Result<Option<Duration>, Error> {
            ms.map(|n| {
                (n > 0)
                    .then_some(Duration::from_millis(n))
                    .ok_or_else(|| format!("{name} must be positive milliseconds").into())
            })
            .transpose()
        };
        Ok((
            parse("headerTimeout", options.header_timeout_ms)?,
            parse("chunkTimeout", options.chunk_timeout_ms)?,
        ))
    }
    pub fn resolve(&mut self, variant: Option<&str>) -> Result<Arc<GenaiModel>, Error> {
        self.request_policy()?;
        if self.max_model_calls == Some(0) {
            return Err("max_model_calls must be positive".into());
        }
        let (provider_id, model_key) = self
            .model
            .split_once('/')
            .ok_or("model must be provider_id/model_id")?;
        let provider = self
            .provider
            .get(provider_id)
            .ok_or("Unknown provider in model selection")?;
        let defaults = ModelConfig::default();
        let model = provider.models.get(model_key).unwrap_or(&defaults);
        let name = model.id.as_deref().unwrap_or(model_key);
        if name.trim().is_empty() {
            return Err("Model id is empty".into());
        }
        let mut options = Value::Object(model.options.clone());
        if let Some(variant) = variant {
            let selected = model.variants.get(variant).ok_or("Unknown model variant")?;
            if selected.get("disabled") == Some(&Value::Bool(true)) {
                return Err("Model variant is disabled".into());
            }
            let mut selected = selected.clone();
            selected.remove("disabled");
            merge(&mut options, Value::Object(selected));
        }
        let mut options = options.as_object().unwrap().clone();
        let requested = options
            .remove("maxOutputTokens")
            .map(|v| {
                v.as_u64()
                    .filter(|v| *v > 0 && *v <= u32::MAX as u64)
                    .ok_or("maxOutputTokens must be a positive integer")
            })
            .transpose()?;
        if model.limit.output == Some(0)
            || model.limit.context == Some(0)
            || model.limit.input == Some(0)
        {
            return Err("Model limits must be positive".into());
        }
        let output =
            requested.unwrap_or(u64::from(model.limit.output.unwrap_or(32_000).min(32_000)));
        if model
            .limit
            .output
            .is_some_and(|limit| output > u64::from(limit))
        {
            return Err("maxOutputTokens exceeds model limit.output".into());
        }
        self.context.context_window = model.limit.context;
        self.context.input_limit = model.limit.input;
        self.context.output_reserve = output;
        self.context.validate()?;
        let mut chat = ChatOptions::default().with_max_tokens(output as u32);
        if let Some(v) = options.remove("temperature") {
            chat.temperature = Some(
                v.as_f64()
                    .filter(|n| n.is_finite() && *n >= 0.0)
                    .ok_or("Invalid temperature")?,
            );
        }
        if let Some(v) = options.remove("topP") {
            chat.top_p = Some(
                v.as_f64()
                    .filter(|n| (0.0..=1.0).contains(n))
                    .ok_or("Invalid topP")?,
            );
        }
        if let Some(v) = options.remove("reasoningEffort") {
            use genai::chat::ReasoningEffort;
            chat.reasoning_effort = Some(match v.as_str() {
                Some("none") => ReasoningEffort::None,
                Some("minimal") => ReasoningEffort::Minimal,
                Some("low") => ReasoningEffort::Low,
                Some("medium") => ReasoningEffort::Medium,
                Some("high") => ReasoningEffort::High,
                Some("xhigh") => ReasoningEffort::XHigh,
                Some("max") => ReasoningEffort::Max,
                _ => return Err("Invalid reasoningEffort".into()),
            });
        }
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
        let client = provider.client(provider_id, chat)?;
        Ok(Arc::new(
            GenaiModel::new(client, name).with_headers(headers.into()),
        ))
    }
}
/// Resolves the XDG base directory for configuration.
///
/// Returns `$XDG_CONFIG_HOME` when it is set and absolute, otherwise
/// `$HOME/.config`. Per the XDG Base Directory Specification, a relative or
/// empty `XDG_CONFIG_HOME` is ignored in favour of the home fallback. A
/// missing or non-absolute home with no usable `XDG_CONFIG_HOME` is an error.
fn config_dir(xdg_config_home: Option<&Path>, home: Option<&Path>) -> Result<PathBuf, Error> {
    if let Some(dir) = xdg_config_home.filter(|dir| dir.is_absolute()) {
        return Ok(dir.to_path_buf());
    }
    let home = home
        .filter(|home| home.is_absolute())
        .ok_or("Cannot locate config directory: set XDG_CONFIG_HOME or HOME")?;
    Ok(home.join(".config"))
}

/// Returns the default config file path per the XDG Base Directory
/// Specification: `$XDG_CONFIG_HOME/yourai/yourai.json`, or
/// `$HOME/.config/yourai/yourai.json` when `XDG_CONFIG_HOME` is unset or
/// relative. Override the location with `--config`.
pub fn default_config_path() -> Result<PathBuf, Error> {
    Ok(config_dir(
        std::env::var_os("XDG_CONFIG_HOME")
            .as_deref()
            .map(Path::new),
        std::env::var_os("HOME").as_deref().map(Path::new),
    )?
    .join("yourai")
    .join("yourai.json"))
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
impl ProviderConfig {
    fn client(&self, provider_id: &str, chat: ChatOptions) -> Result<Client, Error> {
        let adapter = match self.api.as_deref().unwrap_or(match provider_id {
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
            _ => return Err("Unsupported provider api; see docs/model-config.md".into()),
        };
        if chat.extra_body.is_some()
            && !matches!(adapter, AdapterKind::OpenAI | AdapterKind::OpenAIResp)
        {
            return Err("Extra model options are currently supported only for OpenAI-compatible/Responses adapters".into());
        }
        let mut builder = Client::builder()
            .with_adapter_kind(adapter)
            .with_chat_options(chat);
        if let Some(key) = &self.options.api_key {
            let key = expand(key)?;
            if key.trim().is_empty() {
                return Err("API key is empty".into());
            }
            builder = builder
                .with_auth_resolver_fn(move |_| Ok(Some(AuthData::from_single(key.clone()))));
        }
        if let Some(base) = &self.options.base_url {
            let mut url = url::Url::parse(&expand(base)?).map_err(|_| "Invalid baseURL")?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(
                    "baseURL must be HTTP(S), without credentials, query or fragment".into(),
                );
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
        if let Some(timeout) = &self.options.timeout {
            let duration = if timeout == &Value::Bool(false) {
                None
            } else {
                Some(Duration::from_millis(
                    timeout
                        .as_u64()
                        .filter(|n| *n > 0)
                        .ok_or("timeout must be positive milliseconds or false")?,
                ))
            };
            builder = builder.with_web_config(genai::WebConfig {
                timeout: duration,
                ..Default::default()
            });
        }
        Ok(builder.build())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn config() -> Config {
        serde_json::from_value(json!({
            "model": "gateway/alias/with/slash",
            "provider": {
                "gateway": {"npm": "@ai-sdk/openai-compatible", "options": {"baseURL": "http://localhost:8080/v1", "apiKey": "test-only"},
                    "models": {"alias/with/slash": {"id": "actual-api-model", "limit": {"context": 64000, "output": 16000}, "options": {"maxOutputTokens": 8000}, "variants": {"short": {"maxOutputTokens": 4000, "reasoningEffort": "high"}, "off": {"disabled": true}}}}},
                "unused": {"npm": "@ai-sdk/anthropic", "options": {"apiKey": "{env:YOURAI_UNUSED_PROVIDER_MISSING}"}, "models": {}}
            },
            "soul_file": "./SOUL.md", "yolo": true
        })).unwrap()
    }
    #[tokio::test]
    async fn minimal_gateway_config_uses_model_name_and_default_prompt() {
        use yourai_core::model::ModelProvider;
        let mut config: Config = serde_json::from_value(json!({
            "model":"gateway/glm",
            "provider":{"gateway":{"options":{"baseURL":"http://localhost:8080/v1","apiKey":"test-only"}}}
        })).unwrap();
        assert!(config.system_prompt.is_none());
        assert_eq!(config.session_dir, PathBuf::from(".yourai/sessions"));
        assert_eq!(config.resolve(None).unwrap().model_iden(), "glm");
        let client = config.provider["gateway"]
            .client("gateway", ChatOptions::default())
            .unwrap();
        let target = client
            .resolve_service_target(genai::ModelIden::new(AdapterKind::OpenAI, "glm"))
            .await
            .unwrap();
        assert_eq!(target.model.adapter_kind, AdapterKind::OpenAI);
        assert_eq!(target.endpoint.base_url(), "http://localhost:8080/v1/");
        let explicit: ProviderConfig =
            serde_json::from_value(json!({"api":"anthropic","options":{"apiKey":"test-only"}}))
                .unwrap();
        assert!(explicit.client("custom", ChatOptions::default()).is_ok());
        assert!(serde_json::from_value::<ProviderConfig>(
            json!({"api":"anthropic","npm":"@ai-sdk/openai"})
        )
        .is_err());
    }
    #[test]
    fn selects_model_and_variant_without_resolving_unused_credentials() {
        use yourai_core::model::ModelProvider;
        let mut config = config();
        assert_eq!(
            config.resolve(Some("short")).unwrap().model_iden(),
            "actual-api-model"
        );
        assert_eq!(config.context.context_window, Some(64000));
        assert_eq!(config.context.output_reserve, 4000);
        assert!(config.resolve(Some("off")).is_err());
        assert!(config.resolve(Some("missing")).is_err());
        config.model = "absent/model".into();
        assert!(config.resolve(None).is_err());
    }
    #[test]
    fn validates_output_and_protects_runtime_fields() {
        let mut config = config();
        config
            .provider
            .get_mut("gateway")
            .unwrap()
            .models
            .get_mut("alias/with/slash")
            .unwrap()
            .options
            .insert("maxOutputTokens".into(), json!(20000));
        assert!(config.resolve(None).is_err());
        let mut config = self::config();
        config
            .provider
            .get_mut("gateway")
            .unwrap()
            .models
            .get_mut("alias/with/slash")
            .unwrap()
            .options
            .insert("messages".into(), json!([]));
        assert!(config.resolve(None).is_err());
    }
    #[tokio::test]
    async fn endpoint_and_protocol_are_explicit() {
        let config = config();
        let client = config.provider["gateway"]
            .client("gateway", ChatOptions::default())
            .unwrap();
        let target = client
            .resolve_service_target(genai::ModelIden::new(
                AdapterKind::OpenAI,
                "actual-api-model",
            ))
            .await
            .unwrap();
        assert_eq!(target.endpoint.base_url(), "http://localhost:8080/v1/");
        assert_eq!(target.model.adapter_kind, AdapterKind::OpenAI);
    }
    #[test]
    fn json_example_and_prompt_fields_parse_and_unknown_fields_fail() {
        let config = config();
        assert!(config.yolo);
        assert_eq!(config.prompt.soul_file, Some(PathBuf::from("./SOUL.md")));
        let mut example: Value =
            serde_json::from_str(include_str!("../../../yourai.example.json")).unwrap();
        assert!(serde_json::from_value::<Config>(example.clone()).is_ok());
        example["typo"] = json!(true);
        assert!(serde_json::from_value::<Config>(example).is_err());
    }
    #[test]
    fn config_dir_prefers_absolute_xdg_config_home() {
        let dir = config_dir(Some(Path::new("/tmp/xdg")), Some(Path::new("/home/u"))).unwrap();
        assert_eq!(dir, PathBuf::from("/tmp/xdg"));
    }
    #[test]
    fn config_dir_ignores_relative_xdg_config_home() {
        let dir = config_dir(Some(Path::new("rel")), Some(Path::new("/home/u"))).unwrap();
        assert_eq!(dir, PathBuf::from("/home/u/.config"));
    }
    #[test]
    fn config_dir_falls_back_to_home() {
        let dir = config_dir(None, Some(Path::new("/home/u"))).unwrap();
        assert_eq!(dir, PathBuf::from("/home/u/.config"));
    }
    #[test]
    fn config_dir_errors_without_home_or_xdg() {
        assert!(config_dir(None, None).is_err());
    }
}
