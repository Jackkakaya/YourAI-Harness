use crate::config::{Config, Error};
use crate::ui::state::{ModelInfo, SessionView, View};
use std::sync::Arc;
use yourai_core::prelude::*;
use yourai_harness::{Harness, HarnessConfig};

pub(in crate::ui) async fn restore_history(h: &Harness, v: &mut View) -> Result<(), YourAiError> {
    let id = h.host.context().id;
    let mut after = 0;
    loop {
        let page = h
            .sessions
            .read_messages(
                &id,
                MessageQuery {
                    after,
                    active_only: false,
                    limit: 256,
                },
            )
            .await?;
        // Include compacted originals, but not generated summaries/API sidecars.
        v.restore(page.messages);
        match page.next {
            Some(next) if next > after => after = next,
            _ => break,
        }
    }
    let usage = h.usage.session_usage(&id).await?;
    v.restore_usage(
        Usage {
            input_tokens: usage.total_input_tokens,
            output_tokens: usage.total_output_tokens,
            total_tokens: usage.total_tokens,
        },
        usage.request_count,
    );
    match restore_title(h).await {
        Ok(title) => v.session.title = title,
        Err(e) => v.notice(Level::Warning, format!("Session title unavailable: {e}")),
    }
    v.settle();
    v.follow();
    Ok(())
}
/// One title policy for startup and live sessions. Only committed user messages
/// are candidates; a failed write leaves the caller free to retry next poll.
pub(super) async fn restore_title(h: &Harness) -> Result<Option<String>, YourAiError> {
    let id = h.host.context().id;
    let mut meta = h.sessions.load_session(&id).await?;
    if let Some(title) = meta
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        return Ok(Some(title.to_owned()));
    }
    let mut after = 0;
    loop {
        let page = h
            .sessions
            .read_messages(
                &id,
                MessageQuery {
                    after,
                    active_only: false,
                    limit: 256,
                },
            )
            .await?;
        for row in page.messages {
            if row.summary || row.runtime_context || row.message.role != ChatRole::User {
                continue;
            }
            if let Some(title) = derive_title(&row.message.content.texts().join("\n")) {
                meta.title = Some(title.clone());
                h.sessions.save_session(&meta).await?;
                return Ok(Some(title));
            }
        }
        match page.next {
            Some(next) if next > after => after = next,
            _ => return Ok(None),
        }
    }
}
/// Title from the first line of a prompt: whitespace-collapsed, truncated.
fn derive_title(text: &str) -> Option<String> {
    let mut title = text
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() {
        return None;
    }
    if title.chars().count() > 48 {
        title = title.chars().take(47).collect();
        title.push('…');
    }
    Some(title)
}
pub(super) async fn switch_model(
    config: &Arc<std::sync::Mutex<Config>>,
    harness: &Harness,
    model_id: &str,
    variant: Option<String>,
    effort: Option<crate::models::EffortChoice>,
) -> Result<ModelInfo, String> {
    let mut candidate = config.lock().map_err(|e| e.to_string())?.clone();
    candidate.model = model_id.to_string();
    // Resolve the in-memory override before switching the main model and
    // compaction model. Already-bound hooks and child factories retain their
    // models; a newly opened session uses this selection. Disk is untouched.
    if let Some(effort) = &effort {
        candidate
            .set_entry_effort(model_id, variant.as_deref(), effort)
            .map_err(|e| e.to_string())?;
    }
    let crate::config::ResolvedModel { model, settings } = candidate
        .resolve(variant.as_deref())
        .map_err(|e| e.to_string())?;
    candidate.selected_variant = variant.clone();
    harness
        .switch_model_with_settings(model, candidate.context.clone(), settings)
        .await
        .map_err(|e| e.to_string())?;
    let effective_effort = candidate.effective_effort(model_id, variant.as_deref());
    let p = candidate.pricing();
    *config.lock().map_err(|e| e.to_string())? = candidate;
    let pricing = match (p.input, p.output) {
        (Some(i), Some(o)) => Some((i, o)),
        _ => None,
    };
    let label = if let Some(v) = &variant {
        format!("{model_id} · {v}")
    } else {
        model_id.to_string()
    };
    Ok(ModelInfo {
        label,
        pricing,
        effort: effective_effort,
    })
}

/// Rebuild model-dependent settings from the structured runtime selection.
fn prepare_session(
    config: &Arc<std::sync::Mutex<Config>>,
    template: &HarnessConfig,
    id: Option<SessionId>,
) -> Result<(HarnessConfig, Arc<dyn ModelProvider>), Error> {
    let candidate = config
        .lock()
        .map_err(|e| Error::from(e.to_string()))?
        .clone();
    let variant = candidate.selected_variant.clone();
    let selection = candidate.resolve(variant.as_deref())?;
    let mut hc = template.clone();
    hc.resume = id;
    selection.apply_to(&mut hc);
    Ok((hc, selection.model))
}

/// Open the target session and restore its history into a fresh view.
/// Runs entirely before the old session is stopped, so any failure leaves
/// the current session and its UI intact. Only conversation state is returned;
/// application state is never replaced.
pub(super) async fn open_session(
    config: &Arc<std::sync::Mutex<Config>>,
    template: &HarnessConfig,
    yolo: bool,
    id: Option<SessionId>,
) -> Result<(Harness, SessionView), (Level, String)> {
    let error = |message: String| (Level::Error, message);
    let (mut hc, model) = prepare_session(config, template, id)
        .map_err(|e| error(format!("Could not open session: {e}")))?;
    hc.yolo = yolo;
    let new_h = Harness::open(hc, model)
        .await
        .map_err(|e| error(format!("Could not open session: {e}")))?;
    let mut fresh = View::default();
    if let Err(e) = restore_history(&new_h, &mut fresh).await {
        let _ = new_h.close().await;
        return Err(error(format!("Could not restore history: {e}")));
    }
    Ok((new_h, fresh.session))
}

#[cfg(test)]
mod switch_tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;

    #[tokio::test]
    async fn failed_switch_is_atomic_and_session_rebuild_keeps_variant() {
        let dir = tempfile::tempdir().unwrap();
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "model": "mock/large",
            "provider": { "mock": {
                "npm": "@ai-sdk/openai-compatible",
                "options": {"baseURL": "http://127.0.0.1:1/v1", "apiKey": "test"},
                "models": {
                    "large": {"limit": {"context": 64000, "output": 4096}},
                    "small": {"limit": {"context": 32000, "output": 4096},
                        "variants": {"short": {"maxOutputTokens": 2048},
                            "invalid": {"temperature": -1}}}
                }
            }}
        }))
        .unwrap();
        let crate::config::ResolvedModel { model, .. } = cfg.resolve(None).unwrap();
        let mut hc = HarnessConfig::new(dir.path().join("sessions"), dir.path().into());
        hc.system_prompt = Some("test".into());
        hc.context_policy = cfg.context.clone();
        let h = Harness::open(hc.clone(), model).await.unwrap();
        let config = Arc::new(std::sync::Mutex::new(cfg));
        let mut view = View::default();
        view.model.label = "mock/large".into();
        for (id, variant) in [
            ("missing/model", None),
            ("mock/small", Some("invalid")),
            ("mock/small", Some("missing")),
        ] {
            assert!(
                switch_model(&config, &h, id, variant.map(str::to_owned), None)
                    .await
                    .is_err()
            );
            let cfg = config.lock().unwrap();
            assert_eq!(cfg.model, "mock/large");
            assert_eq!(
                cfg.resolve(None)
                    .unwrap()
                    .model
                    .token_budget()
                    .limits()
                    .context,
                Some(64000)
            );
            assert_eq!(cfg.selected_variant, None);
            assert_eq!(view.model.label, "mock/large");
            assert_eq!(h.host.context_usage().unwrap().context_window, Some(64000));
        }
        let selected = switch_model(&config, &h, "mock/small", Some("short".into()), None)
            .await
            .unwrap();
        view.model.label = selected.label;
        assert_eq!(h.host.context_usage().unwrap().output_reserve, 2048);
        let (_resume, resume_model) =
            prepare_session(&config, &hc, Some(h.host.context().id)).unwrap();
        assert_eq!(resume_model.token_budget().limits().context, Some(32000));
        // This differs from the default model's 4096, proving variant resolution.
        assert_eq!(resume_model.token_budget().max_output_tokens(), 2048);
        assert_eq!(
            config.lock().unwrap().selected_variant.as_deref(),
            Some("short")
        );
        assert_eq!(view.model.label, "mock/small · short");
        h.close().await.unwrap();
    }

    #[tokio::test]
    async fn effort_override_sets_clears_and_reports_effective() {
        let dir = tempfile::tempdir().unwrap();
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "model": "mock/large",
            "provider": { "mock": {
                "npm": "@ai-sdk/openai-compatible",
                "options": {"baseURL": "http://127.0.0.1:1/v1", "apiKey": "test"},
                "models": {
                    "large": {"limit": {"context": 64000, "output": 4096},
                        "options": {"reasoningEffort": "low"}},
                    "small": {"limit": {"context": 32000, "output": 4096},
                        "variants": {"short": {"maxOutputTokens": 2048}}}
                }
            }}
        }))
        .unwrap();
        let crate::config::ResolvedModel { model, .. } = cfg.resolve(None).unwrap();
        let mut hc = HarnessConfig::new(dir.path().join("sessions"), dir.path().into());
        hc.system_prompt = Some("test".into());
        hc.context_policy = cfg.context.clone();
        let h = Harness::open(hc, model).await.unwrap();
        let config = Arc::new(std::sync::Mutex::new(cfg));

        // Set: writes the variant's options and reports the effective effort.
        let selected = switch_model(
            &config,
            &h,
            "mock/small",
            Some("short".into()),
            Some(crate::models::EffortChoice::Set("high".into())),
        )
        .await
        .unwrap();
        assert_eq!(selected.effort.as_deref(), Some("high"));
        assert_eq!(
            config.lock().unwrap().provider["mock"].models["small"].variants["short"]
                .get("reasoningEffort"),
            Some(&serde_json::json!("high"))
        );
        // The file-configured model-level effort still applies to its entry.
        assert_eq!(
            config
                .lock()
                .unwrap()
                .effective_effort("mock/large", None)
                .as_deref(),
            Some("low")
        );

        // Config: clears the override on that entry (falls back to none here).
        let selected = switch_model(
            &config,
            &h,
            "mock/small",
            Some("short".into()),
            Some(crate::models::EffortChoice::Config),
        )
        .await
        .unwrap();
        assert_eq!(selected.effort, None);
        assert!(
            !config.lock().unwrap().provider["mock"].models["small"].variants["short"]
                .contains_key("reasoningEffort")
        );

        // Invalid keywords never reach the resolve or the shared config.
        assert!(switch_model(
            &config,
            &h,
            "mock/small",
            None,
            Some(crate::models::EffortChoice::Set("nope".into())),
        )
        .await
        .is_err());
        assert!(!config.lock().unwrap().provider["mock"].models["small"]
            .options
            .contains_key("reasoningEffort"));
        h.close().await.unwrap();
    }
}
