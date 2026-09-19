use genai::{
    adapter::AdapterKind,
    resolver::{AuthData, Endpoint},
    Client,
};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use yourai_runtime::GenaiModel;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub model: ModelConfig,
    #[serde(default)]
    pub extensions: bool,
    #[serde(default)]
    pub context: yourai_core::prelude::ContextPolicy,
    #[serde(default = "sessions")]
    pub session_dir: PathBuf,
    #[serde(default)]
    pub system_prompt: Option<String>,
}
fn sessions() -> PathBuf {
    ".yourai/sessions".into()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    pub provider: String,
    pub name: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub api_key: Option<String>,
}
impl Config {
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            format!(
                "Cannot read {}: {e}. Copy yourai.example.toml to yourai.toml first.",
                path.display()
            )
        })?;
        // Do not echo TOML source: it may contain a literal API key.
        let mut config: Self = toml::from_str(&text)
            .map_err(|_| "Invalid config TOML or unknown field; see yourai.example.toml")?;
        config.context.validate()?;
        if config.session_dir.is_relative() {
            config.session_dir = path
                .canonicalize()?
                .parent()
                .unwrap_or(Path::new("."))
                .join(&config.session_dir);
        }
        Ok(config)
    }
    pub fn model(&self) -> Result<Arc<GenaiModel>, Error> {
        Ok(Arc::new(GenaiModel::new(
            self.model.client()?,
            self.model.name.clone(),
        )))
    }
}
impl ModelConfig {
    pub fn client(&self) -> Result<Client, Error> {
        if self.name.trim().is_empty() {
            return Err("model.name is empty".into());
        }
        let adapter = match self.provider.as_str() {
            "openai" => AdapterKind::OpenAI,
            "openai-responses" => AdapterKind::OpenAIResp,
            "anthropic" => AdapterKind::Anthropic,
            "gemini" => AdapterKind::Gemini,
            "ollama" => AdapterKind::Ollama,
            _ => {
                return Err(
                    "provider must be openai, openai-responses, anthropic, gemini or ollama".into(),
                )
            }
        };
        if self.api_key.is_some() && self.api_key_env.is_some() {
            return Err("Set only api_key_env or api_key".into());
        }
        let key = if let Some(env) = &self.api_key_env {
            Some(std::env::var(env).map_err(|_| format!("Missing environment variable: {env}"))?)
        } else {
            self.api_key.clone()
        };
        if key.as_ref().is_some_and(|k| k.trim().is_empty()) {
            return Err("API key is empty".into());
        }
        let endpoint = self.base_url.as_ref().map(|s| {
            let mut url = url::Url::parse(s).map_err(|_| "Invalid model.base_url")?;
            if !matches!(url.scheme(),"http"|"https") || url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
                return Err("base_url must be an HTTP(S) base URL without credentials, query or fragment");
            }
            if !url.path().ends_with('/') { url.set_path(&format!("{}/",url.path())); }
            Ok(Endpoint::from_owned(url.to_string()))
        }).transpose()?;
        let mut builder = Client::builder().with_adapter_kind(adapter);
        if let Some(key) = key {
            builder = builder
                .with_auth_resolver_fn(move |_| Ok(Some(AuthData::from_single(key.clone()))));
        }
        if let Some(endpoint) = endpoint {
            builder =
                builder.with_service_target_resolver_fn(move |mut target: genai::ServiceTarget| {
                    target.endpoint = endpoint.clone();
                    Ok(target)
                });
        }
        Ok(builder.build())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn custom_model_routes_to_explicit_adapter_and_normalized_endpoint() {
        let config:Config=toml::from_str("[model]\nprovider='openai'\nname='custom-model'\nbase_url='http://localhost:8080/v1'\napi_key='test-only'").unwrap();
        let client = config.model.client().unwrap();
        let target = client
            .resolve_service_target(genai::ModelIden::new(AdapterKind::OpenAI, "custom-model"))
            .await
            .unwrap();
        assert_eq!(target.endpoint.base_url(), "http://localhost:8080/v1/");
        assert_eq!(target.model.adapter_kind, AdapterKind::OpenAI);
    }
}
