//! Active context and its durable mutation boundary. SQL lives in SessionStore.
mod projection;
pub mod prompt;

use crate::error;
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc, Mutex,
    },
};
use yourai_core::prelude::*;

#[derive(Default)]
struct View {
    last_seq: i64,
    records: Vec<StoredMessage>,
    call_ids: HashSet<String>,
    result_ids: HashSet<String>,
    markers: Vec<String>,
    observation: Option<RequestObservation>,
}
/// Owns the active history and builds model requests, including cheap output pruning.
/// It persists mutations but never calls the model or dispatches compaction hooks.
pub struct DefaultContext {
    id: SessionId,
    view: Mutex<View>,
    system: Mutex<String>,
    gate: tokio::sync::Mutex<()>,
    dirty: AtomicBool,
    store: Option<Arc<dyn SessionStore>>,
    policy: ContextPolicy,
}
impl DefaultContext {
    pub fn memory(id: SessionId) -> Arc<Self> {
        Self::new(id, String::new(), None, ContextPolicy::default())
    }
    pub fn new(
        id: SessionId,
        system: String,
        store: Option<Arc<dyn SessionStore>>,
        policy: ContextPolicy,
    ) -> Arc<Self> {
        Arc::new(Self {
            id,
            view: Mutex::new(View::default()),
            system: Mutex::new(system),
            gate: tokio::sync::Mutex::new(()),
            dirty: AtomicBool::new(store.is_some()),
            store,
            policy,
        })
    }
    fn replace(&self, all: Vec<StoredMessage>) {
        let mut view = self.view.lock().unwrap();
        view.last_seq = all.iter().map(|m| m.seq).max().unwrap_or(0);
        view.call_ids = all
            .iter()
            .flat_map(|m| m.message.content.tool_calls())
            .map(|c| c.call_id.clone())
            .collect();
        view.result_ids = all
            .iter()
            .flat_map(|m| m.message.content.tool_responses())
            .map(|r| r.call_id.clone())
            .collect();
        view.markers = all
            .iter()
            .filter_map(|m| m.message.content.first_text())
            .filter(|s| s.starts_with("[Runtime event "))
            .map(str::to_owned)
            .collect();
        view.records = all
            .into_iter()
            .filter(|m| m.status == MessageStatus::Active)
            .collect();
        view.records.sort_by_key(|m| (!m.summary, m.seq));
        self.dirty.store(false, Ordering::Release);
    }
    async fn reload(&self) -> Result<(), YourAiError> {
        if let Some(store) = &self.store {
            let system = store
                .load_session(&self.id)
                .await?
                .system_prompt
                .ok_or_else(|| {
                    error(
                        "context",
                        "session system prompt must be initialized before restore",
                    )
                })?;
            let mut records = vec![];
            let mut after = 0;
            loop {
                let page = store
                    .read_messages(
                        &self.id,
                        MessageQuery {
                            active_only: false,
                            after,
                            limit: 1000,
                        },
                    )
                    .await?;
                records.extend(page.messages);
                match page.next {
                    Some(next) => after = next,
                    None => break,
                }
            }
            *self.system.lock().unwrap() = system;
            self.replace(records);
        }
        Ok(())
    }
    async fn recover(&self) -> Result<(), YourAiError> {
        if self.dirty.load(Ordering::Acquire) {
            self.reload().await?;
        }
        Ok(())
    }
    async fn append_locked(&self, messages: Vec<StoredMessage>) -> Result<(), YourAiError> {
        self.recover().await?;
        let mut ordinary = vec![];
        let mut observation = None;
        {
            let view = self.view.lock().unwrap();
            for m in messages {
                if m.summary || m.status != MessageStatus::Active {
                    return Err(error("context", "append requires ordinary active messages"));
                }
                if m.message.role == ChatRole::System {
                    return Err(error(
                        "context",
                        "system instructions belong in build_request, not append",
                    ));
                }
                if m.request_observation.is_some() {
                    observation = m.request_observation.clone();
                }
                // ToolDefinition completion is keyed by call_id across summaries/restarts.
                if m.message
                    .content
                    .tool_responses()
                    .iter()
                    .any(|r| view.result_ids.contains(&r.call_id))
                {
                    continue;
                }
                ordinary.push(m);
            }
        }
        if let Some(store) = &self.store {
            if !ordinary.is_empty() {
                self.dirty.store(true, Ordering::Release);
                ordinary = store.append_messages(&self.id, ordinary).await?;
            }
        }
        {
            let mut view = self.view.lock().unwrap();
            for mut m in ordinary {
                if view.records.iter().any(|r| r.id == m.id) {
                    continue;
                }
                if self.store.is_none() {
                    m.seq = view.records.iter().map(|r| r.seq).max().unwrap_or(0) + 1;
                }
                view.last_seq = view.last_seq.max(m.seq);
                view.call_ids.extend(
                    m.message
                        .content
                        .tool_calls()
                        .iter()
                        .map(|c| c.call_id.clone()),
                );
                view.result_ids.extend(
                    m.message
                        .content
                        .tool_responses()
                        .iter()
                        .map(|r| r.call_id.clone()),
                );
                view.markers.extend(
                    m.message
                        .content
                        .first_text()
                        .filter(|s| s.starts_with("[Runtime event "))
                        .map(str::to_owned),
                );
                if m.status == MessageStatus::Active {
                    view.records.push(m);
                }
            }
        }
        let mut view = self.view.lock().unwrap();
        self.dirty.store(false, Ordering::Release);
        if let Some(observation) = observation {
            view.observation = Some(observation);
        }
        Ok(())
    }
}
impl ContextManager for DefaultContext {
    fn last_sequence(&self) -> i64 {
        self.view.lock().unwrap().last_seq
    }
    fn system_prompt(&self) -> String {
        self.system.lock().unwrap().clone()
    }
    fn session_id(&self) -> &SessionId {
        &self.id
    }
    fn policy(&self) -> ContextPolicy {
        self.policy.clone()
    }
    fn restore(&self) -> BoxFuture<'_, Result<(), YourAiError>> {
        Box::pin(async {
            let _guard = self.gate.lock().await;
            self.reload().await
        })
    }
    fn append(&self, messages: Vec<StoredMessage>) -> BoxFuture<'_, Result<(), YourAiError>> {
        Box::pin(async move {
            let _guard = self.gate.lock().await;
            self.append_locked(messages).await
        })
    }
    fn records(&self) -> Vec<StoredMessage> {
        self.view.lock().unwrap().records.clone()
    }
    fn contains_tool_call(&self, id: &str) -> bool {
        self.view.lock().unwrap().call_ids.contains(id)
    }
    fn contains_context_marker(&self, marker: &str) -> bool {
        self.view
            .lock()
            .unwrap()
            .markers
            .iter()
            .any(|s| s.starts_with(marker))
    }
    fn build_request(
        &self,
        tools: &[ToolDefinition],
        model: &dyn ModelProvider,
        suffix: &[ChatMessage],
    ) -> Result<ContextRequest, YourAiError> {
        self.policy.validate_for(model.token_budget())?;
        if self.dirty.load(Ordering::Acquire) {
            return Err(error(
                "context",
                "restore required after uncertain storage operation",
            ));
        }
        let records = self.records();
        let mut request = self.project(&records, tools)?;
        let input_budget = self.policy.input_budget(model.token_budget());
        request.messages.extend_from_slice(suffix);
        let estimated_tokens = self.estimate(&request, model)?;
        let maintenance_needed = input_budget
            .is_some_and(|b| estimated_tokens >= b.saturating_sub(self.policy.advance_tokens));
        Ok(ContextRequest {
            request,
            estimated_tokens,
            input_budget,
            maintenance_needed,
        })
    }
    fn project(
        &self,
        records: &[StoredMessage],
        tools: &[ToolDefinition],
    ) -> Result<ChatRequest, YourAiError> {
        let mut request = ChatRequest::new(self.project_messages(records)?);
        request.system = Some(self.system_prompt());
        request.tools = Some(tools.to_vec());
        Ok(request)
    }
    fn estimate(
        &self,
        request: &ChatRequest,
        model: &dyn ModelProvider,
    ) -> Result<u64, YourAiError> {
        let raw = yourai_core::model::estimate_request(request, model)?;
        let view = self.view.lock().unwrap();
        if let Some(o) = &view.observation {
            let n = o.request.messages.len();
            if o.model == model.model_iden()
                && n <= request.messages.len()
                && o.request.system == request.system
                && serde_json::to_value(&o.request.tools).ok()
                    == serde_json::to_value(&request.tools).ok()
                && serde_json::to_value(&o.request.messages).ok()
                    == serde_json::to_value(&request.messages[..n]).ok()
            {
                return Ok(o.input_tokens.saturating_add(
                    raw.saturating_sub(yourai_core::model::estimate_request(&o.request, model)?),
                ));
            }
        }
        Ok(raw)
    }

    fn save_summary<'a>(
        &'a self,
        expected: &'a [StoredMessage],
        sources: Vec<String>,
        summary: StoredMessage,
        progress: Arc<AtomicU8>,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async move {
            let _guard = self.gate.lock().await;
            self.recover().await?;
            let current = self.records();
            if current.len() != expected.len()
                || current.iter().zip(expected).any(|(a, b)| {
                    a.id != b.id || a.tool_output_pruned_at != b.tool_output_pruned_at
                })
            {
                return Err(error(
                    "context",
                    "context changed during compaction; discard stale summary",
                ));
            }
            progress.store(CommitStatus::Started as u8, Ordering::Release);
            if let Some(store) = &self.store {
                // If the future is dropped during SQLite work, the next mutation reloads
                // after the adapter's transaction gate. Reads reject the uncertain view.
                self.dirty.store(true, Ordering::Release);
                store
                    .save_summary(&self.id, sources, summary, progress)
                    .await?;
                self.reload().await
            } else {
                let mut records = self.records();
                records.retain(|m| !sources.contains(&m.id));
                records.push(summary);
                // Preserve archival identities even in an ephemeral context.
                let mut view = self.view.lock().unwrap();
                records.sort_by_key(|m| (!m.summary, m.seq));
                view.records = records;
                progress.store(CommitStatus::Committed as u8, Ordering::Release);
                Ok(())
            }
        })
    }
}
