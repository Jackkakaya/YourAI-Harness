//! Model picker display entries. Resolution and switching belong to the backend.
/// Selectable reasoning-effort levels in the `/models` effort sub-picker.
/// `None` means "no explicit effort" (remove any runtime override; the
/// merged config options decide).
pub use yourai_harness::model::{EffortChoice, EFFORT_CHOICES};

use crate::config::Config;
use yourai_harness::model;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelChoice {
    pub id: String,
    pub variant: Option<String>,
    pub label: String,
    pub effort: Option<String>,
}

/// Menu organization belongs to the TUI; availability and parameter rules belong to model.
pub fn model_choices(config: &Config) -> Vec<ModelChoice> {
    let mut choices: Vec<_> = model::list_models(config, true)
        .into_iter()
        .map(|entry| ModelChoice {
            label: match &entry.variant {
                Some(name) => format!("{} · {name}", entry.id),
                None => entry.id.clone(),
            },
            id: entry.id,
            variant: entry.variant,
            effort: entry.effort,
        })
        .collect();
    choices.sort_by(|a, b| (&a.id, &a.variant).cmp(&(&b.id, &b.variant)));
    let current = config.model();
    if !config.model_ids().contains(&current) {
        if let Some(index) = choices.iter().position(|entry| entry.id == current) {
            let entry = choices.remove(index);
            choices.insert(0, entry);
        }
    }
    choices
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;
    use crate::config::Config;
    use serde_json::json;

    #[test]
    fn model_and_variant_order_is_stable_across_json_object_order() {
        let config = parse_config(
            r#"{
            "model":"mock/z", "provider":{"mock":{"models":{
                "z":{}, "a":{"variants":{"z":{},"a":{}}}, "m":{}
            }}}
        }"#,
        );
        let labels: Vec<_> = model_choices(&config)
            .into_iter()
            .map(|choice| choice.label)
            .collect();
        assert_eq!(
            labels,
            ["mock/a", "mock/a · a", "mock/a · z", "mock/m", "mock/z"]
        );
    }

    fn parse_config(json_str: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, json_str).unwrap();
        let config = Config::init(&path).unwrap();
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
    #[test]
    fn empty_or_unresolvable_selections_do_not_create_menu_entries() {
        let cfg = Config::default();
        assert!(model_choices(&cfg).is_empty());
        for id in ["", "malformed", "missing/model"] {
            cfg.set_model(id.into());
            assert!(model_choices(&cfg).is_empty());
            assert!(model::select(&cfg, id, None, None).is_err());
        }
    }
    #[test]
    fn model_menu_and_selection_share_eligibility_without_building_clients() {
        let cfg: Config = serde_json::from_value(json!({
        "model":"mock/implicit",
        "providers":{"mock":{"apiKey":"{env:YOURAI_MENU_QUERY_MUST_NOT_READ_AUTH}","baseURL":"invalid URL"}},
        "models":{
            "":{"provider":"mock"},
            "malformed":{"provider":"mock"},
            "missing/a":{"provider":"missing"},
            "alias/a":{"provider":"mock","options":{"reasoningEffort":"low"},"variants":{
                "fast":{"reasoningEffort":"high"}, "off":{"disabled":true}
            }}
        },
        "effort_overrides":{"alias/a":{"off":"high"},"missing/unused":{}}
    })).unwrap();
        let before = serde_json::to_value(&cfg).unwrap();
        let choices = model_choices(&cfg);
        assert_eq!(serde_json::to_value(&cfg).unwrap(), before);
        assert_eq!(
            choices.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(),
            ["mock/implicit", "alias/a", "alias/a · fast"]
        );
        assert_eq!(choices[1].effort.as_deref(), Some("low"));
        assert_eq!(choices[2].effort.as_deref(), Some("high"));
        // Eligibility must not expand credentials, validate the endpoint, or build a Client.
        assert!(model::resolve(&cfg, &cfg.model(), cfg.variant().as_deref()).is_err());
        for choice in choices {
            model::select(&cfg, &choice.id, choice.variant.clone(), None).unwrap();
            assert_eq!(
                model::list_models(&cfg, false)
                    .into_iter()
                    .find(|entry| entry.id == choice.id && entry.variant == choice.variant)
                    .and_then(|entry| entry.effort),
                choice.effort
            );
        }
        for (id, variant) in [
            ("", None),
            ("malformed", None),
            ("missing/a", None),
            ("alias/a", Some("off")),
            ("alias/a", Some("missing")),
        ] {
            let before = serde_json::to_value(&cfg).unwrap();
            assert!(model::select(&cfg, id, variant.map(str::to_owned), None).is_err());
            assert_eq!(serde_json::to_value(&cfg).unwrap(), before);
        }
        // Inactive overrides on disabled variants remain valid metadata.
        assert!(yourai_harness::assembly::validate_config(&cfg).is_ok());
        cfg.set_variant(Some("off".into()));
        assert!(yourai_harness::assembly::validate_config(&cfg).is_err());
        assert!(model::resolve(&cfg, &cfg.model(), cfg.variant().as_deref()).is_err());
        cfg.set_model("missing/implicit".into());
        assert!(!model_choices(&cfg)
            .iter()
            .any(|c| c.id == "missing/implicit"));
    }

    #[test]
    fn model_menu_projects_one_consistent_configuration_version() {
        use yourai_harness::config::{Config, ModelConfig, ProviderConfig};
        let config = std::sync::Arc::new(Config::default());
        config.set_provider("mock".into(), ProviderConfig::default());
        config.set_model("mock/main".into());
        let versions: Vec<ModelConfig> = ["a", "b"]
            .into_iter()
            .map(|id| {
                serde_json::from_value(serde_json::json!({
                    "provider":"mock","variants":{
                        format!("{id}-fast"):{"reasoningEffort":"high"},
                        format!("{id}-slow"):{"reasoningEffort":"low"}
                    }
                }))
                .unwrap()
            })
            .collect();
        config
            .set_model_config("mock/main".into(), versions[0].clone())
            .unwrap();
        let writer = std::sync::Arc::clone(&config);
        let task = std::thread::spawn(move || {
            for iteration in 0..1000 {
                writer
                    .set_model_config("mock/main".into(), versions[iteration % 2].clone())
                    .unwrap();
            }
        });
        for _ in 0..1000 {
            let choices = model_choices(&config);
            assert_eq!(choices.len(), 3);
            assert!(choices.iter().all(|choice| choice.id == "mock/main"));
            let prefix = choices[1].variant.as_ref().unwrap().chars().next().unwrap();
            assert_eq!(
                choices[2].variant.as_ref().unwrap().chars().next().unwrap(),
                prefix
            );
            assert_eq!(choices[1].effort.as_deref(), Some("high"));
            assert_eq!(choices[2].effort.as_deref(), Some("low"));
        }
        task.join().unwrap();
    }
}
