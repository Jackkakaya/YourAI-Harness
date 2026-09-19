mod control;
mod metrics;
pub use control::RequestPolicy;
use futures_util::StreamExt;
use metrics::Attempt;
pub use metrics::RequestMetrics;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use yourai_core::prelude::*;
pub fn usage(u: &GenaiUsage) -> Usage {
    let input = u.prompt_tokens.unwrap_or(0).max(0) as u64;
    let output = u.completion_tokens.unwrap_or(0).max(0) as u64;
    Usage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: u
            .total_tokens
            .map(|n| n.max(0) as u64)
            .unwrap_or(input + output),
    }
}
/// Real network adapter. Client configuration controls endpoints and credentials.
pub struct GenaiModel {
    client: genai::Client,
    model: String,
    headers: genai::Headers,
}
impl GenaiModel {
    pub fn new(client: genai::Client, model: impl Into<String>) -> Self {
        Self {
            client,
            model: model.into(),
            headers: genai::Headers::default(),
        }
    }
    /// Defaults applied to every request, including compaction and child agents.
    pub fn with_headers(mut self, headers: genai::Headers) -> Self {
        self.headers = headers;
        self
    }
    fn request(&self, mut request: ModelRequest) -> ModelRequest {
        let mut headers = self.headers.clone();
        if let Some(overrides) = &request.options.extra_headers {
            headers.merge_with(overrides);
        }
        request.options.extra_headers = Some(headers);
        request
    }
}
impl ModelProvider for GenaiModel {
    fn retry_after(&self, error: &YourAiError) -> Option<std::time::Duration> {
        control::retry_after(error)
    }
    fn complete<'a>(&'a self, r: ModelRequest) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
        let r = self.request(r);
        Box::pin(async move {
            self.client
                .exec_chat(&self.model, r.request, Some(&r.options))
                .await
                .map_err(|source| ErrorKind::Model { source }.into())
        })
    }
    fn stream_events<'a>(
        &'a self,
        r: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
        let r = self.request(r);
        Box::pin(async move {
            let response = self
                .client
                .exec_chat_stream(&self.model, r.request, Some(&r.options))
                .await
                .map_err(|source| YourAiError::from(ErrorKind::Model { source }))?;
            Ok(Box::pin(
                response
                    .stream
                    .map(|item| item.map_err(|source| ErrorKind::Model { source }.into())),
            ) as ModelEventStream)
        })
    }
    fn model_iden(&self) -> &str {
        &self.model
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BudgetSnapshot {
    #[serde(default)]
    pub requests: RequestMetrics,
    pub calls: u64,
    pub usage: Usage,
}
/// Shared by main model, compact, model hooks and subagents; admission is atomic.
pub struct ModelBudget {
    state: Mutex<BudgetSnapshot>,
    starts: Mutex<std::collections::VecDeque<std::time::Instant>>,
    max_calls: Option<u64>,
    max_tokens: Option<u64>,
    control: control::RequestControl,
}
impl ModelBudget {
    pub fn new(max_calls: Option<u64>, max_tokens: Option<u64>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(BudgetSnapshot::default()),
            starts: Mutex::new(std::collections::VecDeque::new()),
            max_calls,
            max_tokens,
            control: control::RequestControl::default(),
        })
    }
    pub fn configured(
        max_calls: Option<u64>,
        max_tokens: Option<u64>,
        policy: RequestPolicy,
        store: crate::SqliteStore,
    ) -> Result<Arc<Self>, YourAiError> {
        policy.validate()?;
        Ok(Arc::new(Self {
            state: Mutex::new(BudgetSnapshot::default()),
            starts: Mutex::new(std::collections::VecDeque::new()),
            max_calls,
            max_tokens,
            control: control::RequestControl::new(policy, Some(store)),
        }))
    }
    pub fn snapshot(&self) -> BudgetSnapshot {
        let mut snapshot = self.state.lock().unwrap().clone();
        let mut starts = self.starts.lock().unwrap();
        starts.retain(|t| t.elapsed() < std::time::Duration::from_secs(60));
        snapshot.requests.attempts_last_minute = starts.len() as u64;
        drop(starts);
        snapshot.requests.cooldown_seconds = self.control.remaining().as_secs_f64().ceil() as u64;
        snapshot
    }
    fn reserve(&self) -> Result<(), YourAiError> {
        let mut s = self.state.lock().unwrap();
        if self.max_calls.is_some_and(|n| s.calls >= n)
            || self.max_tokens.is_some_and(|n| s.usage.total_tokens >= n)
        {
            return Err(AbortReason::LimitReached(TurnLimit::ModelCalls).into());
        }
        s.calls += 1;
        s.requests.active += 1;
        let mut starts = self.starts.lock().unwrap();
        starts.retain(|t| t.elapsed() < std::time::Duration::from_secs(60));
        starts.push_back(std::time::Instant::now());
        Ok(())
    }
}
pub struct MeteredModel {
    pub inner: Arc<dyn ModelProvider>,
    pub budget: Arc<ModelBudget>,
}
impl ModelProvider for MeteredModel {
    fn media_tokens(&self, part: &ContentPart) -> Result<u64, YourAiError> {
        self.inner.media_tokens(part)
    }

    fn complete<'a>(&'a self, r: ModelRequest) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
        Box::pin(async move {
            self.budget.admit().await?;
            let mut attempt = Attempt::start(self.budget.clone(), &r, self.model_iden());
            match self.inner.complete(r).await {
                Ok(result) => {
                    attempt.finish(Some(&result.usage));
                    Ok(result)
                }
                Err(error) => {
                    attempt.fail(Some(&error));
                    Err(error)
                }
            }
        })
    }

    fn stream_events<'a>(
        &'a self,
        r: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
        Box::pin(async move {
            self.budget.admit().await?;
            let mut attempt = Attempt::start(self.budget.clone(), &r, self.model_iden());
            let stream = match self.inner.stream_events(r).await {
                Ok(stream) => stream,
                Err(error) => {
                    attempt.fail(Some(&error));
                    return Err(error);
                }
            };
            Ok(Box::pin(futures_util::stream::unfold(
                (stream, attempt),
                |(mut stream, mut attempt)| async move {
                    match stream.next().await {
                        Some(event) => {
                            match &event {
                                Ok(ChatStreamEvent::End(end)) => {
                                    attempt.finish(end.captured_usage.as_ref())
                                }
                                Err(error) => attempt.fail(Some(error)),
                                _ => {}
                            }
                            Some((event, (stream, attempt)))
                        }
                        None => {
                            attempt.fail(None);
                            None
                        }
                    }
                },
            )) as ModelEventStream)
        })
    }
    fn retry_after(&self, e: &YourAiError) -> Option<std::time::Duration> {
        Some(
            self.budget
                .control
                .remaining()
                .max(self.inner.retry_after(e).unwrap_or_default()),
        )
    }
    fn recovery(&self, e: &YourAiError) -> ModelRecovery {
        self.inner.recovery(e)
    }
    fn model_iden(&self) -> &str {
        self.inner.model_iden()
    }
}

/// Labels agentic hook calls too, while preserving their session IDs and shared admission.
pub(crate) struct SourceModel {
    pub inner: Arc<dyn ModelProvider>,
    pub source: &'static str,
}
impl ModelProvider for SourceModel {
    fn complete<'a>(
        &'a self,
        mut r: ModelRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
        r.source = self.source;
        self.inner.complete(r)
    }
    fn stream_events<'a>(
        &'a self,
        mut r: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
        r.source = self.source;
        self.inner.stream_events(r)
    }
    fn model_iden(&self) -> &str {
        self.inner.model_iden()
    }
    fn recovery(&self, e: &YourAiError) -> ModelRecovery {
        self.inner.recovery(e)
    }
    fn retry_after(&self, e: &YourAiError) -> Option<std::time::Duration> {
        self.inner.retry_after(e)
    }
    fn media_tokens(&self, part: &ContentPart) -> Result<u64, YourAiError> {
        self.inner.media_tokens(part)
    }
}
