use super::Turn;
use crate::prelude::*;
use serde_json::Value;
use std::{
    future::Future,
    time::{Duration, Instant},
};

impl Turn<'_> {
    pub(crate) fn op_timeout(&self) -> Option<Duration> {
        self.config.operation_timeout
    }
    pub(crate) fn deadline(&self, timeout: Option<Duration>) -> Option<Instant> {
        let local = timeout
            .or(self.config.operation_timeout)
            .map(|d| Instant::now() + d);
        match (local, self.tc.info.options.limits.deadline) {
            (Some(local), Some(total)) => Some(local.min(total)),
            (local, total) => local.or(total),
        }
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
    pub(crate) fn route(&mut self, mut input: In) {
        input.ensure_id();
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
    pub(crate) async fn wait_operation<T>(
        &mut self,
        future: impl Future<Output = Result<T, YourAiError>>,
        timeout: Option<Duration>,
        phase: &'static str,
    ) -> Result<T, YourAiError> {
        self.wait_until(future, self.deadline(timeout), phase).await
    }
    pub(crate) async fn wait_until<T>(
        &mut self,
        future: impl Future<Output = Result<T, YourAiError>>,
        deadline: Option<Instant>,
        phase: &'static str,
    ) -> Result<T, YourAiError> {
        self.tc.check_control()?;
        tokio::pin!(future);
        loop {
            tokio::select! {
                biased;
                _ = self.tc.cancel.cancelled() => return Err(AbortReason::Cancelled.into()),
                _ = self.tc.outbox.closed() => return Err(AbortReason::Disconnected.into()),
                _ = crate::time::sleep_until(deadline) => return Err(self.timeout_error(phase)),
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
                    self.wait_operation(
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
    pub async fn checkpoint(&mut self) -> Result<(), YourAiError> {
        self.ensure_active()?;
        self.consume_events().await?;
        self.tc.check_control()?;
        self.drain();
        self.flush_context().await?;
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
                self.accept_queued_input(index, false).await?;
            } else {
                index += 1;
            }
        }
        Ok(())
    }
    pub(crate) async fn add_context(&mut self, contexts: &[String]) -> Result<(), YourAiError> {
        self.defer_context(contexts.to_vec());
        self.flush_context().await
    }
    async fn flush_context(&mut self) -> Result<(), YourAiError> {
        let count = self.deferred_context.len();
        if count == 0 {
            return Ok(());
        }
        let history = self.history.clone();
        self.wait_operation(
            history.append(self.deferred_context.clone()),
            self.op_timeout(),
            "history",
        )
        .await?;
        self.deferred_context.drain(..count);
        Ok(())
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
        let cleanup = async {
            if let Some(partial) = self.partial_message.clone() {
                history.append(vec![partial]).await?;
                self.partial_message = None;
            }
            for record in self.calls.clone() {
                let (output, is_error) = match &record.state {
                    super::CallState::Observed { output, is_error } => (output.clone(), *is_error),
                    _ => (
                        serde_json::json!({"error":cause.to_string(),"status":"interrupted_or_not_executed"}),
                        true,
                    ),
                };
                history.append(vec![record.result_record(&output)]).await?;
                self.calls
                    .retain(|pending| pending.call.call_id != record.call.call_id);
                outbox.send(Out::ToolDone {
                    id: record.call.call_id,
                    name: record.call.fn_name,
                    output,
                    is_error,
                });
            }
            if !self.deferred_context.is_empty() {
                history.append(self.deferred_context.clone()).await?;
                self.deferred_context.clear();
            }
            Ok::<_, YourAiError>(())
        };
        let failure = match crate::time::timeout(self.config.cleanup_timeout, cleanup).await {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error.to_string()),
            Err(_) => Some("cleanup timed out".into()),
        };
        if let Some(message) = failure {
            outbox.send(Out::Notice {
                level: Level::Error,
                message: format!(
                    "History cleanup incomplete; do not replay tools automatically: {message}"
                ),
            });
            if let Some(observability) = &self.tc.snap.observability {
                observability.increment("loop.cleanup_failed", &[]);
            }
        }
    }
}

impl Turn<'_> {
    pub(crate) async fn ask(
        &mut self,
        id: String,
        payload: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, YourAiError> {
        self.tc.check_control()?;
        // Discard pre-sent replies before publishing a new request.
        self.drain();
        self.send(Out::Ask {
            id: id.clone(),
            payload,
        })?;
        let deadline = self.deadline(timeout);
        loop {
            tokio::select! {
                biased;
                _ = self.tc.cancel.cancelled() => return Err(AbortReason::Cancelled.into()),
                _ = self.tc.outbox.closed() => return Err(AbortReason::Disconnected.into()),
                _ = crate::time::sleep_until(deadline) => return Err(self.timeout_error("approval")),
                input = self.tc.inbox.recv() => match input {
                    Some(In::Reply { id: reply_id, payload }) if reply_id == id => return Ok(payload),
                    Some(input) => self.route(input),
                    None => { self.input_closed = true; return Err(AbortReason::Disconnected.into()); }
                }
            }
        }
    }
}
