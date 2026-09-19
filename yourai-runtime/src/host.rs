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
    pub hook_timeout: Duration,
    pub cleanup_timeout: Duration,
    pub allow_background_wake: bool,
    pub max_followups: usize,
    pub instruction_paths: Vec<PathBuf>,
    pub workspace_enabled: bool,
}
impl Default for HostConfig {
    fn default() -> Self {
        Self {
            hook_timeout: Duration::from_secs(30),
            cleanup_timeout: Duration::from_secs(10),
            allow_background_wake: true,
            max_followups: 64,
            instruction_paths: vec![],
            workspace_enabled: false,
        }
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
}
pub struct SessionHost {
    self_ref: std::sync::OnceLock<std::sync::Weak<SessionHost>>,
    pub(crate) context: Mutex<SessionContext>,
    pub(crate) agent: Arc<Agent>,
    pub(crate) dir: PathBuf,
    config: HostConfig,
    live: Mutex<Live>,
    events: Arc<RuntimeEvents>,
    operation: Arc<tokio::sync::Mutex<()>>,
    workspace: std::sync::OnceLock<Arc<crate::extensions::Workspace>>,
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
        if !self.0.closing.is_cancelled() {
            l.status = SessionStatus::Idle;
        }
    }
}
impl SessionHost {
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
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(|e| error("host", e))?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("host.lock"))
            .map_err(|e| error("host", e))?;
        lock.try_lock_exclusive().map_err(|e| error("host", e))?;
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
            config,
            live: Mutex::new(Live {
                journal,
                status: SessionStatus::Idle,
                inbox: None,
                cancel: None,
                asks: HashSet::new(),
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
        host.persist()?;
        host.attach_background();
        if host.dir.join("config.json").exists() {
            read_json::<crate::extensions::RuntimeConfig>(&host.dir.join("config.json"))?
                .apply(&host);
        }
        if host.config.workspace_enabled || !host.config.instruction_paths.is_empty() {
            let workspace = host.workspace()?;
            workspace
                .setup(if source == "startup" {
                    "init"
                } else {
                    "maintenance"
                })
                .await?;
            for path in &host.config.instruction_paths {
                workspace.load_instructions(path, source).await?;
            }
        }
        if was_interrupted {
            host.reconcile_history().await?;
            host.post_event(RuntimeEvent{id:format!("recovered-{}",uuid::Uuid::new_v4()),context:None,notice:Some("Previous execution was interrupted; tools were not replayed. Inspect interrupted_inputs() before continuing.".into()),wake:false})?;
        }
        let result = host
            .dispatch(HookEvent::SessionStart {
                source: source.into(),
                model: host.agent.ctx().try_model().map(|m| m.model_iden().into()),
            })
            .await?;
        host.consume_hook(&result, false)?;
        if let HookPointOutcome::SessionStart(o) = result.outcome {
            for path in o.watch_paths {
                host.watch_path(PathBuf::from(path))?;
            }
            if let Some(message) = o.initial_user_message {
                host.submit(In::user_text(message))
                    .map_err(|e| error("host", e))?;
            }
        }
        if !host.watch_paths().is_empty() {
            host.workspace()?
                .start_watching(Duration::from_millis(250))?;
        }
        Ok(host)
    }
    pub async fn create(
        root: &Path,
        cwd: PathBuf,
        model: Arc<dyn ModelProvider>,
        hooks: Option<Arc<dyn HookRuntime>>,
        tools: Option<Arc<dyn ToolRegistry>>,
    ) -> Result<Arc<Self>, YourAiError> {
        let catalog = Arc::new(crate::SessionCatalog::new(root)?);
        let meta = catalog.create_session().await?;
        Self::restore(
            root,
            meta.id,
            cwd,
            model,
            hooks,
            tools,
            ContextPolicy::default(),
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
        let catalog = Arc::new(crate::SessionCatalog::new(root)?);
        let mut meta = catalog.load_session(&id).await?;
        meta.model = Some(model.model_iden().into());
        catalog.save_session(&meta).await?;
        let dir = catalog.directory(&id)?;
        let usage = Arc::new(crate::local::LocalUsage((*catalog.store).clone()));
        let (agent, _) = crate::harness::assemble(
            &catalog,
            &id,
            model,
            hooks,
            Some(usage),
            tools,
            policy,
            None,
        )
        .await?;
        let mut context = SessionContext::new(id, cwd);
        context.transcript_path = Some(crate::SqliteStore::path(root));
        Self::open(dir, context, agent, HostConfig::default(), source).await
    }
    fn persist_locked(&self, l: &Live) -> Result<(), YourAiError> {
        let mut journal = l.journal.clone();
        journal.events = self.events.pending();
        atomic_write(&self.dir.join("host.json"), &journal)
    }
    fn persist(&self) -> Result<(), YourAiError> {
        self.persist_locked(&self.live.lock().unwrap())
    }
    pub fn workspace(self: &Arc<Self>) -> Result<Arc<crate::extensions::Workspace>, YourAiError> {
        if let Some(ws) = self.workspace.get() {
            return Ok(ws.clone());
        }
        let ws = crate::extensions::Workspace::new(self)?;
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
        let mut l = self.live.lock().unwrap();
        if !l.journal.watch.contains(&path) {
            l.journal.watch.push(path);
        }
        self.persist_locked(&l)?;
        drop(l);
        if let Some(host) = self.self_ref.get().and_then(std::sync::Weak::upgrade) {
            host.workspace()?
                .start_watching(Duration::from_millis(250))?;
        }
        Ok(())
    }
    pub fn post_event(&self, event: RuntimeEvent) -> Result<bool, YourAiError> {
        let mut l = self.live.lock().unwrap();
        if self.closing.is_cancelled() {
            return Err(error("host", "session closing"));
        }
        if l.journal.seen.contains(&event.id) {
            return Ok(false);
        }
        let old = l.journal.clone();
        l.journal.seen.insert(event.id.clone());
        l.journal.events = self.events.pending();
        l.journal.events.push(event.clone());
        if event.wake
            && self.config.allow_background_wake
            && matches!(l.status, SessionStatus::Idle)
            && l.journal.queue.is_empty()
        {
            l.journal
                .queue
                .push_back(In::follow_up("Process the pending runtime event."));
        }
        if let Err(e) = atomic_write(&self.dir.join("host.json"), &l.journal) {
            l.journal = old;
            return Err(e);
        }
        self.events.push(event);
        self.notify.notify_one();
        Ok(true)
    }
    pub(crate) async fn dispatch(
        &self,
        event: HookEvent,
    ) -> Result<HookDispatchResult, YourAiError> {
        let Some(hooks) = self.agent.ctx().try_hooks() else {
            return Ok(HookDispatchResult::empty(event.kind()));
        };
        let c = self.context();
        let mut base = BaseInput::new(&c.id.0, c.cwd.to_string_lossy());
        base.transcript_path = c
            .transcript_path
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let invocation = HookInvocation::new(base, event);
        let result = tokio::time::timeout(self.config.hook_timeout, hooks.dispatch(&invocation))
            .await
            .map_err(|_| error("hook", "host hook timed out"))??;
        if result.event != invocation.event.kind() {
            return Err(error("hook", "mismatched event"));
        }
        Ok(result)
    }
    pub(crate) fn consume_hook(
        &self,
        r: &HookDispatchResult,
        deny_block: bool,
    ) -> Result<(), YourAiError> {
        if r.common.prevent_continuation || (deny_block && !r.common.blocking_errors.is_empty()) {
            return Err(
                AbortReason::HookStopped(r.common.stop_reason.clone().unwrap_or_else(|| {
                    r.common
                        .blocking_errors
                        .iter()
                        .map(|e| e.message.clone())
                        .collect::<Vec<_>>()
                        .join("\n")
                }))
                .into(),
            );
        }
        let contexts = match &r.outcome {
            HookPointOutcome::SessionStart(o) => o.additional_contexts.clone(),
            HookPointOutcome::Generic(o) => o.additional_contexts.clone(),
            _ => vec![],
        };
        let mut notices = r.common.system_messages.clone();
        notices.extend(
            r.common
                .messages
                .iter()
                .filter(|m| {
                    !r.runs
                        .iter()
                        .any(|run| run.hook_id == m.hook_id && run.suppress_output)
                })
                .map(|m| m.content.clone()),
        );
        if !contexts.is_empty() || !notices.is_empty() {
            self.post_event(RuntimeEvent {
                id: uuid::Uuid::new_v4().to_string(),
                context: (!contexts.is_empty()).then(|| contexts.join("\n")),
                notice: (!notices.is_empty()).then(|| notices.join("\n")),
                wake: false,
            })?;
        }
        Ok(())
    }
    fn attach_background(self: &Arc<Self>) {
        let Some(mut rx) = self
            .agent
            .ctx()
            .try_hooks()
            .and_then(|h| h.subscribe_background())
        else {
            return;
        };
        let weak = Arc::downgrade(self);
        let cancel = self.closing.clone();
        let task = tokio::spawn(async move {
            loop {
                let event = tokio::select! {_=cancel.cancelled()=>break,e=rx.recv()=>e};
                let Some(host) = weak.upgrade() else { break };
                match event {
                    Ok(e) if e.session_id == host.context().id.0 => {
                        let wake = e.rewake && e.exit_code == 2;
                        let text = if e.stderr.is_empty() {
                            e.stdout
                        } else {
                            format!("{}\n{}", e.stdout, e.stderr)
                        };
                        if let Err(err) = host.post_event(RuntimeEvent {
                            id: e.task_id,
                            context: wake.then(|| text.clone()),
                            notice: Some(text),
                            wake,
                        }) {
                            host.live.lock().unwrap().journal.last_error = Some(err.to_string());
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        let _ = host.post_event(RuntimeEvent {
                            id: uuid::Uuid::new_v4().to_string(),
                            context: None,
                            notice: Some(format!("Lost {n} background events; inspect hook logs")),
                            wake: false,
                        });
                    }
                    Err(_) => break,
                }
            }
        });
        self.background.lock().unwrap().push(task);
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
                self.persist()?;
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
        if self.closing.is_cancelled() {
            return Err(error("host", "session closing"));
        }
        self.operation
            .clone()
            .try_lock_owned()
            .map_err(|_| error("host", "session is busy"))
    }
    pub(crate) fn set_cwd(&self, cwd: PathBuf) -> Result<(), YourAiError> {
        let mut l = self.live.lock().unwrap();
        l.journal.cwd = cwd.clone();
        self.persist_locked(&l)?;
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
    fn submit(&self, input: In) -> Result<(), InputRejected> {
        let mut l = self.live.lock().unwrap();
        let reject = |reason: String| InputRejected {
            input: input.clone(),
            reason,
        };
        if self.closing.is_cancelled() {
            return Err(reject("session closing".into()));
        }
        if let In::Reply { id, .. } = &input {
            if !l.asks.remove(id) {
                return Err(reject("no matching active interaction".into()));
            }
            return l
                .inbox
                .as_ref()
                .ok_or_else(|| reject("no active turn".into()))?
                .send(input.clone())
                .map_err(|_| reject("turn inbox closed".into()));
        }
        let steer = matches!(
            input,
            In::UserText {
                mode: InputMode::Steer,
                ..
            }
        ) && l.inbox.is_some();
        let old = l.journal.clone();
        if steer {
            l.journal.active.push(input.clone());
        } else {
            l.journal.queue.push_back(input.clone());
        }
        if let Err(e) = self.persist_locked(&l) {
            l.journal = old;
            return Err(reject(e.to_string()));
        }
        if steer && l.inbox.as_ref().unwrap().send(input.clone()).is_err() {
            l.journal.active.pop();
            l.journal.queue.push_back(input);
            if let Err(e) = self.persist_locked(&l) {
                l.journal.last_error = Some(e.to_string());
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
            let (mut handle, turn_id) = {
                let mut l = self.live.lock().unwrap();
                let Some(first) = l.journal.queue.pop_front() else {
                    return Ok(None);
                };
                l.journal.active = vec![first.clone()];
                if let Err(e) = self.persist_locked(&l) {
                    l.journal.active.clear();
                    l.journal.queue.push_front(first);
                    return Err(e);
                }
                let mut options = TurnOptions::default();
                options.session = Some(Arc::new(self.context()));
                options.limits = limits;
                options.events = Some(self.events.clone());
                let handle = match self.agent.start_with(first.clone(), options) {
                    Ok(h) => h,
                    Err(e) => {
                        l.journal.active.clear();
                        l.journal.queue.push_front(first);
                        self.persist_locked(&l)?;
                        return Err(e);
                    }
                };
                let id = handle.info.id.clone();
                l.status = SessionStatus::Running {
                    turn_id: id.clone(),
                };
                l.inbox = Some(handle.inbox.clone());
                l.cancel = Some(handle.cancel.clone());
                (handle, id)
            };
            let task_cancel = handle.cancel.clone();
            let guard = CancelGuard(task_cancel.clone());
            let (tx, mut rx) = mpsc::unbounded_channel();
            let (done, result) = oneshot::channel();
            let host = self
                .self_ref
                .get()
                .and_then(std::sync::Weak::upgrade)
                .ok_or_else(|| error("host", "host released"))?;
            let task_id = turn_id.clone();
            tokio::spawn(async move {
                let _gate = gate;
                let mut cancelled_at = None;
                let mut timed_out = false;
                loop {
                    let timeout = async {
                        match cancelled_at {
                            Some(t) => tokio::time::sleep_until(t).await,
                            None => std::future::pending().await,
                        }
                    };
                    tokio::select! {biased;
                                           _=timeout=>{timed_out=true;break},
                                           _=task_cancel.cancelled(),if cancelled_at.is_none()=>{cancelled_at=Some(tokio::time::Instant::now()+host.config.cleanup_timeout);},
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
                {
                    let mut l = host.live.lock().unwrap();
                    l.inbox = None;
                    l.cancel = None;
                    l.asks.clear();
                    let output = match &mut result {
                        Ok(o) => o,
                        Err(e) => &mut e.output,
                    };
                    let pending = std::mem::take(&mut output.pending);
                    for input in pending.into_iter().rev() {
                        if matches!(input, In::UserText { .. }) {
                            l.journal.queue.push_front(input);
                        }
                    }
                    if timed_out {
                        let uncertain = std::mem::take(&mut l.journal.active);
                        l.journal.interrupted.extend(uncertain);
                    }
                    l.journal.active.clear();
                    l.journal.last_error = result.as_ref().err().map(ToString::to_string);
                    if timed_out {
                        host.closing.cancel();
                    }
                    l.status = if host.closing.is_cancelled() {
                        SessionStatus::Closing
                    } else {
                        SessionStatus::Idle
                    };
                    if host.events.pending().iter().any(|e| e.wake)
                        && l.journal.queue.is_empty()
                        && host.config.allow_background_wake
                    {
                        l.journal
                            .queue
                            .push_back(In::follow_up("Process the pending runtime event."));
                    }
                    if let Err(e) = host.persist_locked(&l) {
                        host.closing.cancel();
                        l.status = SessionStatus::Closing;
                        let output = match result {
                            Ok(o) => o,
                            Err(f) => f.output,
                        };
                        result = Err(TurnFailure::new(e, output));
                    }
                }
                let _ = done.send(SessionTurn {
                    turn_id: task_id,
                    result,
                });
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
            Ok(Some(report))
        })
    }
    fn interrupt(&self) {
        if let Some(cancel) = &self.live.lock().unwrap().cancel {
            cancel.cancel();
        }
    }
    fn compact<'a>(
        &'a self,
        mut request: CompactionRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<CompactionResult, YourAiError>> {
        Box::pin(async move {
            let _gate = self.try_operation()?;
            self.live.lock().unwrap().status = SessionStatus::Compacting;
            let _status = StatusGuard(self);
            let deadline = request
                .deadline
                .unwrap_or_else(|| std::time::Instant::now() + Duration::from_secs(120));
            request.deadline = Some(deadline);
            let token = cancel.child_token();
            let _cancel_guard = token.clone().drop_guard();
            let task = async {
                request.trigger = CompactionTrigger::Manual;
                let snapshot = self.agent.ctx().snapshot()?;
                let context = self.context();
                let history = snapshot
                    .context_manager
                    .clone()
                    .ok_or_else(|| error("compact", "context not configured"))?;
                history.restore().await?;
                request.tools = snapshot
                    .tools
                    .as_ref()
                    .map(|r| r.definitions())
                    .unwrap_or_default();
                request.system = Some(
                    snapshot
                        .agent_loop
                        .request_system(&snapshot, Some(&context))
                        .await?,
                );
                let execution = ContextExecution::from_snapshot(
                    &snapshot,
                    history.session_id(),
                    Some(&context),
                )?;
                history.compact(request, &execution, &token).await
            };
            tokio::pin!(task);
            tokio::select! { r=&mut task=>r, _=self.closing.cancelled()=>{ token.cancel(); task.await }, _=cancel.cancelled()=>Err(AbortReason::Cancelled.into()), _=tokio::time::sleep_until(deadline.into())=>Err(AbortReason::DeadlineExceeded.into()) }
        })
    }
    fn close<'a>(&'a self, timeout: Duration) -> BoxFuture<'a, Result<Vec<In>, YourAiError>> {
        Box::pin(async move {
            if self.status() == SessionStatus::Closed {
                return Ok(vec![]);
            }
            self.closing.cancel();
            self.interrupt();
            {
                let mut live = self.live.lock().unwrap();
                if live.status == SessionStatus::Closed {
                    return Ok(vec![]);
                }
                live.status = SessionStatus::Closing;
            }
            tokio::time::timeout(timeout, async {
                let _gate = self.operation.lock().await;
                if self.status() == SessionStatus::Closed {
                    return Ok(vec![]);
                }
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
                    child.close(self.config.cleanup_timeout).await?;
                }
                let tasks = std::mem::take(&mut *self.background.lock().unwrap());
                for t in &tasks {
                    t.abort();
                }
                for t in tasks {
                    let _ = t.await;
                }
                // Closing hooks may report errors, but cannot prevent resource release.
                let hook_result = tokio::time::timeout(
                    timeout / 4,
                    self.dispatch(HookEvent::SessionEnd {
                        reason: "shutdown".into(),
                    }),
                )
                .await
                .map_err(|_| error("hook", "SessionEnd cleanup deadline exceeded"))
                .and_then(|r| r);
                if let Some(hooks) = self.agent.ctx().try_hooks() {
                    hooks.shutdown_session(&self.context().id.0).await?;
                }
                let mut l = self.live.lock().unwrap();
                if let Err(e) = hook_result {
                    l.journal.last_error = Some(e.to_string());
                }
                let pending: Vec<_> = l.journal.queue.drain(..).collect();
                l.journal.closed = true;
                if let Err(e) = self.persist_locked(&l) {
                    l.journal.queue.extend(pending);
                    l.journal.closed = false;
                    return Err(e);
                }
                FileExt::unlock(&self._lock).map_err(|e| error("host", e))?;
                l.status = SessionStatus::Closed;
                Ok(pending)
            })
            .await
            .map_err(|_| error("host", "close timed out; session remains Closing"))?
        })
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
