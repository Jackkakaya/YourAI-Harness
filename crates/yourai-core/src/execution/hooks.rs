use super::Turn;
use crate::prelude::*;

impl Turn<'_> {
    pub(crate) fn invocation(&self, event: HookEvent) -> HookInvocation {
        let mut base = BaseInput::new(self.history.session_id().as_str(), "");
        if let Some(session) = &self.tc.info.options.session {
            base.cwd = session.cwd.to_string_lossy().into_owned();
            base.transcript_path = session
                .transcript_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
        }
        HookInvocation::new(base, event).with_providers(&self.tc.snap)
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
        let timeout = self
            .tc
            .info
            .options
            .limits
            .hook_timeout
            .or(self.config.hook_timeout);
        let result = self
            .wait_operation(hooks.dispatch(&invocation), timeout, "hook")
            .await?;
        result.validate_for(kind)?;
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
        for message in result.visible_messages() {
            self.notice(
                if message.kind == HookMessageKind::NonBlockingError {
                    Level::Warning
                } else {
                    Level::Info
                },
                &message.content,
            )?;
        }
        result.ensure_continuation()
    }
}
