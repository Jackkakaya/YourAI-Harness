//! Active context and its durable mutation boundary. SQL lives in SessionManager.
mod compact;
mod compaction_lifecycle;
mod projection;
pub mod prompt;
mod selection;
mod summary;

use crate::error;
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tokio_util::sync::CancellationToken;
use yourai_core::prelude::*;

#[derive(Default)]
pub struct ContextServices {
    /// Used only for ephemeral contexts; durable contexts restore their snapshot.
    pub system_prompt: String,
    pub store: Option<Arc<dyn SessionManager>>,
    pub hook_timeout: Option<std::time::Duration>,
    pub policy: ContextPolicy,
}
#[derive(Default)]
struct View {
    last_seq: i64,
    records: Vec<StoredMessage>,
    call_ids: HashSet<String>,
    result_ids: HashSet<String>,
    markers: Vec<String>,
    consumed: HashSet<String>,
    last_maintenance: Option<String>,
    last_prune_tokens: u64,
    observation: Option<RequestObservation>,
}
pub struct MemoryContext {
    id: SessionId,
    view: Mutex<View>,
    system: Mutex<String>,
    gate: tokio::sync::Mutex<()>,
    dirty: AtomicBool,
    services: ContextServices,
}
impl MemoryContext {
    pub fn memory(id: SessionId) -> Arc<Self> {
        let services = ContextServices::default();
        Self::new(id, services)
    }
    pub fn new(id: SessionId, services: ContextServices) -> Arc<Self> {
        Arc::new(Self {
            id,
            view: Mutex::new(View::default()),
            system: Mutex::new(services.system_prompt.clone()),
            gate: tokio::sync::Mutex::new(()),
            dirty: AtomicBool::new(services.store.is_some()),
            services,
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
        if let Some(store) = &self.services.store {
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
    async fn commit(&self, change: ContextChange) -> Result<(), YourAiError> {
        if let Some(store) = &self.services.store {
            // If the future is dropped during SQLite work, the next mutation reloads
            // after the adapter's transaction gate. Reads reject the uncertain view.
            self.dirty.store(true, Ordering::Release);
            store.save_context(&self.id, change).await?;
            self.reload().await?;
            self.view.lock().unwrap().observation = None;
            Ok(())
        } else {
            let mut records = self.records();
            for m in &mut records {
                if change.pruned.contains(&m.id) {
                    m.tool_output_pruned_at = Some(crate::storage::sqlite::now());
                }
            }
            if let Some(c) = change.compaction {
                records.retain(|m| !c.sources.contains(&m.id));
                records.push(c.summary);
            }
            // Preserve archival identities even in an ephemeral context.
            let mut view = self.view.lock().unwrap();
            records.sort_by_key(|m| (!m.summary, m.seq));
            view.records = records;
            view.observation = None;
            Ok(())
        }
    }
    async fn append_locked(&self, messages: Vec<StoredMessage>) -> Result<(), YourAiError> {
        self.recover().await?;
        let mut ordinary = vec![];
        let mut consumed = HashSet::new();
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
                if m.model_response {
                    observation = m.request_observation.clone();
                    consumed.extend(
                        view.records
                            .iter()
                            .flat_map(|r| r.message.content.tool_responses())
                            .map(|r| r.call_id.clone()),
                    );
                }
                // Tool completion is keyed by call_id across summaries/restarts.
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
        if let Some(store) = &self.services.store {
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
                if self.services.store.is_none() {
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
        view.consumed.extend(consumed);
        if let Some(observation) = observation {
            view.observation = Some(observation);
        }
        Ok(())
    }
    fn fingerprint(
        records: &[StoredMessage],
        system: Option<&str>,
        input: RequestInput<'_>,
        model: &dyn ModelProvider,
    ) -> String {
        format!(
            "{}:{:?}:{:?}:{system:?}:{}",
            model.model_iden(),
            model.token_budget(),
            records
                .iter()
                .map(|m| (&m.id, m.tool_output_pruned_at))
                .collect::<Vec<_>>(),
            serde_json::to_string(&(input.tools, input.suffix)).unwrap_or_default()
        )
    }
}
impl ContextManager for MemoryContext {
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
        self.services.policy.clone()
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
        input: RequestInput<'_>,
        execution: &ContextExecution,
    ) -> Result<ContextRequest, YourAiError> {
        self.services
            .policy
            .validate_for(execution.model().token_budget())?;
        if self.dirty.load(Ordering::Acquire) {
            return Err(error(
                "context",
                "restore required after uncertain storage operation",
            ));
        }
        let owned_system = self.system_prompt();
        let system = Some(owned_system.as_str());
        let records = self.records();
        let mut request = self.project(&records, system, input.tools)?;
        request.messages.extend_from_slice(input.suffix);
        let estimated_tokens = self.estimate(&request, execution)?;
        let input_budget = self
            .services
            .policy
            .input_budget(execution.model().token_budget());
        let unchanged = self.view.lock().unwrap().last_maintenance.as_ref()
            == Some(&Self::fingerprint(
                &records,
                system,
                input,
                execution.model().as_ref(),
            ));
        let maintenance_needed = !unchanged
            && self
                .services
                .policy
                .maintenance_threshold(execution.model().token_budget())
                .is_some_and(|threshold| estimated_tokens >= threshold);
        Ok(ContextRequest {
            request,
            estimated_tokens,
            input_budget,
            maintenance_needed,
        })
    }
    fn prepare_compaction<'a>(
        &'a self,
        options: &'a CompactionRequest,
        execution: &'a ContextExecution,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<CompactionPlan<'a>, YourAiError>> {
        Box::pin(async move { self.prepare(options, execution, cancel).await })
    }
}

impl MemoryContext {
    /// Fixed public operation, shared with every replacement ContextManager.
    pub fn compact<'a>(
        &'a self,
        request: CompactionRequest,
        execution: &'a ContextExecution,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<CompactionResult, YourAiError>> {
        Box::pin(compaction_lifecycle::compact(
            self,
            request,
            execution,
            cancel,
            self.services.hook_timeout,
        ))
    }
}
pub use compaction_lifecycle::{compact, compact_with_events};
