use super::State;
use futures_util::StreamExt;
use std::{collections::HashSet, time::Instant};
use yourai_core::prelude::*;

impl State<'_> {
    pub(crate) async fn model_step(
        &mut self,
    ) -> Result<(ChatMessage, Vec<ToolCall>), (YourAiError, bool)> {
        let mut visible = false;
        let result = self.read_model(&mut visible).await;
        result.map_err(|e| (e, visible))
    }
    async fn read_model(
        &mut self,
        visible: &mut bool,
    ) -> Result<(ChatMessage, Vec<ToolCall>), YourAiError> {
        self.bound_tools.clear();
        let mut tools = vec![];
        if let Some(registry) = &self.tc.snap.tools {
            for definition in registry.definitions() {
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
        let mut prepared = self
            .history
            .build_request(Some(&self.system), &tools, &execution)?;
        if prepared.maintenance_needed {
            if let Err(e) = self.compact(CompactionTrigger::Threshold).await {
                if matches!(e, YourAiError::Aborted(_)) || !prepared.fits() {
                    return Err(e);
                }
                // Do not continue with a stale view following an uncertain commit.
                self.history.restore().await?;
                self.notice(Level::Warning, format!("Automatic maintenance failed: {e}"))?;
            }
            prepared = self
                .history
                .build_request(Some(&self.system), &tools, &execution)?;
        }
        if !prepared.fits() {
            return Err(ErrorKind::Loop(
                "context exceeds configured input budget; no safe compaction available".into(),
            )
            .into());
        }
        self.check_model_budget()?;
        let request = prepared.request;
        let observed_request = request.clone();
        let options = self
            .history
            .default_options()
            .with_capture_content(true)
            .with_capture_tool_calls(true)
            .with_capture_usage(true)
            .with_capture_reasoning_content(true);
        let model = self.model.clone();
        let timeout = self
            .tc
            .info
            .options
            .limits
            .model_timeout
            .unwrap_or(self.config.operation_timeout);
        let deadline = self.deadline(Some(timeout));
        self.model_calls += 1;
        let mut stream = self
            .wait(
                model.stream_events(ModelRequest::new(request, options)),
                Some(timeout),
                "model",
            )
            .await?;
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut saw_tool_chunk = false;
        loop {
            let next = self
                .wait(
                    async { stream.next().await.transpose() },
                    Some(deadline.saturating_duration_since(Instant::now())),
                    "model",
                )
                .await?;
            let Some(event) = next else {
                return Err(ErrorKind::Provider {
                    name: "model",
                    message: "stream ended without terminal event".into(),
                }
                .into());
            };
            match event {
                ChatStreamEvent::Start => {}
                ChatStreamEvent::Chunk(chunk) => {
                    if !chunk.content.is_empty() {
                        if text.is_empty() {
                            self.output.text.clear();
                        }
                        text.push_str(&chunk.content);
                        self.output.text = text.clone();
                        self.partial_message =
                            Some(ChatMessage::assistant(text.clone()).with_reasoning_content(
                                (!reasoning.is_empty()).then(|| reasoning.clone()),
                            ));
                        *visible = true;
                        self.send(Out::Chunk {
                            text: chunk.content,
                        })?;
                    }
                }
                ChatStreamEvent::ReasoningChunk(chunk) => {
                    if !chunk.content.is_empty() {
                        *visible = true;
                        reasoning.push_str(&chunk.content);
                        self.partial_message = Some(
                            ChatMessage::assistant(text.clone())
                                .with_reasoning_content(Some(reasoning.clone())),
                        );
                        self.send(Out::Reasoning {
                            text: chunk.content,
                        })?;
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
                    if let Some(tracker) = &self.tc.snap.usage {
                        tracker
                            .record_event(
                                self.history.session_id(),
                                &UsageEvent::new(
                                    Some(self.model.model_iden().into()),
                                    "main",
                                    end.captured_usage.clone().unwrap_or_default(),
                                ),
                            )
                            .await?;
                    }
                    if let Some(u) = &end.captured_usage {
                        if let Some(input_tokens) = u.prompt_tokens.filter(|n| *n >= 0) {
                            self.request_observation = Some(RequestObservation {
                                model: self.model.model_iden().into(),
                                request: observed_request.clone(),
                                input_tokens: input_tokens as u64,
                            });
                        }
                        let input = u.prompt_tokens.unwrap_or(0).max(0) as u64;
                        let output = u.completion_tokens.unwrap_or(0).max(0) as u64;
                        self.record_usage(Usage {
                            input_tokens: input,
                            output_tokens: output,
                            total_tokens: u
                                .total_tokens
                                .map(|n| n.max(0) as u64)
                                .unwrap_or(input + output),
                        })
                        .await?;
                    }
                    if matches!(
                        end.captured_stop_reason,
                        Some(StopReason::MaxTokens(_) | StopReason::ContentFilter(_))
                    ) {
                        return Err(ErrorKind::Provider {
                            name: "model",
                            message: "model response truncated or filtered; tools will not execute"
                                .into(),
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
