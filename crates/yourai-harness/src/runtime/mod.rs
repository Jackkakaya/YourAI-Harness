mod compaction;
mod journal;
mod lifecycle;
use crate::{
    error,
    storage::{atomic_write, read_json},
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashSet, VecDeque},
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio_util::sync::CancellationToken;
use yourai_core::{
    prelude::*,
    runtime_event::{RuntimeEvent, RuntimeEvents},
};

#[derive(Clone)]
pub struct HostConfig {
    pub input: yourai_core::turn::InputOptions,
    pub hook_timeout: Option<Duration>,
    /// Optional supervisor quarantine bound after cancellation.
    pub cleanup_timeout: Option<Duration>,
    pub allow_background_wake: bool,
    pub max_followups: usize,
    /// Files already frozen in the system prompt; observe without reinjecting.
    pub instruction_watch_paths: Vec<PathBuf>,
    pub workspace_enabled: bool,
}
impl Default for HostConfig {
    fn default() -> Self {
        Self {
            input: yourai_core::turn::InputOptions::default(),
            hook_timeout: None,
            cleanup_timeout: None,
            allow_background_wake: true,
            max_followups: 64,
            instruction_watch_paths: vec![],
            workspace_enabled: false,
        }
    }
}

/// Workspace file-watcher poll interval.
const WATCH_INTERVAL: Duration = Duration::from_millis(250);
/// Follow-up injected to make an idle host consume a pending runtime event.
const RUNTIME_EVENT_PROMPT: &str = "Process the pending runtime event.";

/// Acquire execution ownership before assembly can mutate session state.
/// The lease is transferred into the host without releasing or reacquiring it.
pub(crate) struct SessionLease {
    pub(crate) dir: PathBuf,
    lock: File,
}
impl SessionLease {
    pub(crate) fn acquire(dir: PathBuf) -> Result<Self, YourAiError> {
        std::fs::create_dir_all(&dir).map_err(|e| error("host", e))?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("host.lock"))
            .map_err(|e| error("host", e))?;
        lock.try_lock_exclusive().map_err(|e| error("host", e))?;
        Ok(Self { dir, lock })
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Journal {
    queue: VecDeque<In>,
    active: Vec<In>,
    interrupted: Vec<In>,
    events: Vec<RuntimeEvent>,
    seen: HashSet<String>,
    cwd: PathBuf,
    watch: Vec<PathBuf>,
    last_error: Option<String>,
    closed: bool,
}
struct Live {
    journal: Journal,
    status: SessionStatus,
    inbox: Option<mpsc::UnboundedSender<In>>,
    cancel: Option<CancellationToken>,
    asks: HashSet<String>,
    closed_pending: Vec<In>,
}
pub struct SessionHost {
    self_ref: std::sync::OnceLock<std::sync::Weak<SessionHost>>,
    pub(crate) context: Mutex<SessionContext>,
    pub(crate) agent: Arc<Agent>,
    pub(crate) dir: PathBuf,
    config: HostConfig,
    input_options: Mutex<yourai_core::turn::InputOptions>,
    live: Mutex<Live>,
    journal_gate: Mutex<()>,
    events: Arc<RuntimeEvents>,
    operation: Arc<tokio::sync::Mutex<()>>,
    workspace: std::sync::OnceLock<Arc<crate::workspace::Workspace>>,
    children: Mutex<Vec<std::sync::Weak<SessionHost>>>,
    notify: Notify,
    closing: CancellationToken,
    background: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    _lock: File,
}
struct CancelGuard(CancellationToken);
impl Drop for CancelGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
struct StatusGuard<'a>(&'a SessionHost);
impl Drop for StatusGuard<'_> {
    fn drop(&mut self) {
        let mut l = self.0.live.lock().unwrap();
        l.cancel = None;
        if !self.0.closing.is_cancelled() {
            l.status = SessionStatus::Idle;
        }
    }
}
/// Estimated next request, including the frozen system prompt and tool schemas.
#[derive(Debug, Clone)]
pub struct ContextUsage {
    pub estimated_tokens: u64,
    pub context_window: Option<u64>,
    pub input_budget: Option<u64>,
    pub output_reserve: u64,
}

impl SessionHost {
    pub(crate) fn configure_input(&self, skill_ids: Vec<String>, memory_search_limit: usize) {
        *self.input_options.lock().unwrap() = yourai_core::turn::InputOptions {
            skill_ids,
            memory_search_limit,
        };
    }
    pub fn context_usage(&self) -> Result<ContextUsage, YourAiError> {
        let snapshot = self.agent.ctx().snapshot()?;
        let history = snapshot
            .context_manager
            .as_ref()
            .ok_or_else(|| error("context", "context not configured"))?;
        let tools = snapshot
            .tools
            .as_ref()
            .map(|r| r.definitions())
            .unwrap_or_default();
        let execution = ContextExecution::from_snapshot(
            &snapshot,
            history.session_id(),
            Some(&self.context()),
        )?;
        let request = history.build_request(RequestInput::tools(&tools), &execution)?;
        let budget = execution.model().token_budget();
        Ok(ContextUsage {
            estimated_tokens: request.estimated_tokens,
            context_window: budget.limits().context,
            input_budget: request.input_budget,
            output_reserve: u64::from(budget.max_output_tokens()),
        })
    }

    pub fn context(&self) -> SessionContext {
        self.context.lock().unwrap().clone()
    }
    pub fn status(&self) -> SessionStatus {
        self.live.lock().unwrap().status.clone()
    }
    pub async fn open(
        dir: impl Into<PathBuf>,
        context: SessionContext,
        agent: Arc<Agent>,
        config: HostConfig,
        source: &str,
    ) -> Result<Arc<Self>, YourAiError> {
        let lease = SessionLease::acquire(dir.into())?;
        Self::open_owned(lease, context, agent, config, source).await
    }

    async fn run_open(
        lease: SessionLease,
        context: SessionContext,
        agent: Arc<Agent>,
        config: HostConfig,
        source: &str,
    ) -> Result<Arc<Self>, YourAiError> {
        let SessionLease { dir, lock } = lease;
        let history = agent.ctx().context_manager()?;
        if history.session_id() != &context.id {
            return Err(error("host", "history/session identity mismatch"));
        }
        let mut journal: Journal = if dir.join("host.json").exists() {
            read_json(&dir.join("host.json"))?
        } else {
            Journal {
                cwd: context.cwd.clone(),
                ..Default::default()
            }
        };
        let was_interrupted = !journal.active.is_empty();
        journal.interrupted.append(&mut journal.active);
        journal.closed = false;
        let events = Arc::new(RuntimeEvents::default());
        for e in &journal.events {
            events.push(e.clone());
        }
        let mut context = context;
        context.cwd = journal.cwd.clone();
        let host = Arc::new(Self {
            self_ref: std::sync::OnceLock::new(),
            context: Mutex::new(context),
            agent,
            dir,
            input_options: Mutex::new(yourai_core::turn::InputOptions {
                skill_ids: config.input.skill_ids.clone(),
                memory_search_limit: config.input.memory_search_limit,
            }),
            config,
            journal_gate: Mutex::new(()),
            live: Mutex::new(Live {
                journal,
                status: SessionStatus::Idle,
                inbox: None,
                cancel: None,
                asks: HashSet::new(),
                closed_pending: vec![],
            }),
            events,
            operation: Arc::new(tokio::sync::Mutex::new(())),
            workspace: std::sync::OnceLock::new(),
            children: Mutex::new(vec![]),
            notify: Notify::new(),
            closing: CancellationToken::new(),
            background: Mutex::new(vec![]),
            _lock: lock,
        });
        let _ = host.self_ref.set(Arc::downgrade(&host));
        host.reconcile_history().await?;
        host.persist().await?;
        host.attach_background();
        if host.dir.join("config.json").exists() {
            read_json::<crate::workspace::RuntimeConfig>(&host.dir.join("config.json"))?
                .apply(&host);
        }
        if host.config.workspace_enabled || !host.config.instruction_watch_paths.is_empty() {
            let workspace = host.workspace()?;
            workspace
                .setup(if source == "startup" {
                    "init"
                } else {
                    "maintenance"
                })
                .await?;
            for path in &host.config.instruction_watch_paths {
                workspace.watch_instructions(path, source).await?;
            }
        }
        if was_interrupted {
            host.reconcile_history().await?;
            host.post_event_async(RuntimeEvent{id:format!("recovered-{}",uuid::Uuid::new_v4()),context:None,notice:Some("Previous execution was interrupted; tools were not replayed. Inspect interrupted_inputs() before continuing.".into()),wake:false}).await?;
        }
        Ok(host)
    }
    pub fn workspace(self: &Arc<Self>) -> Result<Arc<crate::workspace::Workspace>, YourAiError> {
        if let Some(ws) = self.workspace.get() {
            return Ok(ws.clone());
        }
        let ws = crate::workspace::Workspace::new(self)?;
        let _ = self.workspace.set(ws);
        Ok(self.workspace.get().unwrap().clone())
    }
    pub(crate) fn register_child(&self, child: &Arc<Self>) {
        self.children.lock().unwrap().push(Arc::downgrade(child));
    }
    pub fn interrupted_inputs(&self) -> Vec<In> {
        self.live.lock().unwrap().journal.interrupted.clone()
    }
    pub fn queued(&self) -> usize {
        self.live.lock().unwrap().journal.queue.len()
    }
    pub fn last_error(&self) -> Option<String> {
        self.live.lock().unwrap().journal.last_error.clone()
    }
    pub fn watch_paths(&self) -> Vec<PathBuf> {
        self.live.lock().unwrap().journal.watch.clone()
    }
    pub fn watch_path(&self, path: PathBuf) -> Result<(), YourAiError> {
        let path = if path.is_absolute() {
            path
        } else {
            self.context().cwd.join(path)
        };
        let mut journal = self.journal();
        self.ensure_open()?;
        if !journal.watch.contains(&path) {
            journal.watch.push(path);
        }
        journal.commit()?;
        drop(journal);
        if let Some(host) = self.self_ref.get().and_then(std::sync::Weak::upgrade) {
            host.workspace()?.start_watching(WATCH_INTERVAL)?;
        }
        Ok(())
    }
    pub fn post_event(&self, event: RuntimeEvent) -> Result<bool, YourAiError> {
        let mut journal = self.journal();
        self.ensure_open()?;
        if journal.seen.contains(&event.id) {
            return Ok(false);
        }
        journal.seen.insert(event.id.clone());
        journal.events.push(event.clone());
        if event.wake
            && self.config.allow_background_wake
            && self.status() == SessionStatus::Idle
            && journal.queue.is_empty()
        {
            journal.queue.push_back(In::follow_up(RUNTIME_EVENT_PROMPT));
        }
        journal.commit()?;
        self.events.push(event);
        self.notify.notify_one();
        Ok(true)
    }
    pub(crate) async fn history(&self) -> Result<Arc<dyn ContextManager>, YourAiError> {
        self.agent.ctx().context_manager()
    }

    async fn reconcile_history(&self) -> Result<(), YourAiError> {
        let history = self.history().await?;
        history.restore().await?;
        let messages = history.messages();
        let done: HashSet<_> = messages
            .iter()
            .flat_map(|m| m.content.tool_responses())
            .map(|r| r.call_id.clone())
            .collect();
        for call in messages
            .iter()
            .flat_map(|m| m.content.tool_calls())
            .filter(|c| !done.contains(&c.call_id))
        {
            history
                .append(vec![StoredMessage::new(ChatMessage::from(
                    ToolResponse::from_tool_call(
                        call,
                        "{\"error\":\"interrupted; state unknown; do not replay automatically\"}",
                    ),
                ))])
                .await?;
        }
        Ok(())
    }
    /// Drives queued follow-ups; bounded so a producer cannot monopolize the host.
    pub async fn run_until_idle(
        self: &Arc<Self>,
        limits: TurnLimits,
        out: &dyn OutSink,
        cancel: &CancellationToken,
    ) -> Result<Vec<SessionTurn>, YourAiError> {
        let mut reports = vec![];
        for _ in 0..self.config.max_followups {
            match self.run_next(limits.clone(), out, cancel).await? {
                Some(report) => {
                    let failed = report.result.is_err();
                    reports.push(report);
                    if failed {
                        break;
                    }
                }
                None => break,
            }
        }
        Ok(reports)
    }
    /// Long-lived TUI/Web driver, also wakes for allowed background events.
    pub async fn serve(
        self: &Arc<Self>,
        limits: TurnLimits,
        out: &dyn OutSink,
        cancel: &CancellationToken,
    ) -> Result<(), YourAiError> {
        loop {
            let notified = self.notify.notified();
            while let Some(event) = self.events.front() {
                if event.context.is_some() {
                    break;
                }
                if let Some(message) = event.notice {
                    if !out.send(Out::Notice {
                        level: Level::Info,
                        message,
                    }) {
                        return Err(AbortReason::Disconnected.into());
                    }
                }
                self.events.ack(&event.id);
                self.persist().await?;
            }
            if self.queued() > 0 {
                let reports = self.run_until_idle(limits.clone(), out, cancel).await?;
                if let Some(failure) = reports.into_iter().find_map(|r| r.result.err()) {
                    return Err(*failure.error);
                }
                if self.queued() > 0 {
                    tokio::task::yield_now().await;
                    continue;
                }
            }
            tokio::select! {_=cancel.cancelled()=>return Ok(()),_=self.closing.cancelled()=>return Ok(()),_=out.closed()=>return Err(AbortReason::Disconnected.into()),_=notified=>{}}
        }
    }
    pub(crate) fn try_operation(&self) -> Result<tokio::sync::OwnedMutexGuard<()>, YourAiError> {
        self.ensure_open()?;
        let guard = self
            .operation
            .clone()
            .try_lock_owned()
            .map_err(|_| error("host", "session is busy"))?;
        // Check after acquisition: close may have finished while this caller
        // was about to acquire the operation gate.
        self.ensure_open()?;
        Ok(guard)
    }
    fn ensure_open(&self) -> Result<(), YourAiError> {
        if self.closing.is_cancelled() {
            return Err(error("host", "session closing"));
        }
        Ok(())
    }
    pub(crate) fn set_cwd(&self, cwd: PathBuf) -> Result<(), YourAiError> {
        let mut journal = self.journal();
        self.ensure_open()?;
        journal.cwd = cwd.clone();
        journal.commit()?;
        self.context.lock().unwrap().cwd = cwd;
        Ok(())
    }
}
// Implement on Arc so the independently supervised Turn always owns its host.
impl SessionRuntime for SessionHost {
    fn context(&self) -> SessionContext {
        self.context.lock().unwrap().clone()
    }
    fn status(&self) -> SessionStatus {
        self.live.lock().unwrap().status.clone()
    }
    /// Synchronous compatibility entry point; async callers use submit_async.
    fn submit(&self, input: In) -> Result<(), InputRejected> {
        let reject = |reason: String| InputRejected {
            input: input.clone(),
            reason,
        };
        // Replies are ephemeral and never wait for the journal writer.
        if let In::Reply { id, .. } = &input {
            let mut live = self.live.lock().unwrap();
            if self.closing.is_cancelled() {
                return Err(reject("session closing".into()));
            }
            if !live.asks.remove(id) {
                return Err(reject("no matching active interaction".into()));
            }
            return live
                .inbox
                .as_ref()
                .ok_or_else(|| reject("no active turn".into()))?
                .send(input.clone())
                .map_err(|_| reject("turn inbox closed".into()));
        }
        let mut journal = self.journal();
        if self.closing.is_cancelled() {
            return Err(reject("session closing".into()));
        }
        let inbox = self.live.lock().unwrap().inbox.clone();
        let steer = matches!(
            input,
            In::UserText {
                mode: InputMode::Steer,
                ..
            }
        ) && inbox.is_some();
        if steer {
            journal.active.push(input.clone());
        } else {
            journal.queue.push_back(input.clone());
        }
        journal.commit().map_err(|e| reject(e.to_string()))?;
        if steer && inbox.unwrap().send(input.clone()).is_err() {
            journal.active.pop();
            journal.queue.push_back(input);
            if let Err(e) = journal.commit() {
                journal.retain_for_recovery(&e);
            }
        }
        self.notify.notify_one();
        Ok(())
    }
    fn run_next<'a>(
        &'a self,
        limits: TurnLimits,
        outbox: &'a dyn OutSink,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Option<SessionTurn>, YourAiError>> {
        Box::pin(async move {
            let gate = self.try_operation()?;
            // A previous failed/cancelled append may have committed in SQLite
            // without updating the memory view. Establish the committed boundary
            // first, so those older records never enter this turn's sync payload.
            let after_seq = if let Some(history) = self.agent.ctx().try_context_manager() {
                history.restore().await?;
                history.last_sequence()
            } else {
                0
            };
            let request_cancel = cancel.child_token();
            let guard = CancelGuard(request_cancel.clone());
            let (tx, mut rx) = mpsc::unbounded_channel();
            let (done, result) = oneshot::channel();
            let host = self
                .self_ref
                .get()
                .and_then(std::sync::Weak::upgrade)
                .ok_or_else(|| error("host", "host released"))?;
            tokio::spawn(async move {
                let _gate = gate;
                let start_cancel = request_cancel.clone();
                let start = host
                    .blocking(move |host| {
                        let mut journal = host.journal();
                        if host.closing.is_cancelled() {
                            return Err(error("host", "session closing"));
                        }
                        let Some(first) = journal.queue.pop_front() else {
                            return Ok(None);
                        };
                        journal.active = vec![first.clone()];
                        journal.commit()?;
                        if host.closing.is_cancelled() || start_cancel.is_cancelled() {
                            journal.restore_unstarted(first)?;
                            return Err(AbortReason::Cancelled.into());
                        }
                        let mut options = TurnOptions::default();
                        options.session = Some(Arc::new(host.context()));
                        options.limits = limits;
                        options.events = Some(host.events.clone());
                        options.input = Some(host.input_options.lock().unwrap().clone());
                        let handle = match host.agent.start_with(first.clone(), options) {
                            Ok(handle) => handle,
                            Err(e) => {
                                journal.restore_unstarted(first)?;
                                return Err(e);
                            }
                        };
                        let id = handle.info.id.clone();
                        let mut live = host.live.lock().unwrap();
                        live.status = SessionStatus::Running {
                            turn_id: id.clone(),
                        };
                        live.inbox = Some(handle.inbox.clone());
                        live.cancel = Some(handle.cancel.clone());
                        // Close may have raced the durable start before the cancel
                        // handle was published. Holding live here closes that gap.
                        if host.closing.is_cancelled() {
                            live.status = SessionStatus::Closing;
                            handle.cancel.cancel();
                        }
                        Ok(Some((handle, id)))
                    })
                    .await
                    .and_then(|r| r);
                let (mut handle, task_id) = match start {
                    Ok(Some(start)) => start,
                    Ok(None) => {
                        let _ = done.send(Ok(None));
                        return;
                    }
                    Err(e) => {
                        let _ = done.send(Err(e));
                        return;
                    }
                };
                let task_cancel = handle.cancel.clone();
                if request_cancel.is_cancelled() {
                    task_cancel.cancel();
                }
                let mut request_cancelled = request_cancel.is_cancelled();
                let mut cancelled_at = None;
                let mut timed_out = false;
                let mut cancel_consumed = false;
                loop {
                    let timeout = async {
                        match cancelled_at {
                            Some(t) => tokio::time::sleep_until(t).await,
                            None => std::future::pending().await,
                        }
                    };
                    tokio::select! {biased;
                                           _=timeout=>{timed_out=true;break},
                                           _=request_cancel.cancelled(),if !request_cancelled=>{request_cancelled=true;task_cancel.cancel();},
                                           _=task_cancel.cancelled(),if !cancel_consumed=>{cancel_consumed=true;cancelled_at=host.config.cleanup_timeout.map(|t|tokio::time::Instant::now()+t);},
                                           event=handle.outbox.recv()=>match event{Some(event)=>{if let Out::Ask{id,..}=&event{host.live.lock().unwrap().asks.insert(id.clone());}
                    if tx.send(event).is_err(){task_cancel.cancel();}},None=>break}
                                       }
                }
                let mut result = if timed_out {
                    handle.abort().await;
                    Err(TurnFailure::from(error(
                        "host",
                        "turn cleanup timed out; session quarantined",
                    )))
                } else {
                    handle.join().await
                };
                let uncertain = timed_out
                    || matches!(&result, Err(failure) if matches!(failure.error.as_ref(), YourAiError::Error(ErrorKind::LoopTerminated(_))));
                let settled = host
                    .blocking(move |host| {
                        let mut journal = host.journal();
                        let output = match &mut result {
                            Ok(o) => o,
                            Err(e) => &mut e.output,
                        };
                        let pending = std::mem::take(&mut output.pending);
                        if !uncertain {
                            for input in pending.into_iter().rev() {
                                if matches!(input, In::UserText { .. }) {
                                    journal.queue.push_front(input);
                                }
                            }
                        }
                        // On force-drop every submitted active input is already
                        // in the durable ledger, including unread inbox entries.
                        // Keep it once for inspection; never replay uncertain work.
                        if uncertain {
                            let uncertain = std::mem::take(&mut journal.active);
                            journal.interrupted.extend(uncertain);
                        }
                        journal.active.clear();
                        journal.last_error = result.as_ref().err().map(ToString::to_string);
                        if uncertain {
                            host.closing.cancel();
                        }
                        if host.events.pending().iter().any(|e| e.wake)
                            && journal.queue.is_empty()
                            && host.config.allow_background_wake
                        {
                            journal.queue.push_back(In::follow_up(RUNTIME_EVENT_PROMPT));
                        }
                        if let Err(e) = journal.commit() {
                            journal.retain_for_recovery(&e);
                            let output = match result {
                                Ok(o) => o,
                                Err(f) => f.output,
                            };
                            result = Err(TurnFailure::new(e, output));
                        }
                        let mut live = host.live.lock().unwrap();
                        live.inbox = None;
                        live.cancel = None;
                        live.asks.clear();
                        live.status = if host.closing.is_cancelled() {
                            SessionStatus::Closing
                        } else {
                            SessionStatus::Idle
                        };
                        result
                    })
                    .await;
                let result = settled.unwrap_or_else(|e| Err(TurnFailure::from(e)));
                // Stop hooks have settled and history + host journal are committed.
                // This notification cannot change the turn outcome or request continuation.
                if result.is_ok() {
                    host.notify_turn_completed(&task_id, after_seq, &tx).await;
                }
                let _ = done.send(Ok(Some(SessionTurn {
                    turn_id: task_id,
                    result,
                })));
                host.notify.notify_waiters();
            });
            while let Some(event) = tokio::select! {biased;_=cancel.cancelled()=>{guard.0.cancel();rx.recv().await},_=outbox.closed()=>{guard.0.cancel();rx.recv().await},e=rx.recv()=>e}
            {
                if !outbox.send(event) {
                    guard.0.cancel();
                }
            }
            let report = result.await.map_err(|e| error("host", e))?;
            drop(guard);
            report
        })
    }
    fn interrupt(&self) {
        if let Some(cancel) = &self.live.lock().unwrap().cancel {
            cancel.cancel();
        }
    }
    fn compact<'a>(
        &'a self,
        request: CompactionRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<CompactionResult, YourAiError>> {
        self.compact_with_events(request, cancel, &yourai_core::context::DiscardSink)
    }
    fn close<'a>(
        &'a self,
        timeout: Option<Duration>,
    ) -> BoxFuture<'a, Result<Vec<In>, YourAiError>> {
        Box::pin(async move {
            self.finish_close(timeout).await?;
            Ok(self.take_closed_inputs())
        })
    }
}
impl SessionHost {
    pub(crate) fn take_closed_inputs(&self) -> Vec<In> {
        std::mem::take(&mut self.live.lock().unwrap().closed_pending)
    }
    pub(crate) fn close_warning(&self, error: &YourAiError) {
        self.live.lock().unwrap().journal.last_error = Some(error.to_string());
    }
    async fn run_shutdown(&self, timeout: Option<Duration>) -> Result<(), YourAiError> {
        if let Some(ws) = self.workspace.get() {
            ws.stop_watching().await;
        }
        let children: Vec<_> = self
            .children
            .lock()
            .unwrap()
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .collect();
        for child in children {
            child.close(self.config.cleanup_timeout.or(timeout)).await?;
        }
        let tasks = std::mem::take(&mut *self.background.lock().unwrap());
        for t in &tasks {
            t.abort();
        }
        for t in tasks {
            let _ = t.await;
        }
        Ok(())
    }
    async fn run_commit_close(
        &self,
        gate: tokio::sync::OwnedMutexGuard<()>,
        hook_error: Option<String>,
    ) -> Result<(), YourAiError> {
        self.blocking(move |host| {
            let _gate = gate;
            let mut journal = host.journal();
            if let Some(error) = hook_error {
                journal.last_error = Some(error);
            }
            let pending: Vec<_> = journal.queue.drain(..).collect();
            journal.closed = true;
            if let Err(e) = journal.commit() {
                journal.queue.extend(pending);
                journal.closed = false;
                return Err(e);
            }
            // Retain the handoff before any fallible/cancellable step.
            host.live.lock().unwrap().closed_pending.extend(pending);
            FileExt::unlock(&host._lock).map_err(|e| error("host", e))?;
            host.live.lock().unwrap().status = SessionStatus::Closed;
            Ok(())
        })
        .await?
    }
}
impl Drop for SessionHost {
    fn drop(&mut self) {
        self.closing.cancel();
        if let Some(c) = &self.live.get_mut().unwrap().cancel {
            c.cancel();
        }
        for task in self.background.get_mut().unwrap().drain(..) {
            task.abort();
        }
    }
}

// Public convenience entry points share the same concrete assembly code.
impl SessionHost {
    pub async fn create(
        root: &Path,
        cwd: PathBuf,
        model: Arc<dyn ModelProvider>,
        hooks: Option<Arc<dyn HookRuntime>>,
        tools: Option<Arc<dyn ToolRegistry>>,
    ) -> Result<Arc<Self>, YourAiError> {
        Self::open_config(
            crate::HarnessConfig::new(root.into(), cwd),
            model,
            hooks,
            tools,
            "startup",
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    pub async fn restore(
        root: &Path,
        id: SessionId,
        cwd: PathBuf,
        model: Arc<dyn ModelProvider>,
        hooks: Option<Arc<dyn HookRuntime>>,
        tools: Option<Arc<dyn ToolRegistry>>,
        policy: ContextPolicy,
        source: &str,
    ) -> Result<Arc<Self>, YourAiError> {
        let mut config = crate::HarnessConfig::new(root.into(), cwd);
        config.resume = Some(id);
        config.context_policy = policy;
        Self::open_config(config, model, hooks, tools, source).await
    }
    async fn open_config(
        config: crate::HarnessConfig,
        model: Arc<dyn ModelProvider>,
        hooks: Option<Arc<dyn HookRuntime>>,
        tools: Option<Arc<dyn ToolRegistry>>,
        source: &str,
    ) -> Result<Arc<Self>, YourAiError> {
        let catalog = Arc::new(crate::SessionCatalog::new(&config.root)?);
        let crate::assembly::PreparedSession { id, lease, notices } =
            crate::assembly::prepare_session(&catalog, &config, model.as_ref()).await?;
        let usage = Arc::new(crate::storage::LocalUsage((*catalog.store).clone()));
        let (agent, _) = crate::assembly::assemble(
            &catalog,
            &id,
            &config.cwd,
            false,
            model,
            hooks,
            Some(usage),
            tools,
            config.context_policy,
        )
        .await?;
        let mut context = SessionContext::new(id, config.cwd);
        context.transcript_path = Some(crate::SqliteStore::path(&config.root));
        let host = Self::open_owned(lease, context, agent, HostConfig::default(), source).await?;
        for notice in notices {
            host.post_event_async(yourai_core::runtime_event::RuntimeEvent {
                id: uuid::Uuid::new_v4().to_string(),
                context: None,
                notice: Some(notice),
                wake: false,
            })
            .await?;
        }
        Ok(host)
    }
}
