//! One assembly path used equally by terminal and web drivers.
use crate::hooks::DefaultHookManager;
use crate::{
    collaboration::*, memory::LocalMemory, skills::LocalSkills, storage::LocalUsage,
    workspace::Workspace, *,
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use yourai_core::prelude::*;

/// Startup selection and adapter assembly; persistent settings live in the single `config` value.
#[derive(Clone)]
pub struct SessionOptions {
    pub root: PathBuf,
    pub cwd: PathBuf,
    pub resume: Option<SessionId>,
    /// Optional replacement of the original AgentLoop scheduling interface.
    pub agent_loop: Option<Arc<dyn AgentLoop>>,
    pub config: Arc<crate::config::Config>,
    pub memory_manager: Option<Arc<dyn MemoryManager>>,
    pub skill_provider: Option<Arc<dyn SkillProvider>>,
    pub request_policy: (Option<u32>, u64),
    /// Stable provider key used to retain admission state across model switches.
    pub model_provider: String,
    /// Optional pinned summary model and its provider admission settings.
    /// Without this, compaction follows the currently selected main model.
    pub compaction_model: Option<crate::model::ResolvedModel>,
}
impl SessionOptions {
    pub fn new(root: PathBuf, cwd: PathBuf) -> Self {
        Self {
            root,
            cwd,
            resume: None,
            agent_loop: None,
            config: Arc::new(crate::config::Config::default()),
            memory_manager: None,
            skill_provider: None,
            request_policy: Default::default(),
            model_provider: String::new(),
            compaction_model: None,
        }
    }
}
type ProviderBudgets = HashMap<(String, (Option<u32>, u64)), Arc<ModelBudget>>;
pub struct Harness {
    normal_security: Arc<dyn SecurityProvider>,
    yolo_security: Arc<dyn SecurityProvider>,
    provider_budgets: Mutex<ProviderBudgets>,
    current_budget: Mutex<Arc<ModelBudget>>,
    /// Shared persistence interface for frontends reading clean session history.
    pub sessions: Arc<dyn SessionStore>,
    pub host: Arc<SessionManager>,
    pub tools: Arc<ToolSet>,
    pub hooks: Arc<DefaultHookManager>,
    pub budget: Arc<ModelBudget>,
    pub workspace: Option<Arc<Workspace>>,
    pub tasks: Option<Arc<TaskManager>>,
    pub subagents: Option<Arc<Subagent>>,
    pub memory: Option<Arc<LocalMemory>>,
    pub skills: Option<Arc<LocalSkills>>,
    pub usage: Arc<LocalUsage>,
}
impl Harness {
    pub async fn open(
        config: SessionOptions,
        model: Arc<dyn ModelProvider>,
    ) -> Result<Self, YourAiError> {
        let values = config.config.values();
        crate::config::read::<ExecutionOptions>(&values, "execution")
            .context_policy()
            .validate_for(model.token_budget())?;
        if let Some(summary) = &config.compaction_model {
            crate::config::read::<ExecutionOptions>(&values, "execution")
                .context_policy()
                .validate_for(summary.model.token_budget())?;
            crate::model::validate_requests(summary.requests)?;
        }
        let catalog = Arc::new(SessionCatalog::new(&config.root)?);
        let source = if config.resume.is_some() {
            "resume"
        } else {
            "startup"
        };
        let (meta, lease, prepared) =
            prepare_session(&catalog, &config, model.as_ref(), &values).await?;
        let id = meta.id;
        let session_config = config.clone();
        let budget = ModelBudget::configured(config.request_policy, (*catalog.store).clone())?;
        let mut provider_budgets = HashMap::from([(
            (config.model_provider.clone(), config.request_policy),
            budget.clone(),
        )]);
        let model: Arc<dyn ModelProvider> = Arc::new(MeteredModel {
            inner: model,
            budget: budget.clone(),
        });
        let usage = Arc::new(LocalUsage((*catalog.store).clone()));
        let hooks = Arc::new(DefaultHookManager::new().with_model(model.clone()));
        hooks
            .register_config(
                crate::config::read::<crate::config::HookSettings>(&values, "hooks").events(),
                HookSource::Project,
            )
            .await?;
        let (agent, tools) = assemble(
            &id,
            lease.store(),
            &config.cwd,
            crate::config::read::<bool>(&values, "trusted_shell"),
            Agent::builder()
                .model(model.clone())
                .hooks(hooks.clone())
                .usage(usage.clone()),
            &crate::config::read::<ExecutionOptions>(&values, "execution"),
        )
        .await?;
        if let Some(summary) = config.compaction_model {
            let settings = summary;
            let key = (settings.provider, settings.requests);
            let summary_budget = match provider_budgets.get(&key) {
                Some(existing) => existing.clone(),
                None => budget.for_provider(settings.requests)?,
            };
            provider_budgets.insert(key, summary_budget.clone());
            agent.set_compaction_model(Arc::new(MeteredModel {
                inner: settings.model,
                budget: summary_budget,
            }));
        }
        let normal_security = agent.security()?;
        let yolo_security: Arc<dyn SecurityProvider> = Arc::new(crate::security::YoloSecurity);
        if crate::config::read::<bool>(&values, "yolo") {
            agent.set_security(yolo_security.clone());
        }
        let memory = if crate::config::read::<bool>(&values, "extensions") {
            Some(Arc::new(LocalMemory::new(lease.store(), id.clone())))
        } else {
            None
        };
        let skills = if crate::config::read::<bool>(&values, "extensions") {
            Some(Arc::new(LocalSkills::new(lease.store(), id.clone())))
        } else {
            None
        };
        if let Some(provider) = config.memory_manager {
            agent.set_memory(provider);
        } else if let Some(memory) = &memory {
            agent.set_memory(memory.clone());
        }
        crate::memory::register(hooks.as_ref(), id.clone()).await?;

        if let Some(agent_loop) = config.agent_loop {
            agent.set_agent_loop(agent_loop);
        }
        if let Some(provider) = config.skill_provider {
            agent.set_skills(provider);
        } else if let Some(skills) = &skills {
            agent.set_skills(skills.clone());
        }
        let host =
            start_session(&session_config, id, lease, agent, prepared, source, &values).await?;
        let ready = async {
            let workspace = if crate::config::read::<bool>(&values, "extensions") {
                Some(host.workspace()?)
            } else {
                None
            };
            // Task progress is a basic coding capability, independent of workspace/subagents.
            let tasks = Some(host.task_manager("default")?);
            let subagents = if crate::config::read::<bool>(&values, "extensions") {
                Some(subagent(&host, None, None))
            } else {
                None
            };
            if let Some(tasks) = &tasks {
                tools.register(Arc::new(crate::collaboration::tools::TaskTool(
                    tasks.clone(),
                )));
            }
            if let Some(subagents) = &subagents {
                tools.register(Arc::new(crate::collaboration::tools::SubagentTool(
                    subagents.clone(),
                )));
            }
            Ok(Self {
                normal_security,
                yolo_security,
                provider_budgets: Mutex::new(provider_budgets),
                current_budget: Mutex::new(budget.clone()),
                sessions: catalog.store.clone(),
                host: host.clone(),
                tools,
                hooks,
                budget,
                workspace,
                tasks,
                subagents,
                memory,
                skills,
                usage,
            })
        }
        .await;
        if ready.is_err() {
            let _ = host.finish_close(None).await;
        }
        ready
    }
    pub fn yolo(&self) -> bool {
        self.host
            .agent()
            .try_security()
            .is_some_and(|current| Arc::ptr_eq(&current, &self.yolo_security))
    }
    /// Change the approval mode for this run only; never save a bypass as a session default.
    /// Change permissions only between turns, preserving the original policy.
    /// In-flight tool snapshots and approval questions must finish or be cancelled first.
    pub fn apply_yolo(&self, enabled: bool) -> Result<(), YourAiError> {
        self.apply_yolo_with_commit(enabled, || Ok(()))
    }
    pub(crate) fn apply_yolo_with_commit(
        &self,
        enabled: bool,
        commit: impl FnOnce() -> Result<(), YourAiError>,
    ) -> Result<(), YourAiError> {
        let _gate = self.host.try_operation()?;
        commit()?;
        self.host.agent().set_security(if enabled {
            self.yolo_security.clone()
        } else {
            self.normal_security.clone()
        });
        Ok(())
    }
    /// Replace the main model and its context policy at an idle boundary.
    /// Admission/metrics keep the existing shared budget (including calls already
    /// spent); active turns and compaction reject the switch without changes.
    /// Hooks and new children inherit this selection unless explicitly pinned.
    pub async fn switch_model(
        &self,
        model: Arc<dyn ModelProvider>,
        policy: ContextPolicy,
    ) -> Result<(), YourAiError> {
        self.switch_model_inner(model, policy, None, None).await
    }
    /// Shared usage counters with the current main provider's cooldown.
    pub fn model_snapshot(&self) -> crate::model::BudgetSnapshot {
        self.current_budget.lock().unwrap().snapshot()
    }
    /// Atomically publish main-model context, timeouts and provider admission at idle.
    pub async fn switch_model_with_settings(
        &self,
        model: Arc<dyn ModelProvider>,
        policy: ContextPolicy,
        provider: String,
        requests: (Option<u32>, u64),
    ) -> Result<(), YourAiError> {
        crate::model::validate_requests(requests)?;
        self.switch_model_inner(model, policy, Some((provider, requests)), None)
            .await
    }
    pub(crate) async fn apply_model_with_commit(
        &self,
        model: Arc<dyn ModelProvider>,
        policy: ContextPolicy,
        provider: String,
        requests: (Option<u32>, u64),
        commit: impl FnOnce() -> Result<(), YourAiError> + Send,
    ) -> Result<(), YourAiError> {
        crate::model::validate_requests(requests)?;
        self.switch_model_inner(
            model,
            policy,
            Some((provider, requests)),
            Some(Box::new(commit)),
        )
        .await
    }
    async fn switch_model_inner(
        &self,
        model: Arc<dyn ModelProvider>,
        policy: ContextPolicy,
        settings: Option<(String, (Option<u32>, u64))>,
        commit: Option<Box<dyn FnOnce() -> Result<(), YourAiError> + Send + '_>>,
    ) -> Result<(), YourAiError> {
        policy.validate_for(model.token_budget())?;
        let _gate = self.host.try_operation()?;
        let selected_budget = if let Some(settings) = &settings {
            if let Some(budget) = self
                .provider_budgets
                .lock()
                .unwrap()
                .get(&(settings.0.clone(), settings.1))
                .cloned()
            {
                budget
            } else {
                self.budget.for_provider(settings.1)?
            }
        } else {
            self.current_budget.lock().unwrap().clone()
        };
        let id = self.host.environment().id;
        let history = DefaultContext::new(
            id.clone(),
            String::new(),
            Some(self.host.store()),
            policy.clone(),
        );
        // Prepare everything that can fail before publishing either provider.
        history.restore().await?;
        let mut meta = self.sessions.load_session(&id).await?;
        meta.model = Some(model.model_iden().into());
        let update_metadata_after_publish = commit.is_some();
        if !update_metadata_after_publish {
            self.host.store().save_session(&meta).await?;
        }
        if let Some(commit) = commit {
            commit()?;
        }
        let model = Arc::new(MeteredModel {
            inner: model,
            budget: selected_budget.clone(),
        });
        if let Some(settings) = settings {
            self.provider_budgets
                .lock()
                .unwrap()
                .insert((settings.0, settings.1), selected_budget.clone());
        }
        *self.current_budget.lock().unwrap() = selected_budget;
        self.host.agent().update(|providers| {
            providers.context_manager = Some(history);
            providers.model = Some(model);
        });
        // Metadata is a display projection; file config is the authority.
        if update_metadata_after_publish {
            if let Err(error) = self.host.store().save_session(&meta).await {
                self.host.close_warning(&error);
            }
        }
        Ok(())
    }

    pub async fn close(&self) -> Result<Vec<In>, YourAiError> {
        self.host.finish_close(None).await?;
        if let Err(error) = self.budget.flush().await {
            // Diagnostic failure must not swallow the user's pending inputs.
            self.host.close_warning(&error);
        }
        Ok(self.host.take_closed_inputs())
    }
}

/// Assemble the same provider set for new, resumed and child sessions.
pub(crate) async fn assemble(
    id: &SessionId,
    store: Arc<dyn SessionStore>,
    cwd: &std::path::Path,
    trusted_shell: bool,
    mut bindings: AgentBuilder,
    config: &ExecutionOptions,
) -> Result<(Arc<Agent>, Arc<ToolSet>), YourAiError> {
    let output = crate::tools::ToolOutputStore::new(store.clone(), id.clone());
    let inherited = bindings
        .tools
        .as_ref()
        .map(|r| {
            r.snapshot()
                .into_iter()
                .filter(|tool| !crate::tools::BUILTIN_TOOL_NAMES.contains(&tool.name()))
                .collect()
        })
        .unwrap_or_default();
    let registry = Arc::new(ToolSet::new(inherited));
    registry.extend(crate::tools::coding_tools_with_output(
        cwd,
        Some(output.clone()),
    )?);
    bindings
        .agent_loop
        .get_or_insert_with(|| Arc::new(crate::default_loop::DefaultLoop));
    bindings.context_manager = Some(DefaultContext::new(
        id.clone(),
        String::new(),
        Some(store.clone()),
        config.context_policy().clone(),
    ));
    bindings.session = Some(store.clone());
    bindings.tools = Some(registry.clone());
    bindings.tool_output = Some(output);
    if bindings.security.is_none() {
        bindings.security = Some(PolicySecurity::open_for_workspace(
            store,
            id.clone(),
            vec![],
            cwd.to_owned(),
            trusted_shell,
        )?);
    }
    Ok((bindings.build(), registry))
}

/// Publish a session only after lifecycle initialization and notices succeed.
pub(crate) async fn start_session(
    config: &SessionOptions,
    id: SessionId,
    lease: crate::runtime::SessionLease,
    agent: Arc<Agent>,
    prepared: crate::context::prompt::PreparedPrompt,
    source: &str,
    values: &serde_json::Value,
) -> Result<Arc<SessionManager>, YourAiError> {
    let mut environment = SessionEnvironment::new(id, config.cwd.clone());
    environment.transcript_path = Some(SqliteStore::path(&config.root));
    environment.instructions = prepared.instructions;
    let mut settings = crate::config::read::<ExecutionOptions>(values, "execution");
    if source != "startup" {
        settings.set_instruction_paths(vec![]);
    }
    settings.set_workspace_enabled(
        settings.workspace_enabled() || crate::config::read::<bool>(values, "extensions"),
    );
    let host = SessionManager::open_owned(lease, environment, agent, settings, source).await?;
    let ready = async {
        for notice in prepared.notices {
            host.post_event_async(yourai_core::runtime_event::RuntimeEvent {
                id: uuid::Uuid::new_v4().to_string(),
                context: None,
                notice: Some(notice),
                wake: false,
            })
            .await?;
        }
        Ok::<_, YourAiError>(())
    }
    .await;
    if let Err(error) = ready {
        let _ = host.finish_close(None).await;
        return Err(error);
    }
    Ok(host)
}

/// One initialization path for Harness, standalone hosts and child sessions.
/// Resume acquires ownership before any prompt preparation or metadata write.
pub(crate) async fn prepare_session(
    catalog: &SessionCatalog,
    config: &SessionOptions,
    model: &dyn ModelProvider,
    values: &serde_json::Value,
) -> Result<
    (
        SessionMeta,
        crate::runtime::SessionLease,
        crate::context::prompt::PreparedPrompt,
    ),
    YourAiError,
> {
    let prompt = resolved_prompt(
        &config.config,
        &crate::config::read::<crate::config::PromptSettings>(values, "prompt"),
    );
    let existing = match &config.resume {
        Some(id) => Some(catalog.store.load_session(id).await?),
        None => None,
    };
    let lease = match &existing {
        Some(meta) => Some(crate::runtime::SessionLease::acquire(
            catalog.directory(&meta.id)?,
            &meta.id,
            catalog.store.clone(),
        )?),
        None => None,
    };
    let prepared = if let Some(system) = existing.as_ref().and_then(|m| m.system_prompt.as_ref()) {
        // Frozen sessions must not depend on today's files or external services.
        crate::context::prompt::PreparedPrompt {
            system: system.clone(),
            notices: vec![],
            instructions: Default::default(),
        }
    } else {
        crate::context::prompt::prepare(
            &prompt,
            &config.cwd,
            prompt.system_prompt().as_deref(),
            crate::config::read::<ExecutionOptions>(values, "execution").instruction_paths(),
            config.skill_provider.as_deref(),
            config.memory_manager.as_deref(),
            crate::config::read::<ExecutionOptions>(values, "execution")
                .context_policy()
                .maintenance_threshold(model.token_budget()),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await?
    };
    let mut meta = match existing {
        Some(mut meta) => {
            if meta.system_prompt.is_none() {
                meta.system_prompt = Some(
                    catalog
                        .store
                        .initialize_system(&meta.id, &prepared.system)
                        .await?,
                );
            }
            meta
        }
        None => {
            catalog
                .store
                .create_session(SessionId::new(), &prepared.system)
                .await?
        }
    };
    let lease = match lease {
        Some(lease) => lease,
        None => crate::runtime::SessionLease::acquire(
            catalog.directory(&meta.id)?,
            &meta.id,
            catalog.store.clone(),
        )?,
    };
    catalog.import_state(&meta.id, lease.store().as_ref())?;
    catalog.import_outputs(&meta.id, lease.store()).await?;
    meta.model = Some(model.model_iden().into());
    lease.store().save_session(&meta).await?;
    Ok((meta, lease, prepared))
}

/// Prepare model resources from coherent input data, retaining the same shared Config.
pub fn resolve_session(
    config: &Arc<crate::config::Config>,
    template: &SessionOptions,
    id: Option<SessionId>,
) -> Result<(SessionOptions, Arc<dyn ModelProvider>), crate::config::Error> {
    let values = config.values();
    let selection = crate::model::configured::resolve_data(
        &values,
        values["model"].as_str().unwrap(),
        values["variant"].as_str(),
    )?;
    let mut options = template.clone();
    options.resume = id;
    options.config = Arc::clone(config);
    options.compaction_model = values["compaction_model"]
        .as_str()
        .map(|id| crate::model::configured::resolve_data(&values, id, None))
        .transpose()?;
    options.model_provider = selection.provider.clone();
    options.request_policy = selection.requests;
    Ok((options, selection.model))
}

fn data_session_dir(legacy_base: &std::path::Path) -> PathBuf {
    let data = match std::env::var_os("XDG_DATA_HOME") {
        Some(v) if !v.is_empty() && std::path::Path::new(&v).is_absolute() => PathBuf::from(v),
        _ => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(".local/share"),
            None => return legacy_base.join(".yourai/sessions"),
        },
    };
    let fresh = data.join("yourai/sessions");
    let legacy = legacy_base.join(".yourai/sessions");
    if !fresh.exists() && legacy.exists() {
        return legacy;
    }
    fresh
}

pub fn session_dir(config: &crate::config::Config) -> PathBuf {
    let base = config
        .path()
        .and_then(std::path::Path::parent)
        .unwrap_or_else(|| std::path::Path::new("."));
    let directory = config.directory();
    let root = if directory == std::path::Path::new(".yourai/sessions") {
        data_session_dir(base)
    } else {
        directory
    };
    if root.is_relative() {
        base.join(root)
    } else {
        root
    }
}
fn resolved_prompt(
    config: &crate::config::Config,
    settings: &crate::config::PromptSettings,
) -> crate::config::PromptSettings {
    let base = config
        .path()
        .and_then(std::path::Path::parent)
        .unwrap_or_else(|| std::path::Path::new("."));
    let mut settings = settings.clone();
    let resolve = |path: &Option<PathBuf>| {
        path.as_ref().map(|path| {
            if path.is_relative() {
                base.join(path)
            } else {
                path.clone()
            }
        })
    };
    settings.set_soul_file(resolve(settings.soul_file()));
    settings.set_memory_file(resolve(settings.memory_file()));
    settings.set_profile_file(resolve(settings.profile_file()));
    settings
}

pub fn validate_config(config: &crate::config::Config) -> Result<(), crate::config::Error> {
    let values = config.values();
    crate::model::configured::validate_data(&values, config.path().is_some())?;
    if crate::config::read::<ExecutionOptions>(&values, "execution").steps() == Some(0) {
        return Err("steps must be a positive integer".into());
    }
    crate::config::read::<ExecutionOptions>(&values, "execution")
        .context_policy()
        .validate()?;
    crate::hooks::build_registrations(
        crate::config::read::<crate::config::HookSettings>(&values, "hooks").events(),
        crate::hooks::registration::HookSource::Project,
    )?;
    Ok(())
}
