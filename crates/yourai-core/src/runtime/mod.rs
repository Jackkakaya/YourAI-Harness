#[cfg(test)]
mod input_tests;
mod journal;
mod lifecycle;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
pub(crate) mod test_support;
use crate::{
    error,
    file_store::{atomic_write, read_json},
};
use crate::{
    prelude::*,
    runtime_event::{RuntimeEvent, RuntimeEvents},
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct HostConfig {
    pub input: crate::turn::InputOptions,
    pub hook_timeout: Option<Duration>,
    /// Optional supervisor quarantine bound after cancellation.
    pub cleanup_timeout: Option<Duration>,
    pub allow_background_wake: bool,
    pub max_followups: usize,
    pub instruction_paths: Vec<PathBuf>,
    pub workspace_enabled: bool,
}
impl Default for HostConfig {
    fn default() -> Self {
        Self {
            input: crate::turn::InputOptions::default(),
            hook_timeout: None,
            cleanup_timeout: None,
            allow_background_wake: true,
            max_followups: 64,
            instruction_paths: vec![],
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
pub struct SessionLease {
    pub dir: PathBuf,
    lock: File,
}
impl SessionLease {
    pub fn acquire(dir: PathBuf) -> Result<Self, YourAiError> {
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
    session_end: CloseNotification,
}
#[derive(Default)]
enum CloseNotification {
    #[default]
    Pending,
    Started,
    Finished(Option<String>),
}
pub struct SessionHost {
    self_ref: std::sync::OnceLock<std::sync::Weak<SessionHost>>,
    pub(crate) context: Mutex<SessionContext>,
    agent: Arc<Agent>,
    pub(crate) dir: PathBuf,
    config: HostConfig,
    input_options: Mutex<crate::turn::InputOptions>,
    live: Mutex<Live>,
    journal_gate: Mutex<()>,
    events: Arc<RuntimeEvents>,
    operation: Arc<tokio::sync::Mutex<()>>,
    workspace: std::sync::OnceLock<Arc<crate::workspace::Workspace>>,
    pub(crate) tasks: std::sync::OnceLock<Arc<crate::tasks::TaskManager>>,
    children: Mutex<HashMap<SessionId, Arc<SessionHost>>>,
    active: Mutex<usize>,
    settled: Notify,
    notify: Notify,
    closing: CancellationToken,
    background: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    _lock: File,
}
pub(crate) struct Activity {
    host: Arc<SessionHost>,
}
impl Drop for Activity {
    fn drop(&mut self) {
        *self.host.active.lock().unwrap() -= 1;
        self.host.settled.notify_waiters();
    }
}
struct StatusGuard(Arc<SessionHost>);
impl Drop for StatusGuard {
    fn drop(&mut self) {
        let mut l = self.0.live.lock().unwrap();
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
    pub fn agent(&self) -> &Arc<Agent> {
        &self.agent
    }
    pub fn directory(&self) -> &Path {
        &self.dir
    }
    pub(crate) fn configure_input(&self, skill_ids: Vec<String>, memory_search_limit: usize) {
        *self.input_options.lock().unwrap() = crate::turn::InputOptions {
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
            .map(|r| {
                r.snapshot()
                    .into_iter()
                    .map(|tool| tool.definition())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let model = snapshot
            .model
            .as_ref()
            .ok_or_else(|| error("context", "model not configured"))?;
        let request = history.build_request(&tools, model.as_ref(), &[])?;
        let budget = model.token_budget();
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

    fn run_open(
        lease: SessionLease,
        context: SessionContext,
        agent: Arc<Agent>,
        config: HostConfig,
    ) -> Result<(Arc<Self>, bool), YourAiError> {
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
        for input in &mut journal.queue {
            input.ensure_id();
        }
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
            input_options: Mutex::new(crate::turn::InputOptions {
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
                session_end: CloseNotification::Pending,
            }),
            events,
            operation: Arc::new(tokio::sync::Mutex::new(())),
            workspace: std::sync::OnceLock::new(),
            tasks: std::sync::OnceLock::new(),
            children: Mutex::new(HashMap::new()),
            active: Mutex::new(0),
            settled: Notify::new(),
            notify: Notify::new(),
            closing: CancellationToken::new(),
            background: Mutex::new(vec![]),
            _lock: lock,
        });
        let _ = host.self_ref.set(Arc::downgrade(&host));
        Ok((host, was_interrupted))
    }
    pub fn workspace(self: &Arc<Self>) -> Result<Arc<crate::workspace::Workspace>, YourAiError> {
        let _write = self.resource_lock()?;
        if self.workspace.get().is_none() {
            let ws = crate::workspace::Workspace::new(self)?;
            let _ = self.workspace.set(ws);
        }
        Ok(self.workspace.get().unwrap().clone())
    }
    pub fn task_manager(
        self: &Arc<Self>,
        team: impl Into<String>,
    ) -> Result<Arc<crate::tasks::TaskManager>, YourAiError> {
        let _write = self.resource_lock()?;
        let team = team.into();
        if self.tasks.get().is_none() {
            let manager = crate::tasks::TaskManager::open(self, team.clone())?;
            let _ = self.tasks.set(manager);
        }
        let manager = self.tasks.get().unwrap().clone();
        if manager.team != team {
            return Err(error("tasks", "session already has a different task team"));
        }
        Ok(manager)
    }

    /// Serialize resource writes with final close/unlock, then reject a closed owner.
    pub(crate) fn resource_lock(&self) -> Result<std::sync::MutexGuard<'_, ()>, YourAiError> {
        let guard = self.journal_gate.lock().unwrap();
        self.ensure_open()?;
        Ok(guard)
    }
    /// An already started effect must settle while close waits for its activity.
    pub(crate) fn resource_settlement(&self) -> Result<std::sync::MutexGuard<'_, ()>, YourAiError> {
        let guard = self.journal_gate.lock().unwrap();
        if self.status() == SessionStatus::Closed {
            return Err(error("host", "session closed"));
        }
        Ok(guard)
    }
    pub(crate) fn activity(self: &Arc<Self>) -> Result<Activity, YourAiError> {
        let mut active = self.active.lock().unwrap();
        self.ensure_open()?;
        *active += 1;
        Ok(Activity { host: self.clone() })
    }
    pub(crate) fn closing(&self) -> &CancellationToken {
        &self.closing
    }
    async fn wait_active(&self) {
        loop {
            let notified = self.settled.notified();
            if *self.active.lock().unwrap() == 0 {
                return;
            }
            notified.await;
        }
    }
    pub(crate) fn register_child(&self, child: &Arc<Self>) -> Result<(), YourAiError> {
        let mut children = self.children.lock().unwrap();
        children.insert(child.context().id, child.clone());
        self.ensure_open()?;
        Ok(())
    }
    pub(crate) fn release_child(&self, id: &SessionId) {
        let mut children = self.children.lock().unwrap();
        if children
            .get(id)
            .is_some_and(|child| child.status() == SessionStatus::Closed)
        {
            children.remove(id);
        }
    }
    pub fn child_ids(&self) -> Vec<SessionId> {
        self.children.lock().unwrap().keys().cloned().collect()
    }
    pub(crate) fn supervise_child(self: &Arc<Self>, child: Arc<Self>) {
        child.interrupt();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let parent = self.clone();
            let task = runtime.spawn(async move {
                let id = child.context().id;
                if let Err(cause) = child.close(None).await {
                    parent.close_warning(&cause);
                }
                parent.release_child(&id);
            });
            self.background.lock().unwrap().push(task);
        }
    }
    pub fn interrupted_inputs(&self) -> Vec<In> {
        self.live.lock().unwrap().journal.interrupted.clone()
    }
    pub fn queued(&self) -> usize {
        self.live.lock().unwrap().journal.queue.len()
    }
    pub fn pending_inputs(&self) -> Vec<In> {
        self.live
            .lock()
            .unwrap()
            .journal
            .queue
            .iter()
            .cloned()
            .collect()
    }
    /// Transfer exactly one queued message into the active turn, keeping its ID and attachments.
    pub async fn steer_pending(&self, id: String) -> Result<bool, YourAiError> {
        self.blocking(move |host| {
            let mut journal = host.journal();
            host.ensure_open()?;
            let Some(index) = journal
                .queue
                .iter()
                .position(|input| input.id() == Some(id.as_str()))
            else {
                return Ok(false);
            };
            let inbox = {
                let live = host.live.lock().unwrap();
                if !matches!(live.status, SessionStatus::Running { .. }) {
                    return Ok(false);
                }
                let Some(inbox) = live.inbox.clone() else {
                    return Ok(false);
                };
                inbox
            };
            let original = journal.queue.remove(index).unwrap();
            let mut input = original.clone();
            let In::UserText { mode, .. } = &mut input else {
                return Ok(false);
            };
            *mode = InputMode::Steer;
            journal.active.push(input.clone());
            journal.commit()?;
            if inbox.send(input).is_err() {
                journal.active.pop();
                journal.queue.insert(index, original);
                if let Err(error) = journal.commit() {
                    journal.retain_for_recovery(&error);
                    return Err(error);
                }
                return Ok(false);
            }
            Ok(true)
        })
        .await?
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
    pub fn try_operation(&self) -> Result<tokio::sync::OwnedMutexGuard<()>, YourAiError> {
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
    pub(crate) fn ensure_open(&self) -> Result<(), YourAiError> {
        if self.closing.is_cancelled() {
            return Err(error("host", "session closing"));
        }
        Ok(())
    }
    pub(crate) fn set_cwd_with_watch(
        &self,
        cwd: PathBuf,
        paths: Vec<String>,
    ) -> Result<(), YourAiError> {
        let mut journal = self.journal();
        self.ensure_open()?;
        journal.cwd = cwd.clone();
        for path in paths {
            let path = PathBuf::from(path);
            let path = if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            };
            if !journal.watch.contains(&path) {
                journal.watch.push(path);
            }
        }
        journal.commit()?;
        self.context.lock().unwrap().cwd = cwd;
        Ok(())
    }
}
impl SessionHost {
    /// Synchronous compatibility entry point; async callers use submit_async.
    pub fn submit(&self, mut input: In) -> Result<(), InputRejected> {
        input.ensure_id();
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
        if journal
            .queue
            .iter()
            .chain(journal.active.iter())
            .any(|old| old.id() == input.id())
        {
            return Err(reject("message ID is already pending or active".into()));
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
    /// 消费下一条排队输入，持有 TurnHandle，转发事件并等待结束。
    ///
    /// - 无输入返回 Ok(None)；忙或关闭时拒绝，不取走排队输入。
    /// - 启动失败返回外层 Err，宿主必须保留尚未启动的输入。
    /// - 运行后无论成功或失败都返回 SessionTurn。
    /// - 返回前将 result 中的 pending **移动**回宿主队列，并清空报告中的
    ///   pending；过滤失效 Reply，不重复保存历史或重复投递用户输入。
    /// - outbox 断开或 cancel 触发时取消当前 Turn 并等待有界清理。
    /// - 此 future 被丢弃时也须请求取消；清理完毕前不得允许重叠运行。
    pub async fn run_next(
        &self,
        limits: TurnLimits,
        outbox: &dyn OutSink,
        cancel: &CancellationToken,
    ) -> Result<Option<SessionTurn>, YourAiError> {
        let gate = self.try_operation()?;
        // A previous failed/cancelled append may have committed in SQLite
        // without updating the memory view. Establish the committed boundary
        // first, so those older records never enter this turn's sync payload.
        let after_seq = if let Some(history) = self.agent.ctx().try_context_manager() {
            tokio::select! { biased;
                _ = cancel.cancelled() => return Err(AbortReason::Cancelled.into()),
                _ = self.closing.cancelled() => return Err(AbortReason::Cancelled.into()),
                _ = crate::time::sleep_until(limits.deadline) => return Err(AbortReason::DeadlineExceeded.into()),
                _ = outbox.closed() => return Err(AbortReason::Disconnected.into()),
                restored = history.restore() => restored?,
            }
            history.last_sequence()
        } else {
            0
        };
        let request_cancel = cancel.child_token();
        let caller_cancel = request_cancel.clone();
        let guard = caller_cancel.clone().drop_guard();
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
                    let options = TurnOptions {
                        session: Some(Arc::new(host.context())),
                        limits,
                        events: Some(host.events.clone()),
                        input: Some(host.input_options.lock().unwrap().clone()),
                    };
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
        while let Some(event) = tokio::select! {biased;_=cancel.cancelled()=>{caller_cancel.cancel();rx.recv().await},_=outbox.closed()=>{caller_cancel.cancel();rx.recv().await},e=rx.recv()=>e}
        {
            if !outbox.send(event) {
                caller_cancel.cancel();
            }
        }
        let report = result.await.map_err(|e| error("host", e))?;
        drop(guard);
        report
    }
    /// 请求取消当前运行；不清空尚未运行的输入，不等价于完成清理。
    pub fn interrupt(&self) {
        if let Some(cancel) = &self.live.lock().unwrap().cancel {
            cancel.cancel();
        }
    }
    /// 手动压缩，与 Turn 历史写入互斥。实现必须经由公共入口
    /// [`crate::context_manager::Compactor::exec`] 执行——它固定持有 Hook 生命周期、
    /// 阻断与取消/提交竞态语义；内部调用 ContextManager 的业务计划与摘要提交。
    /// 直接调用 `prepare_compaction` / `CompactionJob::run` 属于实现协议，不获得该契约。
    pub async fn compact(
        &self,
        mut request: CompactionRequest,
        cancel: &CancellationToken,
    ) -> Result<CompactionResult, YourAiError> {
        let gate = self.try_operation()?;
        let host = self
            .self_ref
            .get()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| error("host", "host released"))?;
        {
            let mut live = self.live.lock().unwrap();
            self.ensure_open()?;
            live.status = SessionStatus::Compacting;
        }
        let token = cancel.child_token();
        let _guard = token.clone().drop_guard();
        // Like run_next, the supervisor retains exclusivity until business settles.
        tokio::spawn(async move {
            let _gate = gate;
            let _status = StatusGuard(host.clone());
            let task = async {
                request.trigger = CompactionTrigger::Manual;
                let snapshot = host.agent.ctx().snapshot()?;
                let context = host.context();
                let history = snapshot
                    .context_manager
                    .clone()
                    .ok_or_else(|| error("compact", "context not configured"))?;
                tokio::select! { biased;
                    _ = token.cancelled() => return Err(AbortReason::Cancelled.into()),
                    _ = host.closing.cancelled() => return Err(AbortReason::Cancelled.into()),
                    _ = crate::time::sleep_until(request.deadline) => return Err(AbortReason::DeadlineExceeded.into()),
                    restored = history.restore() => restored?,
                }
                request.tools = snapshot
                    .tools
                    .as_ref()
                    .map(|r| {
                        r.snapshot()
                            .into_iter()
                            .map(|tool| tool.definition())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                Compactor::from_snapshot(&snapshot, Some(&context))?
                    .exec(request, &token, host.config.hook_timeout)
                    .await
            };
            tokio::pin!(task);
            tokio::select! {
                result = &mut task => result,
                _ = host.closing.cancelled() => { token.cancel(); task.await },
            }
        })
        .await
        .map_err(|e| error("compact", e))?
    }
    /// 幂等关闭：拒绝新输入，取消并清理当前执行，触发 SessionEnd，释放资源。
    /// timeout 为 Some 时限制整个关闭过程，None 等待清理完成；失败时不得伪报 Closed。
    /// 成功时返回未执行输入的快照；队列仍保存在原会话，重开后可继续。
    pub async fn close(&self, timeout: Option<Duration>) -> Result<Vec<In>, YourAiError> {
        self.finish_close(timeout).await?;
        Ok(self.take_closed_inputs())
    }
}
impl SessionHost {
    pub fn take_closed_inputs(&self) -> Vec<In> {
        std::mem::take(&mut self.live.lock().unwrap().closed_pending)
    }
    pub fn close_warning(&self, error: &YourAiError) {
        self.live.lock().unwrap().journal.last_error = Some(error.to_string());
    }
    async fn run_shutdown(&self, timeout: Option<Duration>) -> Result<(), YourAiError> {
        if let Some(ws) = self.workspace.get() {
            ws.stop_watching().await;
        }
        let children: Vec<_> = self.children.lock().unwrap().values().cloned().collect();
        let mut failures = vec![];
        for child in children {
            let id = child.context().id;
            if let Err(cause) = Box::pin(child.close(self.config.cleanup_timeout.or(timeout))).await
            {
                failures.push(cause.to_string());
            }
            self.release_child(&id);
        }
        let tasks = std::mem::take(&mut *self.background.lock().unwrap());
        for t in &tasks {
            t.abort();
        }
        for t in tasks {
            let _ = t.await;
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(error("host", failures.join("\n")))
        }
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
            let pending: Vec<_> = journal.queue.iter().cloned().collect();
            journal.closed = true;
            journal.commit()?;
            // Keep the durable queue in its session; callers receive only a snapshot.
            host.live.lock().unwrap().closed_pending = pending;
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
        for child in self.children.get_mut().unwrap().values() {
            child.interrupt();
        }
        for task in self.background.get_mut().unwrap().drain(..) {
            task.abort();
        }
    }
}
