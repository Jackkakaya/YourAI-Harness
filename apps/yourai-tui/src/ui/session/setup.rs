use crate::config::{Config, Error};
use crate::ui::state::{derive_title, Item, Role, View};
use std::sync::Arc;
use yourai_core::prelude::*;
use yourai_harness::{Harness, HarnessConfig};

pub(super) use crate::ui::state::ModelInfo as ModelSelection;

pub(super) async fn restore_history(h: &Harness, v: &mut View) -> Result<(), YourAiError> {
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
    // Restore the session title; backfill older sessions from their first prompt.
    if let Ok(mut meta) = h.sessions.load_session(&id).await {
        v.title = meta
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned);
        if v.title.is_none() {
            let first_user = v.items().iter().find_map(|i| match i {
                Item::Text {
                    role: Role::User,
                    text,
                } => Some(text.as_str()),
                _ => None,
            });
            if let Some(title) = first_user.and_then(derive_title) {
                meta.title = Some(title.clone());
                v.title = Some(title);
                if let Err(e) = h.sessions.save_session(&meta).await {
                    v.title = None;
                    v.notice(Level::Warning, format!("Session title not saved: {e}"));
                }
            }
        }
    }
    v.settle();
    v.follow();
    Ok(())
}
/// Resolve a config snapshot into the live model, its context policy and the
/// settings derived from it. The shared resolution behind model switching
/// and session opening; the caller decides which parts to persist.
fn resolve_selection(
    candidate: &Config,
    variant: Option<&str>,
) -> Result<
    (
        Arc<dyn ModelProvider>,
        ContextPolicy,
        yourai_harness::assembly::ModelSettings,
    ),
    Error,
> {
    let (model, context) = candidate.resolve(variant)?;
    let (header_timeout, chunk_timeout) = candidate.model_timeouts()?;
    Ok((
        model,
        context,
        yourai_harness::assembly::ModelSettings {
            provider: candidate.provider_id().into(),
            requests: candidate.request_policy()?,
            header_timeout,
            chunk_timeout,
        },
    ))
}

pub(super) async fn switch_model(
    config: &Arc<std::sync::Mutex<Config>>,
    harness: &Harness,
    model_id: &str,
    variant: Option<String>,
) -> Result<ModelSelection, String> {
    let mut candidate = config.lock().map_err(|e| e.to_string())?.clone();
    candidate.model = model_id.to_string();
    let (model, context, settings) =
        resolve_selection(&candidate, variant.as_deref()).map_err(|e| e.to_string())?;
    candidate.context = context.clone();
    candidate.selected_variant = variant.clone();
    harness
        .switch_model_with_settings(model, context, settings)
        .await
        .map_err(|e| e.to_string())?;
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
    Ok(ModelSelection { label, pricing })
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
    let (model, context, settings) = resolve_selection(&candidate, variant.as_deref())?;
    let mut hc = template.clone();
    hc.resume = id;
    hc.context_policy = context;
    hc.model_provider = settings.provider;
    hc.request_policy = settings.requests;
    (hc.model_header_timeout, hc.model_chunk_timeout) =
        (settings.header_timeout, settings.chunk_timeout);
    Ok((hc, model))
}

/// Open the target session and restore its history into a fresh view.
/// Runs entirely before the old session is stopped, so any failure leaves
/// the current session and its UI intact. The fresh view is plain: the
/// fields that survive a switch are re-read from the live view when the
/// effect is applied.
pub(super) async fn open_session(
    config: &Arc<std::sync::Mutex<Config>>,
    template: &HarnessConfig,
    yolo: bool,
    id: Option<SessionId>,
) -> Result<(Harness, View), (Level, String)> {
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
    Ok((new_h, fresh))
}

#[cfg(test)]
mod switch_tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;

    #[tokio::test]
    async fn failed_switch_is_atomic_and_session_rebuild_keeps_variant() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg: Config = serde_json::from_value(serde_json::json!({
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
        let (model, context) = cfg.resolve(None).unwrap();
        cfg.context = context;
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
            assert!(switch_model(&config, &h, id, variant.map(str::to_owned))
                .await
                .is_err());
            let cfg = config.lock().unwrap();
            assert_eq!(cfg.model, "mock/large");
            assert_eq!(cfg.context.context_window, Some(64000));
            assert_eq!(cfg.selected_variant, None);
            assert_eq!(view.model.label, "mock/large");
            assert_eq!(h.host.context_usage().unwrap().context_window, Some(64000));
        }
        let selected = switch_model(&config, &h, "mock/small", Some("short".into()))
            .await
            .unwrap();
        view.model.label = selected.label;
        assert_eq!(h.host.context_usage().unwrap().output_reserve, 2048);
        let (resume, _) = prepare_session(&config, &hc, Some(h.host.context().id)).unwrap();
        assert_eq!(resume.context_policy.context_window, Some(32000));
        // This differs from the default model's 4096, proving variant resolution.
        assert_eq!(resume.context_policy.output_reserve, 2048);
        assert_eq!(
            config.lock().unwrap().selected_variant.as_deref(),
            Some("short")
        );
        assert_eq!(view.model.label, "mock/small · short");
        h.close().await.unwrap();
    }
}
