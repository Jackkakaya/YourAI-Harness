//! Private business actions; no hook types or dispatch.
use super::*;

impl Workspace {
    pub(super) async fn run_load_instructions(
        &self,
        host: &Arc<SessionHost>,
        path: PathBuf,
        content: String,
    ) -> Result<(), YourAiError> {
        host.context.lock().unwrap().instructions.insert(
            path.clone(),
            format!("[Instructions: {}]\n{content}", path.display()),
        );
        host.watch_path_async(path).await?;
        Ok(())
    }
    pub(super) async fn run_notify(
        &self,
        host: &Arc<SessionHost>,
        message: &str,
    ) -> Result<(), YourAiError> {
        host.post_event_async(RuntimeEvent {
            id: uuid::Uuid::new_v4().to_string(),
            context: None,
            notice: Some(message.into()),
            wake: false,
        })
        .await?;
        Ok(())
    }
    pub(super) fn run_config(
        &self,
        host: &Arc<SessionHost>,
        config: RuntimeConfig,
        value: &Value,
    ) -> Result<(), YourAiError> {
        let path = host.dir.join("config.json");
        atomic_write(&path, value)?;
        config.apply(host);
        Ok(())
    }
    pub(super) async fn run_cwd(
        &self,
        host: &Arc<SessionHost>,
        path: PathBuf,
    ) -> Result<(), YourAiError> {
        host.set_cwd_async(path).await
    }
    pub(super) async fn run_create_worktree(
        &self,
        host: &Arc<SessionHost>,
        name: &str,
        custom: Option<String>,
    ) -> Result<PathBuf, YourAiError> {
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
    pub(super) async fn run_remove_worktree(
        &self,
        host: &Arc<SessionHost>,
        name: &str,
        path: &Path,
    ) -> Result<(), YourAiError> {
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
}
