//! Optional modules own their effects and invoke hooks at actual lifecycle boundaries.
use crate::{
    error,
    storage::{atomic_write, read_json},
    SessionHost,
};

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeConfig {
    pub system_prompt: Option<String>,
    pub skill_ids: Vec<String>,
    pub memory_search_limit: usize,
}
impl RuntimeConfig {
    pub(crate) fn apply(self, host: &SessionHost) {
        let config = yourai_loop::LoopConfig {
            system_prompt: self.system_prompt,
            skill_ids: self.skill_ids,
            memory_search_limit: self.memory_search_limit,
            ..Default::default()
        };
        host.agent
            .ctx()
            .set_agent_loop(Arc::new(yourai_loop::DefaultLoop::new(config)));
    }
}
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use yourai_core::{prelude::*, runtime_event::RuntimeEvent};

type WatchSnapshots = HashMap<PathBuf, HashMap<PathBuf, Vec<u8>>>;

pub struct Workspace {
    host: Weak<SessionHost>,
    worktrees: Mutex<HashMap<String, PathBuf>>,
    watcher: Mutex<Option<tokio::task::JoinHandle<()>>>,
    stop: CancellationToken,
    snapshots: Mutex<WatchSnapshots>,
}
impl Workspace {
    pub fn new(host: &Arc<SessionHost>) -> Result<Arc<Self>, YourAiError> {
        let p = host.dir.join("worktrees.json");
        let worktrees = if p.exists() {
            read_json(&p)?
        } else {
            HashMap::new()
        };
        Ok(Arc::new(Self {
            host: Arc::downgrade(host),
            worktrees: Mutex::new(worktrees),
            watcher: Mutex::new(None),
            stop: CancellationToken::new(),
            snapshots: Mutex::new(HashMap::new()),
        }))
    }
    fn host(&self) -> Result<Arc<SessionHost>, YourAiError> {
        self.host
            .upgrade()
            .ok_or_else(|| error("workspace", "session gone"))
    }
    pub async fn setup(&self, trigger: &str) -> Result<(), YourAiError> {
        let host = self.host()?;
        let _gate = host.try_operation()?;
        let result = host
            .dispatch(HookEvent::Setup {
                trigger: trigger.into(),
            })
            .await?;
        host.consume_hook(&result, true)
    }
    pub async fn load_instructions(&self, path: &Path, reason: &str) -> Result<(), YourAiError> {
        let host = self.host()?;
        let _gate = host.try_operation()?;
        self.load_inner(&host, path, reason).await
    }
    async fn load_inner(
        &self,
        host: &Arc<SessionHost>,
        path: &Path,
        reason: &str,
    ) -> Result<(), YourAiError> {
        let path = resolve(&host.context().cwd, path)?;
        let content = std::fs::read_to_string(&path).map_err(|e| error("instructions", e))?;
        let result = host
            .dispatch(HookEvent::InstructionsLoaded {
                file_path: path.to_string_lossy().into_owned(),
                memory_type: "project".into(),
                load_reason: reason.into(),
                globs: None,
                trigger_file_path: None,
                parent_file_path: None,
            })
            .await?;
        host.consume_hook(&result, false)?;
        host.context.lock().unwrap().instructions.insert(
            path.clone(),
            format!("[Instructions: {}]\n{content}", path.display()),
        );
        host.watch_path(path)?;
        Ok(())
    }
    pub async fn notify(&self, message: &str, kind: &str) -> Result<(), YourAiError> {
        let host = self.host()?;
        let result = host
            .dispatch(HookEvent::Notification {
                message: message.into(),
                title: None,
                notification_type: kind.into(),
            })
            .await?;
        host.consume_hook(&result, false)?;
        host.post_event(RuntimeEvent {
            id: uuid::Uuid::new_v4().to_string(),
            context: None,
            notice: Some(message.into()),
            wake: false,
        })?;
        Ok(())
    }
    pub async fn change_config(&self, source: &str, value: Value) -> Result<(), YourAiError> {
        let host = self.host()?;
        let _gate = host.try_operation()?;
        let config: RuntimeConfig =
            serde_json::from_value(value.clone()).map_err(|e| error("config", e))?;
        let path = host.dir.join("config.json");
        let candidate = host.dir.join("config.candidate.json");
        atomic_write(&candidate, &value)?;
        let result = host
            .dispatch(HookEvent::ConfigChange {
                source: source.into(),
                file_path: Some(candidate.to_string_lossy().into_owned()),
            })
            .await;
        let _ = std::fs::remove_file(candidate);
        host.consume_hook(&result?, true)?;
        atomic_write(&path, &value)?;
        config.apply(&host);
        Ok(())
    }
    pub fn config(&self) -> Result<Value, YourAiError> {
        let p = self.host()?.dir.join("config.json");
        if p.exists() {
            read_json(&p)
        } else {
            Ok(json!({}))
        }
    }
    pub async fn change_cwd(&self, path: &Path) -> Result<(), YourAiError> {
        let host = self.host()?;
        let _gate = host.try_operation()?;
        let old = host.context().cwd;
        let new = resolve(&old, path)?;
        if !new.is_dir() {
            return Err(error("workspace", "cwd is not a directory"));
        }
        let result = host
            .dispatch(HookEvent::CwdChanged {
                old_cwd: old.to_string_lossy().into_owned(),
                new_cwd: new.to_string_lossy().into_owned(),
            })
            .await?;
        host.consume_hook(&result, true)?;
        if let HookPointOutcome::CwdChanged(o) = result.outcome {
            for p in o.watch_paths {
                host.watch_path(PathBuf::from(p))?;
            }
        }
        host.set_cwd(new)
    }
    pub async fn create_worktree(&self, name: &str) -> Result<PathBuf, YourAiError> {
        let host = self.host()?;
        let _gate = host.try_operation()?;
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(error("workspace", "invalid worktree name"));
        }
        if self.worktrees.lock().unwrap().contains_key(name) {
            return Err(error("workspace", "worktree already tracked"));
        }
        let result = host
            .dispatch(HookEvent::WorktreeCreate { name: name.into() })
            .await?;
        host.consume_hook(&result, true)?;
        let custom = match result.outcome {
            HookPointOutcome::WorktreeCreate(o) => o.worktree_path,
            _ => None,
        };
        let path = if let Some(path) = custom {
            let p = PathBuf::from(path)
                .canonicalize()
                .map_err(|e| error("workspace", e))?;
            if !p.is_dir() {
                return Err(error("workspace", "hook worktree path is not a directory"));
            }
            p
        } else {
            let p = host.dir.join("worktrees").join(name);
            std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| error("workspace", e))?;
            git(
                &host.context().cwd,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    p.to_str()
                        .ok_or_else(|| error("workspace", "non UTF-8 worktree path"))?,
                    "HEAD",
                ],
            )
            .await?;
            p.canonicalize().map_err(|e| error("workspace", e))?
        };
        let mut map = self.worktrees.lock().unwrap();
        map.insert(name.into(), path.clone());
        atomic_write(&host.dir.join("worktrees.json"), &*map)?;
        Ok(path)
    }
    pub async fn remove_worktree(&self, name: &str) -> Result<(), YourAiError> {
        let host = self.host()?;
        let _gate = host.try_operation()?;
        let path = self
            .worktrees
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| error("workspace", "unknown worktree"))?;
        let result = host
            .dispatch(HookEvent::WorktreeRemove {
                worktree_path: path.to_string_lossy().into_owned(),
            })
            .await?;
        host.consume_hook(&result, true)?;
        git(
            &host.context().cwd,
            &[
                "worktree",
                "remove",
                path.to_str()
                    .ok_or_else(|| error("workspace", "non UTF-8 path"))?,
            ],
        )
        .await?;
        let mut map = self.worktrees.lock().unwrap();
        map.remove(name);
        atomic_write(&host.dir.join("worktrees.json"), &*map)
    }
    /// Polling watcher avoids OS-specific dependencies; only explicitly registered paths.
    pub fn start_watching(self: &Arc<Self>, interval: Duration) -> Result<(), YourAiError> {
        if interval.is_zero() {
            return Err(error("watcher", "zero interval"));
        }
        let mut task = self.watcher.lock().unwrap();
        for root in self.host()?.watch_paths() {
            let mut snapshots = self.snapshots.lock().unwrap();
            if let std::collections::hash_map::Entry::Vacant(entry) = snapshots.entry(root.clone())
            {
                entry.insert(file_snapshot(&root)?);
            }
        }
        if task.is_some() {
            return Ok(());
        }
        let weak = Arc::downgrade(self);
        let stop = self.stop.clone();
        *task = Some(tokio::spawn(async move {
            loop {
                tokio::select! {_=stop.cancelled()=>break,_=tokio::time::sleep(interval)=>{}}
                let Some(ws) = weak.upgrade() else { break };
                let Ok(host) = ws.host() else { break };
                if matches!(
                    host.status(),
                    SessionStatus::Closed | SessionStatus::Closing
                ) {
                    break;
                }
                for root in host.watch_paths() {
                    let current = match file_snapshot(&root) {
                        Ok(snapshot) => snapshot,
                        Err(e) => {
                            let _ = host.post_event(RuntimeEvent {
                                id: uuid::Uuid::new_v4().to_string(),
                                context: None,
                                notice: Some(format!("File watcher: {e}")),
                                wake: false,
                            });
                            continue;
                        }
                    };
                    let old = ws.snapshots.lock().unwrap().insert(root, current.clone());
                    if let Some(old) = old {
                        let paths: std::collections::HashSet<_> =
                            old.keys().chain(current.keys()).cloned().collect();
                        for path in paths {
                            if old.get(&path) == current.get(&path) {
                                continue;
                            }
                            let event = if !current.contains_key(&path) {
                                "deleted"
                            } else if !old.contains_key(&path) {
                                "created"
                            } else {
                                "modified"
                            };
                            let result = host
                                .dispatch(HookEvent::FileChanged {
                                    file_path: path.to_string_lossy().into_owned(),
                                    event: event.into(),
                                })
                                .await;
                            if let Ok(r) = result {
                                let _ = host.consume_hook(&r, false);
                                if let HookPointOutcome::FileChanged(o) = r.outcome {
                                    for p in o.watch_paths {
                                        let _ = host.watch_path(PathBuf::from(p));
                                    }
                                }
                            }
                            let _ = host.post_event(RuntimeEvent {
                                id: uuid::Uuid::new_v4().to_string(),
                                context: Some(format!("File {event}: {}", path.display())),
                                notice: None,
                                wake: false,
                            });
                        }
                    }
                }
            }
        }));
        Ok(())
    }
    pub async fn stop_watching(&self) {
        self.stop.cancel();
        let task = self.watcher.lock().unwrap().take();
        if let Some(t) = task {
            t.abort();
            let _ = t.await;
        }
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(t) = self.watcher.get_mut().unwrap().take() {
            t.abort();
        }
    }
}
// Do not follow directory symlinks: a watched tree must not recursively escape its root.
fn file_snapshot(root: &Path) -> Result<HashMap<PathBuf, Vec<u8>>, YourAiError> {
    let mut files = HashMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(error("watcher", e)),
        };
        if meta.is_dir() {
            for entry in std::fs::read_dir(&path).map_err(|e| error("watcher", e))? {
                pending.push(entry.map_err(|e| error("watcher", e))?.path());
            }
        } else if meta.is_file() {
            files.insert(
                path.clone(),
                std::fs::read(path).map_err(|e| error("watcher", e))?,
            );
        }
    }
    Ok(files)
}
fn resolve(cwd: &Path, path: &Path) -> Result<PathBuf, YourAiError> {
    let p = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    p.canonicalize().map_err(|e| error("workspace", e))
}
async fn git(cwd: &Path, args: &[&str]) -> Result<(), YourAiError> {
    let mut c = tokio::process::Command::new("git");
    c.current_dir(cwd).args(args).kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(30), c.output())
        .await
        .map_err(|_| error("git", "timeout"))?
        .map_err(|e| error("git", e))?;
    if !out.status.success() {
        return Err(error("git", String::from_utf8_lossy(&out.stderr)));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub subject: String,
    pub description: Option<String>,
    pub owner: Option<String>,
    pub completed: bool,
}
pub struct TaskBoard {
    host: Weak<SessionHost>,
    tasks: Mutex<HashMap<String, Task>>,
    gate: tokio::sync::Mutex<()>,
    pub team: String,
}
impl TaskBoard {
    pub fn new(host: &Arc<SessionHost>, team: impl Into<String>) -> Result<Arc<Self>, YourAiError> {
        let path = host.dir.join("tasks.json");
        let tasks = if path.exists() {
            read_json(&path)?
        } else {
            HashMap::new()
        };
        Ok(Arc::new(Self {
            host: Arc::downgrade(host),
            tasks: Mutex::new(tasks),
            gate: tokio::sync::Mutex::new(()),
            team: team.into(),
        }))
    }
    pub fn list(&self) -> Vec<Task> {
        let mut v: Vec<_> = self.tasks.lock().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }
    pub async fn create(
        &self,
        subject: String,
        description: Option<String>,
        owner: Option<String>,
    ) -> Result<Task, YourAiError> {
        let _gate = self.gate.lock().await;
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| error("tasks", "host gone"))?;
        let task = Task {
            id: uuid::Uuid::new_v4().to_string(),
            subject,
            description,
            owner,
            completed: false,
        };
        let result = host
            .dispatch(HookEvent::TaskCreated {
                task_id: task.id.clone(),
                task_subject: task.subject.clone(),
                task_description: task.description.clone(),
                teammate_name: task.owner.clone(),
                team_name: Some(self.team.clone()),
            })
            .await?;
        host.consume_hook(&result, true)?;
        let mut tasks = self.tasks.lock().unwrap();
        let mut next = tasks.clone();
        next.insert(task.id.clone(), task.clone());
        atomic_write(&host.dir.join("tasks.json"), &next)?;
        *tasks = next;
        Ok(task)
    }
    pub async fn complete(&self, id: &str) -> Result<(), YourAiError> {
        let _gate = self.gate.lock().await;
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| error("tasks", "host gone"))?;
        let task = self
            .tasks
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| error("tasks", "unknown task"))?;
        if task.completed {
            return Ok(());
        }
        let result = host
            .dispatch(HookEvent::TaskCompleted {
                task_id: task.id,
                task_subject: task.subject,
                task_description: task.description,
                teammate_name: task.owner,
                team_name: Some(self.team.clone()),
            })
            .await?;
        host.consume_hook(&result, true)?;
        let mut tasks = self.tasks.lock().unwrap();
        let mut next = tasks.clone();
        next.get_mut(id).unwrap().completed = true;
        atomic_write(&host.dir.join("tasks.json"), &next)?;
        *tasks = next;
        Ok(())
    }
    pub async fn idle(&self, teammate: &str) -> Result<(), YourAiError> {
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| error("tasks", "host gone"))?;
        if self
            .list()
            .iter()
            .any(|t| t.owner.as_deref() == Some(teammate) && !t.completed)
        {
            return Err(error("tasks", "teammate has unfinished tasks"));
        }
        let result = host
            .dispatch(HookEvent::TeammateIdle {
                teammate_name: teammate.into(),
                team_name: self.team.clone(),
            })
            .await?;
        host.consume_hook(&result, true)
    }
}
impl ToolHandler for TaskBoard {
    fn name(&self) -> &str {
        "tasks"
    }
    fn definition(&self) -> Tool {
        Tool::new("tasks").with_schema(json!({"type":"object","properties":{"action":{"enum":["list","create","complete","idle"]},"subject":{"type":"string"},"id":{"type":"string"},"owner":{"type":"string"}},"required":["action"]}))
    }
    fn security_context(&self, v: &Value) -> SecurityContext {
        SecurityContext {
            action: "tasks".into(),
            input: v.clone(),
            is_destructive: false,
            is_network: false,
        }
    }
    fn execute<'a>(
        &'a self,
        _: ToolContext<'a>,
        v: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            match v["action"].as_str() {
                Some("list") => Ok(json!(self.list())),
                Some("create") => Ok(json!(
                    self.create(
                        v["subject"]
                            .as_str()
                            .ok_or_else(|| error("tasks", "subject required"))?
                            .into(),
                        None,
                        v["owner"].as_str().map(str::to_owned)
                    )
                    .await?
                )),
                Some("complete") => {
                    self.complete(
                        v["id"]
                            .as_str()
                            .ok_or_else(|| error("tasks", "id required"))?,
                    )
                    .await?;
                    Ok(json!({"completed":true}))
                }
                Some("idle") => {
                    self.idle(
                        v["owner"]
                            .as_str()
                            .ok_or_else(|| error("tasks", "owner required"))?,
                    )
                    .await?;
                    Ok(json!({"idle":true}))
                }
                _ => Err(error("tasks", "unknown action")),
            }
        })
    }
}

/// Subagents are independent sessions. Parent owns cancellation and observes child output.
pub struct SubagentTool {
    host: Weak<SessionHost>,
    model: Arc<dyn ModelProvider>,
    tools: Option<Arc<dyn ToolRegistry>>,
    children: Mutex<HashMap<String, Arc<SessionHost>>>,
}
impl SubagentTool {
    pub fn new(
        host: &Arc<SessionHost>,
        model: Arc<dyn ModelProvider>,
        tools: Option<Arc<dyn ToolRegistry>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            host: Arc::downgrade(host),
            model,
            tools,
            children: Mutex::new(HashMap::new()),
        })
    }
    pub fn child_ids(&self) -> Vec<String> {
        self.children.lock().unwrap().keys().cloned().collect()
    }
    pub async fn stop_all(&self) -> Result<(), YourAiError> {
        let children: Vec<_> = self.children.lock().unwrap().values().cloned().collect();
        for child in children {
            child.close(Duration::from_secs(10)).await?;
        }
        Ok(())
    }
    async fn run_child(&self, tc: ToolContext<'_>, prompt: String) -> Result<Value, YourAiError> {
        let parent = self
            .host
            .upgrade()
            .ok_or_else(|| error("subagent", "parent session gone"))?;
        let root = parent
            .context()
            .transcript_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| parent.dir.join("children"));
        let catalog = crate::SessionCatalog::new(&root)?;
        let mut meta = catalog.create_session().await?;
        meta.parent_session_id = Some(parent.context().id);
        catalog.save_session(&meta).await?;
        let id = meta.id.0.clone();
        let start = parent
            .dispatch(HookEvent::SubagentStart {
                agent_id: id.clone(),
                agent_type: "worker".into(),
            })
            .await?;
        parent.consume_hook(&start, true)?;
        let child = SessionHost::restore(
            &root,
            meta.id,
            parent.context().cwd,
            self.model.clone(),
            parent.agent.ctx().try_hooks(),
            self.tools.clone(),
            parent.agent.ctx().context_manager()?.policy(),
            "startup",
        )
        .await?;
        parent.register_child(&child);
        self.children
            .lock()
            .unwrap()
            .insert(id.clone(), child.clone());
        let _cleanup = ChildCleanup(child.clone());
        child
            .submit(In::user_text(prompt))
            .map_err(|e| error("subagent", e))?;
        let mut last = String::new();
        for continuation in 0..=3 {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let work = child.run_next(TurnLimits::default(), &tx, tc.cancel);
            tokio::pin!(work);
            let report = loop {
                tokio::select! {biased;
                    result=&mut work=>break result?,
                    Some(event)=rx.recv()=>match event{
                        Out::Ask{id:request_id,payload}=>{let reply=tc.ask(InteractionKind::Question,json!({"child_id":id,"request":payload})).await?;child.submit(In::Reply{id:request_id,payload:reply}).map_err(|e|error("subagent",e))?;},
                        other=>{if !tc.emit_progress(json!({"child_id":id,"event":other})){child.interrupt();return Err(AbortReason::Disconnected.into());}}
                    }
                }
            };
            if let Some(report) = report {
                last = report.result.map_err(|e| *e.error)?.text;
            }
            let stop = parent
                .dispatch(HookEvent::SubagentStop {
                    stop_hook_active: continuation > 0,
                    agent_id: id.clone(),
                    agent_transcript_path: child
                        .context()
                        .transcript_path
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    agent_type: "worker".into(),
                    last_assistant_message: Some(last.clone()),
                })
                .await?;
            parent.consume_hook(&stop, false)?;
            if stop.common.blocking_errors.is_empty() {
                child.close(Duration::from_secs(10)).await?;
                return Ok(json!({"agent_id":id,"text":last}));
            }
            if continuation == 3 {
                child.close(Duration::from_secs(10)).await?;
                return Err(error("subagent", "SubagentStop continuation limit"));
            }
            child
                .submit(In::user_text(
                    stop.common
                        .blocking_errors
                        .iter()
                        .map(|e| e.message.clone())
                        .collect::<Vec<_>>()
                        .join("\n"),
                ))
                .map_err(|e| error("subagent", e))?;
        }
        Err(error("subagent", "unreachable continuation state"))
    }
}
struct ChildCleanup(Arc<SessionHost>);
impl Drop for ChildCleanup {
    fn drop(&mut self) {
        if self.0.status() == SessionStatus::Closed {
            return;
        }
        self.0.interrupt();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let child = self.0.clone();
            runtime.spawn(async move {
                let _ = child.close(Duration::from_secs(10)).await;
            });
        }
    }
}
impl ToolHandler for SubagentTool {
    fn name(&self) -> &str {
        "subagent"
    }
    fn definition(&self) -> Tool {
        Tool::new("subagent").with_schema(json!({"type":"object","properties":{"prompt":{"type":"string"}},"required":["prompt"],"additionalProperties":false}))
    }
    fn security_context(&self, input: &Value) -> SecurityContext {
        SecurityContext {
            action: "subagent".into(),
            input: input.clone(),
            is_destructive: false,
            is_network: true,
        }
    }
    fn execute<'a>(
        &'a self,
        tc: ToolContext<'a>,
        input: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            let prompt = input["prompt"]
                .as_str()
                .ok_or_else(|| error("subagent", "prompt required"))?;
            self.run_child(tc, prompt.into()).await
        })
    }
}
