//! Reusable backend session commands. Execution and lifecycle advance without UI polling.
use crate::{
    config::{Config, Error},
    model::EffortChoice,
    Harness, SessionOptions,
};
use std::{
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};
use tokio::{
    sync::{broadcast, mpsc, watch, Notify},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use yourai_core::prelude::*;
mod state;
use state::{AdmissionError, Command, InputAction, Shared};

#[derive(Clone, Debug, Default)]
pub struct ModelSelection {
    pub label: String,
    pub pricing: Option<(f64, f64)>,
    pub effort: Option<String>,
}
impl ModelSelection {
    pub fn from_config(preferences: &Config) -> Self {
        Self::from_data(&preferences.values())
    }
    fn from_data(data: &serde_json::Value) -> Self {
        let model: String = crate::config::read(data, "model");
        let variant: Option<String> = crate::config::read(data, "variant");
        let pricing = crate::model::configured::pricing_data(data);
        Self {
            label: variant
                .as_ref()
                .map_or_else(|| model.clone(), |v| format!("{model} · {v}")),
            pricing: pricing.0.zip(pricing.1),
            effort: crate::model::configured::effective_effort_data(
                data,
                &model,
                variant.as_deref(),
            ),
        }
    }
}
#[derive(Clone)]
pub struct SessionHistory {
    pub messages: Vec<StoredMessage>,
    pub usage: Usage,
    pub response_count: u64,
    pub title: Option<String>,
}
#[derive(Clone)]
pub struct SessionSnapshot {
    pub revision: u64,
    pub environment: SessionEnvironment,
    pub pending_inputs: Vec<In>,
    pub active: bool,
    pub compacting: bool,
    pub can_steer: bool,
    pub model_metrics: crate::model::BudgetSnapshot,
    pub context_usage: Option<yourai_core::runtime::ContextUsage>,
    pub response_count: Option<u64>,
    pub title: Option<String>,
    pub refresh_error: Option<String>,
}
#[derive(Clone)]
pub enum SessionEvent {
    /// This subscriber lost events; incremental display is incomplete. Read a fresh snapshot.
    Lagged {
        skipped: u64,
    },
    Output(Out),
    Settled,
    Usage(Usage),
    Listed(Result<Vec<SessionMeta>, String>),
    Deleted(SessionId, Result<(), String>),
    Model(Result<ModelSelection, String>),
    Opened(Result<SessionHistory, String>),
}
#[derive(Debug)]
pub enum Reply {
    Sent,
    Rejected(String),
    Deferred,
}
enum Effect {
    Event(SessionEvent),
    Opened(Result<(Harness, SessionHistory), String>),
}
type Stats = (
    Option<yourai_core::runtime::ContextUsage>,
    Option<u64>,
    Result<Option<String>, YourAiError>,
);

type CloseResult = Result<(String, Vec<In>), Error>;

/// Owns the backend worker and its lifetime. Dropping a client does not close the session.
/// The owner must be retained by the embedding application or future server.
pub struct SessionController {
    client: SessionClient,
    shutdown: CancellationToken,
    worker: Option<JoinHandle<CloseResult>>,
}

/// A cloneable command and snapshot handle. It does not own execution or close the session.
#[derive(Clone)]
pub struct SessionClient {
    shared: Arc<Mutex<Shared>>,
    commands: mpsc::UnboundedSender<Command>,
    inputs: mpsc::UnboundedSender<InputAction>,
    snapshot: watch::Receiver<SessionSnapshot>,
    events: Weak<broadcast::Sender<SessionEvent>>,
    config: Arc<Config>,
}

/// The maximum number of events retained for subscribers; this is not a replay journal.
pub const SESSION_EVENT_CAPACITY: usize = 1024;

/// An independent event cursor. Dropping it only unsubscribes this consumer.
pub struct SessionSubscription {
    receiver: broadcast::Receiver<SessionEvent>,
}
impl SessionSubscription {
    pub async fn recv(&mut self) -> Option<SessionEvent> {
        match self.receiver.recv().await {
            Ok(event) => Some(event),
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                Some(SessionEvent::Lagged { skipped })
            }
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }
    pub fn try_recv(&mut self) -> Option<SessionEvent> {
        match self.receiver.try_recv() {
            Ok(event) => Some(event),
            Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                Some(SessionEvent::Lagged { skipped })
            }
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                None
            }
        }
    }
    pub fn has_events(&self) -> bool {
        !self.receiver.is_empty()
    }
}

impl SessionController {
    pub fn new(
        h: Harness,
        config: Arc<Config>,
        template: SessionOptions,
        limits: TurnLimits,
    ) -> (Self, SessionSubscription) {
        bind_config(&h, &config).expect("session config source must be bound once");
        let h = Arc::new(h);
        let (output, rx) = mpsc::unbounded_channel();
        let snapshot = SessionSnapshot {
            revision: 0,
            environment: h.host.environment(),
            pending_inputs: h.host.pending_inputs(),
            active: false,
            compacting: false,
            can_steer: false,
            model_metrics: h.model_snapshot(),
            context_usage: None,
            response_count: None,
            title: None,
            refresh_error: None,
        };
        let wake = Arc::new(Notify::new());
        let shared = Arc::new(Mutex::new(Shared::new(
            h.clone(),
            output.clone(),
            wake.clone(),
        )));
        let (inputs, mut input_rx) = mpsc::unbounded_channel();
        let input_shared = shared.clone();
        let shutdown = CancellationToken::new();
        let input_stop = shutdown.clone();
        let submitter = tokio::spawn(async move {
            loop {
                let input = tokio::select! {
                    biased;
                    _ = input_stop.cancelled(), if !input_rx.is_closed() => {
                        // Reject future sends while draining inputs already accepted.
                        input_rx.close();
                        continue;
                    }
                    input = input_rx.recv() => match input {
                        Some(input) => input,
                        None => break,
                    },
                };
                let (h, output) = {
                    let shared = input_shared.lock().unwrap();
                    (shared.harness(), shared.output())
                };
                match input {
                    InputAction::Submit(input) => {
                        if let Err(rejection) = h.host.submit_async(input).await {
                            let _ = output.send(Out::InputRejected { rejection });
                        }
                    }
                    InputAction::Steer(id) => match h.host.steer_pending(id).await {
                        Ok(true) => {}
                        result => {
                            let message = match result { Ok(false) => "Message is no longer pending or the turn ended; nothing was resent.".into(), Err(e) => format!("Could not steer queued message: {e}"), Ok(true) => unreachable!() };
                            let _ = output.send(Out::Notice {
                                level: Level::Warning,
                                message,
                            });
                        }
                    },
                }
                input_shared.lock().unwrap().finish_input();
            }
        });
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events_tx, receiver) = broadcast::channel(SESSION_EVENT_CAPACITY);
        let events_tx = Arc::new(events_tx);
        let event_source = Arc::downgrade(&events_tx);
        let events = SessionSubscription { receiver };
        let (snapshot_tx, snapshot_rx) = watch::channel(snapshot.clone());
        let mut runner = Runner {
            shared: shared.clone(),
            rx,
            events: events_tx,
            snapshots: snapshot_tx,
            snapshot,
            config: config.clone(),
            template,
            limits,
            wake,
            cancel: CancellationToken::new(),
            driver: None,
            compact: None,
            stats: None,
            stats_at: Instant::now() - Duration::from_secs(2),
            operation: None,
            retired: None,
        };
        runner.resume();
        let stop = shutdown.clone();
        let worker = tokio::spawn(async move {
            runner.run(command_rx, &stop).await;
            // Drain accepted submissions before releasing storage ownership.
            runner.cancel.cancel();
            runner.harness().host.interrupt();
            let _ = submitter.await;
            runner.close().await
        });
        (
            Self {
                client: SessionClient {
                    shared,
                    commands,
                    inputs,
                    snapshot: snapshot_rx,
                    events: event_source,
                    config,
                },
                shutdown,
                worker: Some(worker),
            },
            events,
        )
    }
    pub fn client(&self) -> SessionClient {
        self.client.clone()
    }
    /// Embedding access belongs to the owner, not to connected clients.
    pub fn harness(&self) -> Arc<Harness> {
        self.client.harness()
    }
    pub async fn close(mut self) -> Result<(String, Vec<In>), Error> {
        self.client.shared.lock().unwrap().close();
        self.shutdown.cancel();
        self.worker
            .take()
            .unwrap()
            .await
            .map_err(|e| -> Error { e.into() })?
    }
}
impl Drop for SessionController {
    fn drop(&mut self) {
        self.client.shared.lock().unwrap().close();
        self.shutdown.cancel();
    }
}

impl SessionClient {
    /// Subscribe to future events. Earlier events are not replayed; query the latest snapshot.
    pub fn subscribe(&self) -> Result<SessionSubscription, String> {
        let s = self.shared.lock().unwrap();
        s.check_open().map_err(|e| e.message().to_owned())?;
        self.events
            .upgrade()
            .map(|events| SessionSubscription {
                receiver: events.subscribe(),
            })
            .ok_or_else(|| "Session backend stopped".into())
    }
    fn harness(&self) -> Arc<Harness> {
        self.shared.lock().unwrap().harness()
    }
    pub fn environment(&self) -> SessionEnvironment {
        self.harness().host.environment()
    }
    pub fn yolo(&self) -> bool {
        self.harness().yolo()
    }
    pub fn queued(&self) -> usize {
        self.harness().host.queued()
    }
    pub fn snapshot(&self) -> SessionSnapshot {
        self.snapshot.borrow().clone()
    }
    pub fn busy(&self) -> bool {
        let s = self.shared.lock().unwrap();
        s.busy()
    }
    pub fn operating(&self) -> bool {
        self.shared.lock().unwrap().operating()
    }
    pub fn compacting(&self) -> bool {
        self.shared.lock().unwrap().compacting()
    }
    pub fn submitting(&self) -> bool {
        self.shared.lock().unwrap().submitting()
    }
    pub fn idle(&self) -> bool {
        let s = self.shared.lock().unwrap();
        s.idle()
    }
    pub fn tasks_since(&self, previous: Option<u64>) -> Option<(u64, Vec<Task>)> {
        self.harness().host.tasks_since(previous)
    }
    pub fn submit(&self, input: In) -> bool {
        self.shared
            .lock()
            .unwrap()
            .enqueue(&self.inputs, InputAction::Submit(input))
            .is_ok()
    }
    pub fn reply(&self, id: String, payload: serde_json::Value) -> Reply {
        let mut s = self.shared.lock().unwrap();
        match s.check_input() {
            Err(AdmissionError::Closed) => {
                return Reply::Rejected(AdmissionError::Closed.message().into())
            }
            Err(AdmissionError::Busy) => return Reply::Deferred,
            Ok(()) => {}
        }
        match s.harness().host.submit(In::Reply { id, payload }) {
            Ok(()) => {
                s.request_resume();
                Reply::Sent
            }
            Err(e) => Reply::Rejected(e.to_string()),
        }
    }
    pub fn interrupt(&self) {
        self.shared.lock().unwrap().interrupt();
    }
    pub fn steer_pending(&self, id: String) -> Result<(), String> {
        self.shared
            .lock()
            .unwrap()
            .enqueue(&self.inputs, InputAction::Steer(id))
    }
    fn operation(&self, command: Command) -> Result<(), String> {
        let mut s = self.shared.lock().unwrap();
        if !s.admit(&command)? {
            return Ok(());
        }
        if self.commands.send(command).is_err() {
            s.finish_operation();
            return Err("Session backend stopped".into());
        }
        Ok(())
    }
    pub fn list(&self) -> Result<(), String> {
        self.operation(Command::List)
    }
    pub fn delete(&self, id: SessionId) -> Result<(), String> {
        self.operation(Command::Delete(id))
    }
    pub fn model(
        &self,
        id: String,
        variant: Option<String>,
        effort: Option<EffortChoice>,
    ) -> Result<(), String> {
        self.operation(Command::Model(id, variant, effort))
    }
    pub fn switch(&self, id: Option<SessionId>) -> Result<(), String> {
        self.operation(Command::Switch(id))
    }
    pub fn compact(&self) -> Result<(), String> {
        self.operation(Command::Compact)
    }
    pub fn change_yolo(&self, enabled: bool) -> Result<(), YourAiError> {
        let s = self.shared.lock().unwrap();
        s.check_operation()
            .map_err(|e| YourAiError::from(ErrorKind::Config(e.message().into())))?;
        s.harness().apply_yolo_with_commit(enabled, || {
            let mut data = self.config.write();
            let previous: bool = crate::config::read(&data, "yolo");
            crate::config::put(&mut data, "yolo", enabled);
            if let Err(error) = self.config.save_data(&data) {
                crate::config::put(&mut data, "yolo", previous);
                return Err(crate::error("config", error));
            }
            Ok(())
        })
    }
    pub fn change_theme(&self, theme: &str) -> Result<(), String> {
        self.config.set_theme(theme.into());
        self.config.save().map_err(|e| e.to_string())
    }
    pub fn refresh(&self) {
        let _ = self.commands.send(Command::Refresh);
    }
}

struct Runner {
    shared: Arc<Mutex<Shared>>,
    rx: mpsc::UnboundedReceiver<Out>,
    events: Arc<broadcast::Sender<SessionEvent>>,
    snapshots: watch::Sender<SessionSnapshot>,
    snapshot: SessionSnapshot,
    config: Arc<Config>,
    template: SessionOptions,
    limits: TurnLimits,
    wake: Arc<Notify>,
    cancel: CancellationToken,
    driver: Option<JoinHandle<Result<(), YourAiError>>>,
    compact: Option<JoinHandle<Option<Usage>>>,
    stats: Option<JoinHandle<Stats>>,
    stats_at: Instant,
    operation: Option<JoinHandle<Effect>>,
    retired: Option<JoinHandle<Result<Vec<In>, YourAiError>>>,
}
impl Runner {
    fn harness(&self) -> Arc<Harness> {
        self.shared.lock().unwrap().harness()
    }
    fn output(&self) -> mpsc::UnboundedSender<Out> {
        self.shared.lock().unwrap().output()
    }
    fn notice(&self, level: Level, message: impl Into<String>) {
        let _ = self.events.send(SessionEvent::Output(Out::Notice {
            level,
            message: message.into(),
        }));
    }
    fn resume(&mut self) {
        if self.driver.is_none() {
            self.shared.lock().unwrap().resume_started();
            let host = self.harness().host.clone();
            let tx = self.output();
            let cancel = self.cancel.clone();
            let limits = self.limits.clone();
            self.driver = Some(tokio::spawn(async move {
                host.serve(limits, &tx, &cancel).await
            }));
        }
    }
    fn command(&mut self, command: Command) {
        let h = self.harness();
        match command {
            Command::Refresh => {
                self.stats_at = Instant::now() - Duration::from_secs(2);
            }
            Command::Compact => {
                self.shared.lock().unwrap().start_compaction();
                let tx = self.output();
                let cancel = self.cancel.clone();
                self.compact = Some(tokio::spawn(async move {
                    let result = h
                        .host
                        .compact(CompactionRequest::new(CompactionTrigger::Manual), &cancel)
                        .await;
                    let (level, message) = match result {
                        Ok(r) => {
                            for notice in r.notices {
                                let _ = tx.send(Out::Notice {
                                    level: Level::Warning,
                                    message: notice,
                                });
                            }
                            (
                                Level::Info,
                                format!(
                                    "Context {}: {} -> {} estimated tokens. {}",
                                    if r.compacted {
                                        "Summarized"
                                    } else {
                                        "Unchanged"
                                    },
                                    r.tokens_before,
                                    r.tokens_after,
                                    r.reason
                                ),
                            )
                        }
                        Err(e) => (Level::Error, format!("Compact failed: {e}")),
                    };
                    let _ = tx.send(Out::Notice { level, message });
                    h.usage
                        .session_usage(&h.host.environment().id)
                        .await
                        .ok()
                        .map(|u| Usage {
                            input_tokens: u.total_input_tokens,
                            output_tokens: u.total_output_tokens,
                            total_tokens: u.total_tokens,
                        })
                }));
            }
            Command::List => {
                self.operation = Some(tokio::spawn(async move {
                    Effect::Event(SessionEvent::Listed(
                        h.sessions.list_sessions().await.map_err(|e| e.to_string()),
                    ))
                }))
            }
            Command::Delete(id) => {
                self.operation = Some(tokio::spawn(async move {
                    let result = h.sessions.delete_session(&id).await;
                    Effect::Event(SessionEvent::Deleted(id, result.map_err(|e| e.to_string())))
                }))
            }
            Command::Model(id, variant, effort) => {
                let config = self.config.clone();
                self.operation = Some(tokio::spawn(async move {
                    Effect::Event(SessionEvent::Model(
                        switch_model(&config, &h, &id, variant, effort).await,
                    ))
                }));
            }
            Command::Switch(id) => {
                let config = self.config.clone();
                let mut template = self.template.clone();
                template.cwd = h.host.environment().cwd;
                self.operation = Some(tokio::spawn(async move {
                    Effect::Opened(open_session(&config, &template, id).await)
                }));
            }
        }
    }
    async fn run(
        &mut self,
        mut commands: mpsc::UnboundedReceiver<Command>,
        stop: &CancellationToken,
    ) {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = stop.cancelled() => break,
                command = commands.recv() => match command {
                    Some(command) => self.command(command),
                    None => break,
                },
                Some(event) = self.rx.recv() => {
                    let _ = self.events.send(SessionEvent::Output(event));
                    for _ in 0..255 {
                        match self.rx.try_recv() {
                            Ok(event) => { let _ = self.events.send(SessionEvent::Output(event)); }
                            Err(_) => break,
                        }
                    }
                },
                result = completed(&mut self.driver), if self.rx.is_empty() => {
                    self.driver = None;
                    match result {
                        Ok(Err(YourAiError::Aborted(reason))) => self.notice(
                            Level::Info,
                            format!("Stopped: {reason}. Queued inputs remain; send a message to resume."),
                        ),
                        Ok(Err(e)) => self.notice(
                            Level::Error,
                            format!(
                                "Execution failed: {}. Send a message to resume.",
                                summarize_error(&e.to_string())
                            ),
                        ),
                        Err(e) => self.notice(Level::Error, format!("Driver failed: {e}")),
                        _ => {}
                    }
                    let _ = self.events.send(SessionEvent::Settled);
                },
                result = completed(&mut self.compact) => {
                    self.compact = None;
                    match result {
                        Ok(Some(usage)) => {
                            let _ = self.events.send(SessionEvent::Usage(usage));
                        }
                        Ok(None) => {}
                        Err(e) => self.notice(Level::Error, format!("Compact failed: {e}")),
                    }
                    self.shared.lock().unwrap().finish_compaction();
                    self.snapshot.revision += 1;
                },
                result = completed(&mut self.retired) => {
                    self.retired = None;
                    match result {
                        Ok(Ok(pending)) if !pending.is_empty() => self.notice(
                            Level::Warning,
                            format!(
                                "{} queued inputs saved in the previous session.",
                                pending.len()
                            ),
                        ),
                        Ok(Err(e)) => self.notice(
                            Level::Error,
                            format!("Could not close previous session: {e}"),
                        ),
                        Err(e) => self.notice(
                            Level::Error,
                            format!("Previous session close task failed: {e}"),
                        ),
                        _ => {}
                    }
                    self.shared.lock().unwrap().finish_retirement();
                    self.resume();
                    self.notice(
                        Level::Info,
                        "Session ready. Previous sessions remain in /sessions.",
                    );
                },
                result = completed(&mut self.operation) => {
                    self.operation = None;
                    match result {
                        Ok(Effect::Event(event)) => {
                            let _ = self.events.send(event);
                        }
                        Ok(Effect::Opened(Err(e))) => {
                            let _ = self.events.send(SessionEvent::Opened(Err(e)));
                        }
                        Ok(Effect::Opened(Ok((h, history)))) => {
                            self.cancel.cancel();
                            self.harness().host.interrupt();
                            let old = self.harness();
                            let driver = self.driver.take();
                            let stats = self.stats.take();
                            let compact = self.compact.take();
                            self.retired = Some(tokio::spawn(async move {
                                if let Some(t) = driver {
                                    let _ = t.await;
                                }
                                if let Some(t) = stats {
                                    let _ = t.await;
                                }
                                if let Some(t) = compact {
                                    let _ = t.await;
                                }
                                old.close().await
                            }));
                            let (output, rx) = mpsc::unbounded_channel();
                            self.rx = rx;
                            self.shared.lock().unwrap().retire(Arc::new(h), output);
                            self.cancel = CancellationToken::new();
                            self.snapshot = SessionSnapshot {
                                revision: self.snapshot.revision + 1,
                                environment: self.harness().host.environment(),
                                pending_inputs: vec![],
                                active: false,
                                compacting: false,
                                can_steer: false,
                                model_metrics: self.harness().model_snapshot(),
                                context_usage: None,
                                response_count: Some(history.response_count),
                                title: history.title.clone(),
                                refresh_error: None,
                            };
                            self.stats_at = Instant::now() - Duration::from_secs(2);
                            let _ = self.events.send(SessionEvent::Opened(Ok(history)));
                        }
                        Err(e) => self.notice(Level::Error, format!("Session operation failed: {e}")),
                    }
                    self.shared.lock().unwrap().finish_operation();
                },
                result = completed(&mut self.stats) => {
                    self.stats = None;
                    match result {
                        Ok((context, count, title)) => {
                            self.snapshot.context_usage = context;
                            self.snapshot.response_count = count;
                            match title {
                                Ok(title) => {
                                    self.snapshot.title = title;
                                    self.snapshot.refresh_error = None;
                                }
                                Err(e) => {
                                    self.snapshot.refresh_error = Some(format!("Session title unavailable: {e}"))
                                }
                            }
                        }
                        Err(e) => self.snapshot.refresh_error = Some(format!("Session refresh failed: {e}")),
                    }
                    self.snapshot.revision += 1;
                },
                _ = self.wake.notified() => {},
                _ = tick.tick() => {},
            }
            if self.driver.is_none()
                && self.compact.is_none()
                && self.shared.lock().unwrap().take_resume()
            {
                self.resume();
            }
            if self.stats.is_none() && self.stats_at.elapsed() >= Duration::from_secs(1) {
                let h = self.harness();
                let host = h.host.clone();
                let usage = h.usage.clone();
                let id = host.environment().id;
                self.stats = Some(tokio::spawn(async move {
                    let context = tokio::task::spawn_blocking(move || host.context_usage().ok())
                        .await
                        .ok()
                        .flatten();
                    let count = usage.session_usage(&id).await.ok().map(|u| u.request_count);
                    let title = h.host.title().await;
                    (context, count, title)
                }));
                self.stats_at = Instant::now();
            }
            self.publish_snapshot();
        }
    }
    fn publish_snapshot(&mut self) {
        let shared = self.shared.lock().unwrap();
        let h = shared.harness();
        let status = h.host.status();
        self.snapshot.environment = h.host.environment();
        self.snapshot.pending_inputs = h.host.pending_inputs();
        self.snapshot.compacting = shared.compacting();
        self.snapshot.active =
            shared.submitting() || !matches!(status, SessionStatus::Idle | SessionStatus::Closed);
        self.snapshot.can_steer = matches!(status, SessionStatus::Running { .. });
        self.snapshot.model_metrics = h.model_snapshot();
        self.snapshots.send_replace(self.snapshot.clone());
    }
    async fn close(mut self) -> Result<(String, Vec<In>), Error> {
        self.cancel.cancel();
        self.harness().host.interrupt();
        if let Some(task) = self.operation.take() {
            if let Ok(Effect::Opened(Ok((h, _)))) = task.await {
                h.close().await?;
            }
        }
        if let Some(task) = self.retired.take() {
            let _ = task.await;
        }
        if let Some(task) = self.stats.take() {
            let _ = task.await;
        }
        if let Some(task) = self.compact.take() {
            let _ = task.await;
        }
        if let Some(task) = self.driver.take() {
            let _ = task.await;
        }
        let id = self.harness().host.environment().id.to_string();
        Ok((id, self.harness().close().await?))
    }
}

async fn completed<T>(task: &mut Option<JoinHandle<T>>) -> Result<T, tokio::task::JoinError> {
    match task {
        Some(task) => task.await,
        None => std::future::pending().await,
    }
}

pub async fn history(h: &Harness) -> Result<SessionHistory, YourAiError> {
    let id = h.host.environment().id;
    let mut after = 0;
    let mut messages = vec![];
    loop {
        let page = h
            .sessions
            .read_messages(
                &id,
                MessageQuery {
                    after,
                    active_only: false,
                    limit: 256,
                },
            )
            .await?;
        messages.extend(page.messages);
        match page.next {
            Some(next) if next > after => after = next,
            _ => break,
        }
    }
    let usage = h.usage.session_usage(&id).await?;
    Ok(SessionHistory {
        messages,
        usage: Usage {
            input_tokens: usage.total_input_tokens,
            output_tokens: usage.total_output_tokens,
            total_tokens: usage.total_tokens,
        },
        response_count: usage.request_count,
        title: h.host.title().await?,
    })
}
fn bind_config(h: &Harness, config: &Arc<Config>) -> Result<(), YourAiError> {
    let config = config.clone();
    h.host.bind_config_source(Arc::new(move || {
        config.execution_options().resolve(&TurnLimits::default())
    }))
}
async fn switch_model(
    config: &Arc<Config>,
    h: &Harness,
    id: &str,
    variant: Option<String>,
    effort: Option<EffortChoice>,
) -> Result<ModelSelection, String> {
    let values = config.values();
    let dependencies = crate::model::configured::dependencies(&values);
    let candidate =
        crate::model::configured::select_data(&values, id, variant.clone(), effort.as_ref())
            .map_err(|e| e.to_string())?;
    let crate::model::ResolvedModel {
        model,
        provider,
        requests,
    } = crate::model::configured::resolve_data(&candidate).map_err(|e| e.to_string())?;
    let policy =
        crate::config::read::<yourai_core::execution::ExecutionOptions>(&values, "execution")
            .context_policy()
            .clone();
    h.host
        .check_config_change(
            "model",
            serde_json::json!({"model": id, "variant": variant}),
        )
        .await
        .map_err(|e| e.to_string())?;
    let selection = ModelSelection::from_data(&candidate);
    h.apply_model_with_commit(model, policy, provider, requests, || {
        let mut data = config.write();
        if crate::model::configured::dependencies(&data) != dependencies {
            return Err(crate::error(
                "config",
                "Model settings changed while preparing the switch; retry",
            ));
        }
        let previous = data.clone();
        crate::model::configured::apply_selection(&mut data, &candidate);
        if let Err(error) = config.save_data(&data) {
            crate::model::configured::apply_selection(&mut data, &previous);
            return Err(crate::error("config", error));
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(selection)
}
async fn open_session(
    config: &Arc<Config>,
    template: &SessionOptions,
    id: Option<SessionId>,
) -> Result<(Harness, SessionHistory), String> {
    let (options, model) = crate::assembly::resolve_session(config, template, id)
        .map_err(|e| format!("Could not open session: {e}"))?;
    let h = Harness::open(options, model)
        .await
        .map_err(|e| format!("Could not open session: {e}"))?;
    let ready = async {
        bind_config(&h, config).map_err(|e| e.to_string())?;
        history(&h).await.map_err(|e| e.to_string())
    }
    .await;
    match ready {
        Ok(history) => Ok((h, history)),
        Err(e) => {
            let _ = h.close().await;
            Err(format!("Could not restore session: {e}"))
        }
    }
}

fn summarize_error(text: &str) -> String {
    let Some((head, body)) = text.split_once("Response body:") else {
        return text.into();
    };
    let head = head.trim_end().trim_end_matches('.');
    let body = body.trim();
    let parsed = serde_json::from_str::<serde_json::Value>(body).ok();
    let message = parsed.as_ref().and_then(|v| {
        v.get("message")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                v.pointer("/error/message")
                    .and_then(serde_json::Value::as_str)
                    .filter(|s| !s.trim().is_empty())
            })
    });
    let detail = message
        .map(str::to_owned)
        .unwrap_or_else(|| body.split_whitespace().collect::<Vec<_>>().join(" "));
    if detail.is_empty() {
        head.into()
    } else {
        format!("{head}: {}", detail.chars().take(512).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::summarize_error;
    #[test]
    fn error_summary_preserves_nested_flat_and_unstructured_diagnoses() {
        for body in [
            r#"{"error":{"message":"maximum context length exceeded"}}"#,
            r#"{"message":"maximum context length exceeded"}"#,
            r#"{"message":null,"error":{"message":"maximum context length exceeded"}}"#,
            "maximum context length exceeded",
        ] {
            for separator in [" ", "\n"] {
                let raw = format!("HTTP 400. Response body:{separator}{body}");
                let summary = summarize_error(&raw);
                assert!(
                    summary.contains("maximum context length exceeded"),
                    "{summary}"
                );
                assert!(!summary.contains("Response body:"));
            }
        }
        assert!(
            summarize_error(r#"HTTP 400. Response body: {"detail":"invalid parameter"}"#)
                .contains("invalid parameter")
        );
        assert_eq!(summarize_error("plain failure"), "plain failure");
    }
}

#[cfg(test)]
mod switch_tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;

    #[tokio::test]
    async fn failed_switch_is_atomic_and_session_rebuild_keeps_variant() {
        let dir = tempfile::tempdir().unwrap();
        let cfg: Config = serde_json::from_value(serde_json::json!({
          "models": {
            "model": "mock/large",
            "providers": {
              "mock": {
                "npm": "@ai-sdk/openai-compatible",
                "options": {
                  "baseURL": "http://127.0.0.1:1/v1",
                  "apiKey": "test"
                },
                "models": {
                  "large": {
                    "limit": {
                      "context": 64000,
                      "output": 4096
                    }
                  },
                  "small": {
                    "limit": {
                      "context": 32000,
                      "output": 4096
                    },
                    "variants": {
                      "short": {
                        "maxOutputTokens": 2048
                      },
                      "invalid": {
                        "temperature": -1
                      }
                    }
                  }
                }
              }
            }
          }
        }))
        .unwrap();
        let path = dir.path().join("yourai.json");
        std::fs::write(&path, serde_json::to_vec(&cfg).unwrap()).unwrap();
        let cfg = Config::init(&path).unwrap();
        let crate::model::ResolvedModel { model, .. } = crate::model::resolve(&cfg).unwrap();
        let hc = SessionOptions::new(dir.path().join("sessions"), dir.path().into());
        hc.config.set_system_prompt(Some("test".into()));
        hc.config
            .set_context_policy(cfg.execution_options().context_policy().clone());
        let h = Harness::open(hc.clone(), model).await.unwrap();
        let config = Arc::new(cfg);
        for (id, variant) in [
            ("missing/model", None),
            ("mock/small", Some("invalid")),
            ("mock/small", Some("missing")),
        ] {
            assert!(crate::controller::switch_model(
                &config,
                &h,
                id,
                variant.map(str::to_owned),
                None
            )
            .await
            .is_err());
            let cfg = &config;
            assert_eq!(cfg.model(), "mock/large");
            assert_eq!(
                crate::model::resolve(cfg)
                    .unwrap()
                    .model
                    .token_budget()
                    .limits()
                    .context(),
                Some(64000)
            );
            assert!(cfg.variant().is_none());
            assert_eq!(h.host.context_usage().unwrap().context_window, Some(64000));
        }
        let selected =
            crate::controller::switch_model(&config, &h, "mock/small", Some("short".into()), None)
                .await
                .unwrap();
        assert_eq!(h.host.context_usage().unwrap().output_reserve, 2048);
        let (_resume, resume_model) =
            crate::assembly::resolve_session(&config, &hc, Some(h.host.environment().id)).unwrap();
        assert_eq!(resume_model.token_budget().limits().context(), Some(32000));
        // This differs from the default model's 4096, proving variant resolution.
        assert_eq!(resume_model.token_budget().max_output_tokens(), 2048);
        assert_eq!(config.variant().as_deref(), Some("short"));
        assert_eq!(selected.label, "mock/small · short");
        h.close().await.unwrap();
    }

    #[tokio::test]
    async fn effort_override_sets_clears_and_reports_effective() {
        let dir = tempfile::tempdir().unwrap();
        let cfg: Config = serde_json::from_value(serde_json::json!({
          "models": {
            "model": "mock/large",
            "providers": {
              "mock": {
                "npm": "@ai-sdk/openai-compatible",
                "options": {
                  "baseURL": "http://127.0.0.1:1/v1",
                  "apiKey": "test"
                },
                "models": {
                  "large": {
                    "limit": {
                      "context": 64000,
                      "output": 4096
                    },
                    "options": {
                      "reasoningEffort": "low"
                    }
                  },
                  "small": {
                    "limit": {
                      "context": 32000,
                      "output": 4096
                    },
                    "variants": {
                      "short": {
                        "maxOutputTokens": 2048
                      }
                    }
                  }
                }
              }
            }
          }
        }))
        .unwrap();
        let path = dir.path().join("yourai.json");
        std::fs::write(&path, serde_json::to_vec(&cfg).unwrap()).unwrap();
        let cfg = Config::init(&path).unwrap();
        let crate::model::ResolvedModel { model, .. } = crate::model::resolve(&cfg).unwrap();
        let hc = SessionOptions::new(dir.path().join("sessions"), dir.path().into());
        hc.config.set_system_prompt(Some("test".into()));
        hc.config
            .set_context_policy(cfg.execution_options().context_policy().clone());
        let h = Harness::open(hc, model).await.unwrap();
        let config = Arc::new(cfg);

        // Set: writes the variant's options and reports the effective effort.
        let selected = crate::controller::switch_model(
            &config,
            &h,
            "mock/small",
            Some("short".into()),
            Some(crate::model::EffortChoice::Set("high".into())),
        )
        .await
        .unwrap();
        assert_eq!(selected.effort.as_deref(), Some("high"));
        assert_eq!(
            crate::model::effective_effort(&config, "mock/small", Some("short")).as_deref(),
            Some("high")
        );
        // The file-configured model-level effort still applies to its entry.
        assert_eq!(
            crate::model::effective_effort(&config, "mock/large", None).as_deref(),
            Some("low")
        );

        // Config: clears the override on that entry (falls back to none here).
        let selected = crate::controller::switch_model(
            &config,
            &h,
            "mock/small",
            Some("short".into()),
            Some(crate::model::EffortChoice::Config),
        )
        .await
        .unwrap();
        assert_eq!(selected.effort, None);
        assert!(!config
            .variant_options("mock/small", "short")
            .unwrap()
            .contains_key("reasoningEffort"));

        // Invalid keywords never reach the resolve or the shared config.
        assert!(crate::controller::switch_model(
            &config,
            &h,
            "mock/small",
            None,
            Some(crate::model::EffortChoice::Set("nope".into())),
        )
        .await
        .is_err());
        assert!(config
            .model_options("mock/small")
            .unwrap()
            .get("reasoningEffort")
            .is_none());
        h.close().await.unwrap();
    }
}
