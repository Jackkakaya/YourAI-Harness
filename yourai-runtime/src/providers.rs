use crate::{
    error,
    storage::{atomic_write, read_json},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
};
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
}
impl GenaiModel {
    pub fn new(client: genai::Client, model: impl Into<String>) -> Self {
        Self {
            client,
            model: model.into(),
        }
    }
}
impl ModelProvider for GenaiModel {
    fn complete<'a>(&'a self, r: ModelRequest) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
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
    pub calls: u64,
    pub usage: Usage,
}
/// Shared by main model, compact, model hooks and subagents; admission is atomic.
pub struct ModelBudget {
    state: Mutex<BudgetSnapshot>,
    max_calls: Option<u64>,
    max_tokens: Option<u64>,
}
impl ModelBudget {
    pub fn new(max_calls: Option<u64>, max_tokens: Option<u64>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(BudgetSnapshot::default()),
            max_calls,
            max_tokens,
        })
    }
    pub fn snapshot(&self) -> BudgetSnapshot {
        self.state.lock().unwrap().clone()
    }
    fn reserve(&self) -> Result<(), YourAiError> {
        let mut s = self.state.lock().unwrap();
        if self.max_calls.is_some_and(|n| s.calls >= n)
            || self.max_tokens.is_some_and(|n| s.usage.total_tokens >= n)
        {
            return Err(AbortReason::LimitReached(TurnLimit::ModelCalls).into());
        }
        s.calls += 1;
        Ok(())
    }
    fn record(&self, u: Usage) {
        let mut s = self.state.lock().unwrap();
        s.usage.input_tokens = s.usage.input_tokens.saturating_add(u.input_tokens);
        s.usage.output_tokens = s.usage.output_tokens.saturating_add(u.output_tokens);
        s.usage.total_tokens = s.usage.total_tokens.saturating_add(u.total_tokens);
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
            self.budget.reserve()?;
            let result = self.inner.complete(r).await?;
            self.budget.record(usage(&result.usage));
            Ok(result)
        })
    }

    fn stream_events<'a>(
        &'a self,
        r: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
        Box::pin(async move {
            self.budget.reserve()?;
            let stream = self.inner.stream_events(r).await?;
            let budget = self.budget.clone();
            let mut recorded = false;
            Ok(Box::pin(stream.map(move |e| {
                if let Ok(ChatStreamEvent::End(end)) = &e {
                    if !recorded {
                        if let Some(u) = &end.captured_usage {
                            budget.record(usage(u));
                        }
                        recorded = true;
                    }
                }
                e
            })) as ModelEventStream)
        })
    }
    fn recovery(&self, e: &YourAiError) -> ModelRecovery {
        self.inner.recovery(e)
    }
    fn model_iden(&self) -> &str {
        self.inner.model_iden()
    }
}
#[derive(Default)]
pub struct ToolSet {
    handlers: RwLock<HashMap<String, Arc<dyn ToolHandler>>>,
}
impl ToolRegistry for ToolSet {
    fn register(&self, h: Arc<dyn ToolHandler>) {
        self.handlers.write().unwrap().insert(h.name().into(), h);
    }
    fn unregister(&self, n: &str) {
        self.handlers.write().unwrap().remove(n);
    }
    fn has(&self, n: &str) -> bool {
        self.handlers.read().unwrap().contains_key(n)
    }
    fn definitions(&self) -> Vec<Tool> {
        let mut v: Vec<_> = self
            .handlers
            .read()
            .unwrap()
            .values()
            .map(|h| h.definition())
            .collect();
        v.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        v
    }
    fn resolve(&self, n: &str) -> Result<Arc<dyn ToolHandler>, YourAiError> {
        self.handlers
            .read()
            .unwrap()
            .get(n)
            .cloned()
            .ok_or_else(|| error("tools", format!("unknown tool: {n}")))
    }
    fn count(&self) -> usize {
        self.handlers.read().unwrap().len()
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Policy {
    allow: Vec<String>,
    deny: Vec<String>,
    ask: Vec<String>,
}
/// Persistent exact tool-name rules. Hard deny configuration cannot be removed by hooks.
pub struct PolicySecurity {
    path: PathBuf,
    rules: Mutex<Policy>,
    hard_deny: Vec<String>,
}
impl PolicySecurity {
    pub fn open(path: PathBuf, hard_deny: Vec<String>) -> Result<Arc<Self>, YourAiError> {
        let rules = if path.exists() {
            read_json(&path)?
        } else {
            Policy::default()
        };
        Ok(Arc::new(Self {
            path,
            rules: Mutex::new(rules),
            hard_deny,
        }))
    }
}
impl SecurityProvider for PolicySecurity {
    fn update_permissions<'a>(
        &'a self,
        updates: &'a [Value],
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async move {
            let mut guard = self.rules.lock().unwrap();
            let mut next = guard.clone();
            for update in updates {
                if update
                    .get("destination")
                    .is_some_and(|d| d.as_str() != Some("session"))
                {
                    return Err(error(
                        "security",
                        "only session permission destination is supported",
                    ));
                }
                let kind = update["type"].as_str().unwrap_or("");
                if kind != "addRules" && kind != "removeRules" && kind != "replaceRules" {
                    return Err(error("security", "unsupported permission update type"));
                }
                let rules = update["rules"]
                    .as_array()
                    .ok_or_else(|| error("security", "rules must be an array"))?;
                let target = match update["behavior"].as_str() {
                    Some("allow") => &mut next.allow,
                    Some("deny") => &mut next.deny,
                    Some("ask") => &mut next.ask,
                    _ => return Err(error("security", "invalid rule behavior")),
                };
                if kind == "replaceRules" {
                    target.clear();
                }
                for rule in rules {
                    let name = rule["toolName"]
                        .as_str()
                        .ok_or_else(|| error("security", "rule requires toolName"))?;
                    if rule.get("ruleContent").is_some_and(|v| !v.is_null()) {
                        return Err(error(
                            "security",
                            "scoped rule patterns unsupported; refusing broader grant",
                        ));
                    }
                    if kind == "addRules" || kind == "replaceRules" {
                        if !target.iter().any(|n| n == name) {
                            target.push(name.into());
                        }
                    } else {
                        target.retain(|n| n != name);
                    }
                }
            }
            atomic_write(&self.path, &next)?;
            *guard = next;
            Ok(())
        })
    }
    fn check_tool_call<'a>(
        &'a self,
        c: &'a SecurityContext,
    ) -> BoxFuture<'a, Result<ApprovalDecision, YourAiError>> {
        Box::pin(async move {
            let p = self.rules.lock().unwrap();
            Ok(
                if self.hard_deny.contains(&c.action) || p.deny.contains(&c.action) {
                    ApprovalDecision::Deny
                } else if p.ask.contains(&c.action) {
                    ApprovalDecision::Ask
                } else if p.allow.contains(&c.action) {
                    ApprovalDecision::Allow
                } else {
                    ApprovalDecision::Ask
                },
            )
        })
    }
    fn check_command<'a>(
        &'a self,
        _: &'a str,
    ) -> BoxFuture<'a, Result<PolicyDecision, YourAiError>> {
        Box::pin(async { Ok(PolicyDecision::Deny) })
    }
    fn check_file_access<'a>(
        &'a self,
        _: &'a str,
        _: bool,
    ) -> BoxFuture<'a, Result<PolicyDecision, YourAiError>> {
        Box::pin(async { Ok(PolicyDecision::Deny) })
    }
}
