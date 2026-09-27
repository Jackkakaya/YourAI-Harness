use super::State;
use yourai_core::prelude::*;

pub(crate) fn feedback(result: &HookDispatchResult) -> Vec<String> {
    result
        .common
        .blocking_errors
        .iter()
        .map(|e| e.message.clone())
        .collect()
}
pub(crate) fn additional(result: &HookDispatchResult) -> Vec<String> {
    match &result.outcome {
        HookPointOutcome::UserPromptSubmit(o) => o.additional_contexts.clone(),
        HookPointOutcome::PreToolUse(o) => o.additional_contexts.clone(),
        HookPointOutcome::PostToolUse(o) => o.additional_contexts.clone(),
        HookPointOutcome::Generic(o) => o.additional_contexts.clone(),
        _ => vec![],
    }
}
impl State<'_> {
    pub(crate) fn invocation(&self, event: HookEvent) -> HookInvocation {
        let mut base = BaseInput::new(self.history.session_id().0.as_str(), "");
        if let Some(session) = &self.tc.info.options.session {
            base.cwd = session.cwd.to_string_lossy().into_owned();
            base.transcript_path = session
                .transcript_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
        }
        HookInvocation::new(base, event)
    }
    pub(crate) async fn hook(
        &mut self,
        event: HookEvent,
    ) -> Result<HookDispatchResult, YourAiError> {
        let kind = event.kind();
        let Some(hooks) = self.tc.snap.hooks.clone() else {
            return Ok(HookDispatchResult::empty(kind));
        };
        let invocation = self.invocation(event);
        let timeout = Some(
            self.tc
                .info
                .options
                .limits
                .hook_timeout
                .unwrap_or(self.config.hook_timeout),
        );
        let result = self
            .wait(hooks.dispatch(&invocation), timeout, "hook")
            .await?;
        if result.event != kind
            || result.outcome.event_name() != HookDispatchResult::empty(kind).outcome.event_name()
        {
            return Err(ErrorKind::Loop("HookRuntime returned a mismatched event".into()).into());
        }
        if let Some(obs) = &self.tc.snap.observability {
            for run in &result.runs {
                obs.timing(
                    "loop.hook",
                    run.duration.as_secs_f64(),
                    &[("event", kind.as_str()), ("hook", &run.hook_id)],
                );
            }
        }
        Ok(result)
    }
    pub(crate) fn apply_common(&self, result: &HookDispatchResult) -> Result<(), YourAiError> {
        for text in &result.common.system_messages {
            self.notice(Level::Info, text)?;
        }
        for message in &result.common.messages {
            let suppressed = result
                .runs
                .iter()
                .any(|r| r.hook_id == message.hook_id && r.suppress_output);
            if !suppressed {
                self.notice(
                    if message.kind == HookMessageKind::NonBlockingError {
                        Level::Warning
                    } else {
                        Level::Info
                    },
                    &message.content,
                )?;
            }
        }
        if result.common.prevent_continuation {
            return Err(AbortReason::HookStopped(
                result
                    .common
                    .stop_reason
                    .clone()
                    .unwrap_or_else(|| "hook requested stop".into()),
            )
            .into());
        }
        Ok(())
    }
    pub(crate) async fn stop_failure(&mut self, cause: &YourAiError) {
        // A reporting hook cannot overwrite the model error or block cleanup forever.
        let event = HookEvent::StopFailure {
            error: "model_error".into(),
            error_details: Some(cause.to_string()),
            last_assistant_message: Some(self.output.text.clone()),
        };
        if let Ok(result) = self.hook(event).await {
            let _ = self.apply_common(&result);
        }
    }
}
