use super::State;
use std::{
    future::Future,
    time::{Duration, Instant},
};
use yourai_core::prelude::*;

impl State<'_> {
    pub(crate) fn op_timeout(&self) -> Option<Duration> {
        Some(self.config.operation_timeout)
    }
    pub(crate) fn deadline(&self, timeout: Option<Duration>) -> Instant {
        let local = Instant::now() + timeout.unwrap_or(self.config.operation_timeout);
        self.tc
            .info
            .options
            .limits
            .deadline
            .map_or(local, |total| local.min(total))
    }
    pub(crate) fn timeout_error(&self, phase: &'static str) -> YourAiError {
        if self
            .tc
            .info
            .options
            .limits
            .deadline
            .is_some_and(|d| Instant::now() >= d)
        {
            AbortReason::DeadlineExceeded.into()
        } else {
            ErrorKind::Provider {
                name: phase,
                message: "operation timed out".into(),
            }
            .into()
        }
    }
    pub(crate) fn send(&self, out: Out) -> Result<(), YourAiError> {
        if self.tc.outbox.send(out) {
            Ok(())
        } else {
            Err(AbortReason::Disconnected.into())
        }
    }
    pub(crate) fn notice(
        &self,
        level: Level,
        message: impl Into<String>,
    ) -> Result<(), YourAiError> {
        self.send(Out::Notice {
            level,
            message: message.into(),
        })
    }
    pub(crate) fn route(&mut self, input: In) {
        // Replies are valid only while a particular request is awaiting them.
        if matches!(input, In::UserText { .. }) {
            self.queued.push_back(input);
        }
    }
    pub(crate) fn drain(&mut self) {
        // Bound the checkpoint to the snapshot length: producers cannot starve work.
        for _ in 0..self.tc.inbox.len() {
            match self.tc.inbox.try_recv() {
                Ok(input) => self.route(input),
                Err(_) => break,
            }
        }
    }
    pub(crate) fn has_steer(&self) -> bool {
        self.queued.iter().any(|i| {
            matches!(
                i,
                In::UserText {
                    mode: InputMode::Steer,
                    ..
                }
            )
        })
    }
    pub(crate) async fn wait<T>(
        &mut self,
        future: impl Future<Output = Result<T, YourAiError>>,
        timeout: Option<Duration>,
        phase: &'static str,
    ) -> Result<T, YourAiError> {
        self.tc.check_control()?;
        let deadline = self.deadline(timeout);
        tokio::pin!(future);
        loop {
            tokio::select! {
                biased;
                _ = self.tc.cancel.cancelled() => return Err(AbortReason::Cancelled.into()),
                _ = self.tc.outbox.closed() => return Err(AbortReason::Disconnected.into()),
                _ = tokio::time::sleep_until(deadline.into()) => return Err(self.timeout_error(phase)),
                result = &mut future => return result,
                input = self.tc.inbox.recv(), if !self.input_closed => match input {
                    Some(input) => self.route(input), None => self.input_closed = true,
                },
            }
        }
    }
    pub(crate) async fn consume_events(&mut self) -> Result<(), YourAiError> {
        let Some(events) = self.tc.info.options.events.clone() else {
            return Ok(());
        };
        let count = events.pending().len();
        for _ in 0..count {
            let Some(event) = events.front() else { break };
            if let Some(context) = &event.context {
                let marker = format!("[Runtime event {}]", event.id);
                if !self.history.contains_context_marker(&marker) {
                    let history = self.history.clone();
                    self.wait(
                        history.append(vec![StoredMessage::runtime_context(format!(
                            "{marker}\n{context}"
                        ))]),
                        self.op_timeout(),
                        "history",
                    )
                    .await?;
                }
            }
            if let Some(notice) = &event.notice {
                self.notice(Level::Info, notice)?;
            }
            events.ack(&event.id);
        }
        Ok(())
    }
    pub(crate) fn has_events(&self) -> bool {
        self.tc
            .info
            .options
            .events
            .as_ref()
            .is_some_and(|e| e.has_context())
    }
    pub(crate) async fn checkpoint(&mut self) -> Result<(), YourAiError> {
        self.consume_events().await?;
        self.tc.check_control()?;
        self.drain();
        let contexts = std::mem::take(&mut self.deferred_context);
        self.add_context(&contexts).await?;
        // New arrivals during hooks are deferred to the next checkpoint.
        let mut remaining = self.queued.len();
        let mut index = 0;
        while remaining > 0 {
            remaining -= 1;
            if matches!(
                self.queued.get(index),
                Some(In::UserText {
                    mode: InputMode::Steer,
                    ..
                })
            ) {
                self.accept_input(index).await?;
            } else {
                index += 1;
            }
        }
        Ok(())
    }
    pub(crate) async fn accept_input(&mut self, index: usize) -> Result<bool, YourAiError> {
        let text = match &self.queued[index] {
            In::UserText { text, .. } => text.clone(),
            _ => return Ok(false),
        };
        let hook = self
            .hook(HookEvent::UserPromptSubmit {
                prompt: text.clone(),
            })
            .await?;
        self.apply_common(&hook)?;
        if !hook.common.blocking_errors.is_empty() {
            self.notice(Level::Warning, super::hooks::feedback(&hook).join("\n"))?;
            self.queued.remove(index); // Explicit rejection, not an unprocessed input.
            return Ok(false);
        }
        let history = self.history.clone();
        let record = StoredMessage::new(ChatMessage::user(&text));
        self.wait(history.append(vec![record]), self.op_timeout(), "history")
            .await?;
        self.queued.remove(index); // Transfer only after a successful commit.
        self.add_context(&super::hooks::additional(&hook)).await?;
        Ok(true)
    }
    pub(crate) async fn add_context(&mut self, contexts: &[String]) -> Result<(), YourAiError> {
        if contexts.is_empty() {
            return Ok(());
        }
        let history = self.history.clone();
        self.wait(
            history.append(vec![StoredMessage::runtime_context(format!(
                "[Runtime context]\n{}",
                contexts.join("\n")
            ))]),
            self.op_timeout(),
            "history",
        )
        .await
    }
    pub(crate) async fn record_usage(&mut self, usage: Usage) -> Result<(), YourAiError> {
        let total = self.output.usage.get_or_insert_with(Usage::default);
        total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
        total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
        total.total_tokens = total.total_tokens.saturating_add(usage.total_tokens);
        // Update the return report first, so accounting failures retain known usage.
        self.send(Out::Usage { usage }) // Each event is a delta; TurnOutput is cumulative.
    }
    pub(crate) async fn cleanup(&mut self, cause: &YourAiError) {
        let history = self.history.clone();
        let outbox = self.tc.outbox;
        let unresolved = std::mem::take(&mut self.unresolved);
        let completed = self.tool_completion.take();
        let partial = self.partial_message.take();
        // One cleanup deadline for the entire batch, independent of cancelled Turn token.
        let cleanup = async {
            if let Some(partial) = partial {
                history.append(vec![StoredMessage::new(partial)]).await?;
            }
            for call in unresolved {
                let (output, is_error) = completed.as_ref().filter(|(c,_,_)| c.call_id == call.call_id)
                    .map(|(_,v,e)| (v.clone(),*e)).unwrap_or_else(||
                        (serde_json::json!({"error": cause.to_string(), "status": "interrupted_or_not_executed"}), true));
                history
                    .append(vec![super::tools::result_record(&call, &output, is_error)])
                    .await?;
                outbox.send(Out::ToolDone {
                    id: call.call_id,
                    name: call.fn_name,
                    output,
                    is_error,
                });
            }
            Ok::<_, YourAiError>(())
        };
        let failure = match tokio::time::timeout(self.config.cleanup_timeout, cleanup).await {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e.to_string()),
            Err(_) => Some("cleanup timed out".into()),
        };
        if let Some(message) = failure {
            outbox.send(Out::Notice {
                level: Level::Error,
                message: format!(
                    "History cleanup incomplete; do not replay tools automatically: {message}"
                ),
            });
            if let Some(obs) = &self.tc.snap.observability {
                obs.increment("loop.cleanup_failed", &[]);
            }
        }
    }
}
