use std::{sync::Arc, time::Duration};
use yourai_core::prelude::*;

/// Applies model-wide defaults to any provider, including complete calls and child loops.
/// Request options remain explicit overrides. No execution policy is stored in the loop.
pub struct ConfiguredModel {
    inner: Arc<dyn ModelProvider>,
    timeouts: ModelTimeouts,
}
impl ConfiguredModel {
    pub fn wrap(
        inner: Arc<dyn ModelProvider>,
        headers: Option<Duration>,
        read: Option<Duration>,
    ) -> Result<Arc<dyn ModelProvider>, YourAiError> {
        if headers.into_iter().chain(read).any(|d| d.is_zero()) {
            return Err(ErrorKind::Config("model timeouts must be positive".into()).into());
        }
        if headers.is_none() && read.is_none() {
            return Ok(inner);
        }
        let defaults = inner.timeouts();
        Ok(Arc::new(Self {
            inner,
            timeouts: ModelTimeouts {
                headers: headers.unwrap_or(defaults.headers),
                read: read.unwrap_or(defaults.read),
            },
        }))
    }
    fn request(&self, mut request: ModelRequest) -> ModelRequest {
        self.timeouts.apply(&mut request.options);
        request
    }
}
impl ModelProvider for ConfiguredModel {
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
        self.inner.complete(self.request(request))
    }
    fn stream_events<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
        self.inner.stream_events(self.request(request))
    }
}
