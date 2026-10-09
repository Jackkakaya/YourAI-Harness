use std::{sync::Arc, time::Duration};
use yourai_core::prelude::*;

/// Applies model-wide defaults to any provider, including complete calls and child loops.
/// Request options remain explicit overrides. No execution policy is stored in the loop.
pub struct ConfiguredModel {
    inner: Arc<dyn ModelProvider>,
    timeouts: ModelTimeouts,
    budget: ModelTokenBudget,
}
impl ConfiguredModel {
    pub fn new(
        inner: Arc<dyn ModelProvider>,
        budget: ModelTokenBudget,
        timeouts: ModelTimeouts,
    ) -> Result<Arc<Self>, YourAiError> {
        let existing = inner.token_budget();
        existing.validate_output(budget.max_output_tokens())?;
        let old = existing.limits();
        let new = budget.limits();
        for (declared, bound) in [
            (new.context, old.context),
            (new.input, old.input),
            (new.output.map(u64::from), old.output.map(u64::from)),
        ] {
            if bound.is_some_and(|bound| declared.is_none_or(|n| n > bound)) {
                return Err(ErrorKind::Config(
                    "model configuration cannot broaden an existing capacity".into(),
                )
                .into());
            }
        }
        if timeouts.headers.is_zero() || timeouts.read.is_zero() {
            return Err(ErrorKind::Config("model timeouts must be positive".into()).into());
        }
        Ok(Arc::new(Self {
            inner,
            budget,
            timeouts,
        }))
    }
    fn request(&self, mut request: ModelRequest) -> Result<ModelRequest, YourAiError> {
        let output = *request
            .options
            .max_tokens
            .get_or_insert(self.budget.max_output_tokens());
        self.budget.validate_output(output)?;
        self.timeouts.apply(&mut request.options);
        Ok(request)
    }
}
impl ModelProvider for ConfiguredModel {
    fn token_budget(&self) -> ModelTokenBudget {
        self.budget
    }
    fn timeouts(&self) -> ModelTimeouts {
        self.timeouts
    }
    fn uses_transport_timeouts(&self) -> bool {
        self.inner.uses_transport_timeouts()
    }
    fn retry_after(&self, error: &YourAiError) -> Option<Duration> {
        self.inner.retry_after(error)
    }
    fn classify_error(&self, error: &YourAiError) -> ModelErrorClass {
        self.inner.classify_error(error)
    }
    fn recovery(&self, error: &YourAiError) -> ModelRecovery {
        self.inner.recovery(error)
    }
    fn model_iden(&self) -> &str {
        self.inner.model_iden()
    }
    fn media_tokens(&self, part: &ContentPart) -> Result<u64, YourAiError> {
        self.inner.media_tokens(part)
    }
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
        Box::pin(async move { self.inner.complete(self.request(request)?).await })
    }
    fn stream_events<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
        Box::pin(async move { self.inner.stream_events(self.request(request)?).await })
    }
}
