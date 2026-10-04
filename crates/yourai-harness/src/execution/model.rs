use super::{ExecutionState, ModelOptions};
use futures_util::StreamExt;
use std::collections::HashSet;
use yourai_core::prelude::*;

impl ExecutionState<'_> {
    /// Save what streamed so far so cancellation cleanup can persist it, then return the error.
    fn fail_with_partial(
        &mut self,
        error: YourAiError,
        text: &str,
        reasoning: &str,
    ) -> YourAiError {
        self.output.text = text.to_owned();
        if !text.is_empty() || !reasoning.is_empty() {
            self.partial_message = Some(
                ChatMessage::assistant(text.to_owned())
                    .with_reasoning_content((!reasoning.is_empty()).then(|| reasoning.to_owned())),
            );
        }
        error
    }

    pub(crate) async fn model_step(
        &mut self,
        attempt: u32,
        model_options: &ModelOptions,
    ) -> Result<(ChatMessage, Vec<ToolCall>), (YourAiError, bool)> {
        let mut visible = false;
        let result = self.read_model(&mut visible, attempt, model_options).await;
        result.map_err(|e| (e, visible))
    }
    async fn read_model(
        &mut self,
        visible: &mut bool,
        attempt: u32,
        model_options: &ModelOptions,
    ) -> Result<(ChatMessage, Vec<ToolCall>), YourAiError> {
        self.bound_tools.clear();
        let mut tools = vec![];
        if let (false, Some(registry)) = (!model_options.tools_enabled, &self.tc.snap.tools) {
            let mut definitions = registry.definitions();
            definitions.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
            for definition in definitions {
                let handler = registry.resolve(definition.name.as_str())?;
                let definition = handler.definition();
                if handler.name() != definition.name.as_str() {
                    return Err(ErrorKind::Config(
                        "tool handler name differs from its schema".into(),
                    )
                    .into());
                }
                self.bound_tools.insert(handler.name().to_owned(), handler);
                tools.push(definition);
            }
        }
        let execution = ContextExecution::from_snapshot(
            &self.tc.snap,
            self.history.session_id(),
            self.tc.info.options.session.as_deref(),
        )?;
        let mut suffix = Vec::new();
        if let Some(session) = &self.tc.info.options.session {
            suffix.extend(
                session
                    .instructions
                    .values()
                    .map(|text| ChatMessage::user(text.clone())),
            );
        }
        suffix.extend(
            model_options
                .prefill
                .iter()
                .map(|text| ChatMessage::assistant(text.clone())),
        );
        let input = RequestInput {
            tools: &tools,
            suffix: &suffix,
        };
        let mut prepared = self.history.build_request(input, &execution)?;
        if prepared.maintenance_needed {
            if let Err(e) = self.compact(CompactionTrigger::Threshold).await {
                if matches!(e, YourAiError::Aborted(_)) || !prepared.fits() {
                    return Err(e);
                }
                // Do not continue with a stale view following an uncertain commit.
                self.history.restore().await?;
                self.notice(Level::Warning, format!("Automatic maintenance failed: {e}"))?;
            }
            prepared = self.history.build_request(input, &execution)?;
        }
        if !prepared.fits() {
            return Err(ErrorKind::Loop(
                "context exceeds configured input budget; no safe compaction available".into(),
            )
            .into());
        }
        let request = prepared.request;
        let observed_request = request.clone();
        let model = self
            .model
            .clone()
            .ok_or_else(|| ErrorKind::Config("model not configured".into()))?;
        let mut options = ChatOptions::default()
            .with_max_tokens(model.token_budget().max_output_tokens())
            .with_capture_content(true)
            .with_capture_tool_calls(true)
            .with_capture_usage(true)
            .with_capture_reasoning_content(true);
        if !model_options.tools_enabled {
            options = options.with_tool_choice(ToolChoice::None);
        }
        let model_timeout = self.tc.info.options.limits.model_timeout;
        let header_timeout = model_timeout
            .or(options.stream_header_timeout)
            .unwrap_or(model.timeouts().headers);
        let chunk_timeout = model_timeout
            .or(options.stream_read_timeout)
            .unwrap_or(model.timeouts().read);
        self.model_calls += 1;
        let mut request = ModelRequest::new(request, options);
        request.session_id = Some(self.history.session_id().as_str().to_owned());
        request.turn_id = Some(self.tc.info.id.to_string());
        request.attempt = attempt;
        let transport = model.uses_transport_timeouts();
        let total_deadline = self.tc.info.options.limits.deadline;
        if transport {
            request.options.stream_header_timeout = Some(header_timeout);
            request.options.stream_read_timeout = Some(chunk_timeout);
        }
        let header_deadline = if transport {
            total_deadline
        } else {
            self.deadline(Some(header_timeout))
        };
        let mut stream = self
            .wait_until(model.stream_events(request), header_deadline, "model")
            .await?;
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut saw_tool_chunk = false;
        loop {
            let deadline = if transport {
                total_deadline
            } else {
                self.deadline(Some(chunk_timeout))
            };
            let next = match self
                .wait_until(async { stream.next().await.transpose() }, deadline, "model")
                .await
            {
                Ok(next) => next,
                Err(e) => return Err(self.fail_with_partial(e, &text, &reasoning)),
            };
            let Some(event) = next else {
                return Err(self.fail_with_partial(
                    ErrorKind::Provider {
                        name: crate::model::MODEL_NAME,
                        message: format!(
                            "stream ended without terminal event (text_bytes={}, reasoning_bytes={}, tool_call_chunks_seen={saw_tool_chunk}); incomplete tools were not executed",
                            text.len(), reasoning.len()
                        ),
                    }
                    .into(),
                    &text,
                    &reasoning,
                ));
            };
            match event {
                ChatStreamEvent::Start => {}
                ChatStreamEvent::Chunk(chunk) => {
                    if !chunk.content.is_empty() {
                        text.push_str(&chunk.content);
                        *visible = true;
                        if let Err(e) = self.send(Out::Chunk {
                            text: chunk.content,
                        }) {
                            return Err(self.fail_with_partial(e, &text, &reasoning));
                        }
                    }
                }
                ChatStreamEvent::ReasoningChunk(chunk) => {
                    if !chunk.content.is_empty() {
                        *visible = true;
                        reasoning.push_str(&chunk.content);
                        if let Err(e) = self.send(Out::Reasoning {
                            text: chunk.content,
                        }) {
                            return Err(self.fail_with_partial(e, &text, &reasoning));
                        }
                    }
                }
                ChatStreamEvent::ToolCallChunk(_) => {
                    saw_tool_chunk = true;
                }
                ChatStreamEvent::ThoughtSignatureChunk(_) => {} // Preserved in captured content.
                ChatStreamEvent::End(end) => {
                    // Nothing after End can be retried: accounting/persistence is not a model retry.
                    *visible = true;
                    if let Some(content) = &end.captured_content {
                        text = content.texts().join("");
                        self.output.text = text.clone();
                    }
                    if !text.is_empty() || !reasoning.is_empty() {
                        self.partial_message = Some(
                            ChatMessage::assistant(text.clone()).with_reasoning_content(
                                end.captured_reasoning_content
                                    .clone()
                                    .or_else(|| (!reasoning.is_empty()).then(|| reasoning.clone())),
                            ),
                        );
                    }
                    if let Some(warning) = crate::model::accounting::record_response(
                        self.tc.snap.usage.as_deref(),
                        self.history.session_id(),
                        model.model_iden(),
                        "main",
                        end.captured_usage.clone().unwrap_or_default(),
                    )
                    .await
                    {
                        self.notice(Level::Warning, warning)?;
                    }
                    if let Some(u) = &end.captured_usage {
                        if let Some(input_tokens) = u.prompt_tokens.filter(|n| *n >= 0) {
                            self.request_observation = Some(RequestObservation {
                                model: model.model_iden().into(),
                                request: observed_request.clone(),
                                input_tokens: input_tokens as u64,
                            });
                        }
                        self.record_usage(crate::model::usage(u)).await?;
                    }
                    if let Some(
                        reason @ (StopReason::MaxTokens(_) | StopReason::ContentFilter(_)),
                    ) = &end.captured_stop_reason
                    {
                        return Err(ErrorKind::Provider {
                            name: crate::model::MODEL_NAME,
                            message: format!(
                                "model response truncated or filtered; tools will not execute (finish_reason={})",
                                reason.raw()
                            ),
                        }
                        .into());
                    }
                    let calls: Vec<_> = end
                        .captured_tool_calls()
                        .unwrap_or_default()
                        .into_iter()
                        .cloned()
                        .collect();
                    if saw_tool_chunk && calls.is_empty() {
                        return Err(
                            ErrorKind::Loop("stream lost captured tool calls".into()).into()
                        );
                    }
                    let mut ids = HashSet::new();
                    for call in &calls {
                        if call.call_id.is_empty()
                            || call.fn_name.is_empty()
                            || self.call_ids.contains(&call.call_id)
                            || self.history.contains_tool_call(&call.call_id)
                            || !ids.insert(call.call_id.clone())
                        {
                            return Err(ErrorKind::Loop(
                                "missing or duplicate tool-call identity".into(),
                            )
                            .into());
                        }
                    }
                    self.call_ids.extend(ids);
                    let content = end
                        .captured_content
                        .unwrap_or_else(|| MessageContent::from_text(text.clone()));
                    self.output.text = content.texts().join("");
                    let message = ChatMessage::assistant(content).with_reasoning_content(
                        end.captured_reasoning_content
                            .or_else(|| (!reasoning.is_empty()).then_some(reasoning)),
                    );
                    return Ok((message, calls));
                }
            }
        }
    }
}

/// Safe, structured error context for retry notices; never print request payloads.
pub(crate) fn retry_cause(error: &YourAiError) -> String {
    let Some((status, body)) = crate::model::failure::http_error(error) else {
        return "model provider error".into();
    };
    let mut reason = format!("HTTP {status}");
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        for field in ["code", "dimension"] {
            if let Some(value) = value
                .get("error")
                .and_then(|e| e.get(field))
                .and_then(|v| v.as_str())
            {
                if !value.is_empty()
                    && value.len() <= 64
                    && value
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
                {
                    reason.push_str(&format!(", {field}={value}"));
                }
            }
        }
    }
    reason
}
