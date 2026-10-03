//! Model selection: builds the candidate list from config and resolves switches.
use crate::config::Config;
use serde_json::Value;

/// Selectable reasoning-effort levels in the `/models` effort sub-picker.
/// `None` means "no explicit effort" (remove any runtime override; the
/// merged config options decide).
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

/// Runtime thinking-effort selection for one `/models` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffortChoice {
    /// Keep what the (merged) config options specify; removes any runtime
    /// override previously applied through the picker.
    Config,
    /// Set the effort keyword for this entry.
    Set(String),
}

/// Enumerate all selectable model entries from the config.
/// Each model always gets one default (no-variant) entry plus one per non-disabled variant.
pub fn model_choices(config: &Config) -> Vec<ModelChoice> {
    let mut choices = Vec::new();
    for (provider_id, provider) in &config.provider {
        for (model_key, model) in &provider.models {
            let id = format!("{provider_id}/{model_key}");
            // Default entry (no variant).
            choices.push(ModelChoice {
                id: id.clone(),
                variant: None,
                label: id.clone(),
                effort: config.effective_effort(&id, None),
            });
            // Variant entries.
            for (variant_name, variant_cfg) in &model.variants {
                if variant_cfg.get("disabled") == Some(&Value::Bool(true)) {
                    continue;
                }
                choices.push(ModelChoice {
                    id: id.clone(),
                    variant: Some(variant_name.clone()),
                    label: format!("{id} · {variant_name}"),
                    effort: config.effective_effort(&id, Some(variant_name)),
                });
            }
        }
    }
    if !choices
        .iter()
        .any(|choice| choice.id == config.model && choice.variant.is_none())
    {
        choices.insert(
            0,
            ModelChoice {
                id: config.model.clone(),
                variant: None,
                label: config.model.clone(),
                effort: None,
            },
        );
    }
    choices
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;
    use crate::config::Config;

    fn parse_config(json_str: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, json_str).unwrap();
        let config = Config::load(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        config
    }

    #[test]
    fn model_choices_includes_variants_and_filters_disabled() {
        let config_str = r#"{
            "model": "gateway/kimi-k3",
            "provider": {
                "gateway": {
                    "npm": "@ai-sdk/openai-compatible",
                    "options": {"baseURL": "http://localhost/v1", "apiKey": "test"},
                    "models": {
                        "kimi-k3": {
                            "id": "kimi-k3",
                            "limit": {"context": 128000, "output": 4096},
                            "options": {"maxOutputTokens": 4096},
                            "variants": {
                                "short": {"temperature": 0.3},
                                "disabled-one": {"disabled": true, "temperature": 0.1}
                            }
                        },
                        "glm-4.6": {
                            "id": "glm-4.6",
                            "limit": {"context": 64000, "output": 4096},
                            "options": {"maxOutputTokens": 4096}
                        }
                    }
                },
                "ollama": {
                    "npm": "@ai-sdk/ollama",
                    "options": {"baseURL": "http://localhost:11434"},
                    "models": {
                        "qwen3:32b": {
                            "id": "qwen3:32b",
                            "limit": {"context": 32000, "output": 4096},
                            "options": {"maxOutputTokens": 4096}
                        }
                    }
                }
            },
            "context": {"keep_recent_tokens": 0, "summary_min_savings": 1}
        }"#;
        let config = parse_config(config_str);
        let choices = model_choices(&config);
        // gateway/kimi-k3 (default) + gateway/kimi-k3 · short (disabled-one filtered) +
        // gateway/glm-4.6 (default) + ollama/qwen3:32b (default) = 4
        assert_eq!(choices.len(), 4);
        assert!(choices
            .iter()
            .any(|c| c.id == "gateway/kimi-k3" && c.variant.is_none()));
        assert!(choices
            .iter()
            .any(|c| c.id == "gateway/kimi-k3" && c.variant.as_deref() == Some("short")));
        assert!(!choices
            .iter()
            .any(|c| c.variant.as_deref() == Some("disabled-one")));
        assert!(choices.iter().any(|c| c.id == "gateway/glm-4.6"));
        assert!(choices.iter().any(|c| c.id == "ollama/qwen3:32b"));
        // Labels are correct.
        let short = choices
            .iter()
            .find(|c| c.variant.as_deref() == Some("short"))
            .unwrap();
        assert_eq!(short.label, "gateway/kimi-k3 · short");
    }

    #[test]
    fn model_choices_report_effective_effort_with_variant_precedence() {
        let config_str = r#"{
            "model": "gateway/kimi-k3",
            "provider": {
                "gateway": {
                    "npm": "@ai-sdk/openai-compatible",
                    "options": {"baseURL": "http://localhost/v1", "apiKey": "test"},
                    "models": {
                        "kimi-k3": {
                            "limit": {"context": 128000, "output": 4096},
                            "options": {"reasoningEffort": "low"},
                            "variants": {
                                "deep": {"reasoningEffort": "high"},
                                "same": {"maxOutputTokens": 2048}
                            }
                        },
                        "glm-4.6": {
                            "limit": {"context": 64000, "output": 4096}
                        }
                    }
                }
            },
            "context": {"keep_recent_tokens": 0, "summary_min_savings": 1}
        }"#;
        let config = parse_config(config_str);
        let choices = model_choices(&config);
        let effort_of = |variant: Option<&str>| {
            choices
                .iter()
                .find(|c| c.id == "gateway/kimi-k3" && c.variant.as_deref() == variant)
                .unwrap()
                .effort
                .clone()
        };
        // Model-level effort applies to the default and variant-without-effort
        // entries; a variant's own effort wins.
        assert_eq!(effort_of(None).as_deref(), Some("low"));
        assert_eq!(effort_of(Some("same")).as_deref(), Some("low"));
        assert_eq!(effort_of(Some("deep")).as_deref(), Some("high"));
        // A model without any effort hint reports None.
        assert_eq!(
            choices
                .iter()
                .find(|c| c.id == "gateway/glm-4.6" && c.variant.is_none())
                .unwrap()
                .effort,
            None
        );
    }
}
