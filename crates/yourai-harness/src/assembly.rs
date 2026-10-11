//! One assembly path used equally by terminal and web drivers.
use crate::hooks::{DefaultHookManager, HooksConfig};
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

#[derive(Clone)]
pub struct HarnessConfig {
    pub root: PathBuf,
    pub cwd: PathBuf,
    pub resume: Option<SessionId>,
    pub hooks: HooksConfig,
    /// Optional replacement of the original AgentLoop scheduling interface.
    pub agent_loop: Option<Arc<dyn AgentLoop>>,
    pub instructions: Vec<PathBuf>,
    pub context_policy: ContextPolicy,
    pub extensions: bool,
    /// Explicit local trust: skip per-command shell approval. No OS isolation.
    pub trusted_shell: bool,
    /// Skip all tool permission approvals for this run, including subagents.
    pub yolo: bool,
    pub system_prompt: Option<String>,
    pub prompt: crate::PromptConfig,
    pub memory_manager: Option<Arc<dyn MemoryManager>>,
    pub skill_provider: Option<Arc<dyn SkillProvider>>,
    pub memory_search_limit: usize,
    pub request_policy: crate::model::RequestPolicy,
    /// Stable provider key used to retain admission state across model switches.
    pub model_provider: String,
}
impl HarnessConfig {
    pub fn new(root: PathBuf, cwd: PathBuf) -> Self {
        Self {
            root,
            cwd,
            resume: None,
            hooks: HooksConfig::default(),
            agent_loop: None,
            instructions: vec![],
            context_policy: ContextPolicy::default(),
            extensions: false,
            trusted_shell: false,
            yolo: false,
            system_prompt: None,
            prompt: crate::PromptConfig::default(),
            memory_manager: None,
            skill_provider: None,
            memory_search_limit: 0,
            request_policy: Default::default(),
            model_provider: String::new(),
        }
    }
}
/// Runtime settings published with the main model at the same idle boundary.
#[derive(Clone)]
pub struct ModelSettings {
    pub provider: String,
    pub requests: crate::model::RequestPolicy,
}
pub struct Harness {
    normal_security: Arc<dyn SecurityProvider>,
    provider_budgets: Mutex<HashMap<(String, crate::model::RequestPolicy), Arc<ModelBudget>>>,
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
        config: HarnessConfig,
        model: Arc<dyn ModelProvider>,
    ) -> Result<Self, YourAiError> {
        config.context_policy.validate_for(model.token_budget())?;
        let catalog = Arc::new(SessionCatalog::new(&config.root)?);
        let source = if config.resume.is_some() {
            "resume"
        } else {
            "startup"
        };
        let (meta, lease, prepared) = prepare_session(&catalog, &config, model.as_ref()).await?;
        let id = meta.id;
        let prompt_notices = prepared.notices;
        let initial_instructions = prepared.instructions;
        let budget =
            ModelBudget::configured(config.request_policy.clone(), (*catalog.store).clone())?;
        let model: Arc<dyn ModelProvider> = Arc::new(MeteredModel {
            inner: model,
            budget: budget.clone(),
        });
        let usage = Arc::new(LocalUsage((*catalog.store).clone()));
        let hooks = Arc::new(DefaultHookManager::new().with_model(model.clone()));
        hooks
            .register_config(&config.hooks, HookSource::Project)
            .await?;
        let (agent, tools) = assemble(
            &catalog,
            &id,
            lease.store(),
            &config.cwd,
            config.trusted_shell,
            model.clone(),
            Some(hooks.clone()),
            Some(usage.clone()),
            None,
            config.context_policy,
        )
        .await?;
        let normal_security = agent.ctx().security()?;
        if config.yolo {
            agent
                .ctx()
                .set_security(Arc::new(crate::security::YoloSecurity));
        }
        let memory = if config.extensions {
            Some(Arc::new(LocalMemory::new(lease.store(), id.clone())))
        } else {
            None
        };
        let skills = if config.extensions {
            Some(Arc::new(LocalSkills::new(lease.store(), id.clone())))
        } else {
            None
        };
        if let Some(provider) = config.memory_manager {
            agent.ctx().set_memory(provider);
        } else if let Some(memory) = &memory {
            agent.ctx().set_memory(memory.clone());
        }
        crate::memory::register(hooks.as_ref(), id.clone()).await?;
        let input = yourai_core::turn::InputOptions {
            memory_search_limit: config.memory_search_limit,
            ..Default::default()
        };
        if let Some(agent_loop) = config.agent_loop {
            agent.ctx().set_agent_loop(agent_loop);
        }
        if let Some(provider) = config.skill_provider {
            agent.ctx().set_skills(provider);
        } else if let Some(skills) = &skills {
            agent.ctx().set_skills(skills.clone());
        }
        let mut environment = SessionEnvironment::new(id, config.cwd);
        environment.transcript_path = Some(crate::SqliteStore::path(&config.root));
        environment.instructions = initial_instructions;
        let host = SessionManager::open_owned(
            lease,
            environment,
            agent,
            SessionConfig {
                input,
                // Preserve initial instruction lifecycle hooks; reopening uses the
                // frozen snapshot and must not require the original files.
                instruction_paths: if source == "startup" {
                    config.instructions
                } else {
                    vec![]
                },
                workspace_enabled: config.extensions,
                ..Default::default()
            },
            source,
        )
        .await?;
        let ready = async {
            for notice in prompt_notices {
                host.post_event_async(yourai_core::runtime_event::RuntimeEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    context: None,
                    notice: Some(notice),
                    wake: false,
                })
                .await?;
            }
            let workspace = if config.extensions {
                Some(host.workspace()?)
            } else {
                None
            };
            // Task progress is a basic coding capability, independent of workspace/subagents.
            let tasks = Some(host.task_manager("default")?);
            let subagents = if config.extensions {
                Some(subagent(&host, None, None))
            } else {
                None
            };
            if let Some(tasks) = &tasks {
                tools.register(tasks.clone());
            }
            if let Some(subagents) = &subagents {
                tools.register(subagents.clone());
            }
            Ok(Self {
                normal_security,
                provider_budgets: Mutex::new(HashMap::from([(
                    (config.model_provider, config.request_policy),
                    budget.clone(),
                )])),
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
    /// Change permissions only between turns, preserving the original policy.
    /// In-flight tool snapshots and approval questions must finish or be cancelled first.
    pub fn set_yolo(&self, enabled: bool) -> Result<(), YourAiError> {
        let _gate = self.host.try_operation()?;
        self.host.agent().ctx().set_security(if enabled {
            Arc::new(crate::security::YoloSecurity)
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
        self.switch_model_inner(model, policy, None).await
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
        settings: ModelSettings,
    ) -> Result<(), YourAiError> {
        settings.requests.validate()?;
        self.switch_model_inner(model, policy, Some(settings)).await
    }
    async fn switch_model_inner(
        &self,
        model: Arc<dyn ModelProvider>,
        policy: ContextPolicy,
        settings: Option<ModelSettings>,
    ) -> Result<(), YourAiError> {
        policy.validate_for(model.token_budget())?;
        let _gate = self.host.try_operation()?;
        let selected_budget = if let Some(settings) = &settings {
            if let Some(budget) = self
                .provider_budgets
                .lock()
                .unwrap()
                .get(&(settings.provider.clone(), settings.requests.clone()))
                .cloned()
            {
                budget
            } else {
                self.budget.for_provider(settings.requests.clone())?
            }
        } else {
            self.current_budget.lock().unwrap().clone()
        };
        let id = self.host.environment().id;
        let history =
            DefaultContext::new(id.clone(), String::new(), Some(self.host.store()), policy);
        // Prepare everything that can fail before publishing either provider.
        history.restore().await?;
        let mut meta = self.sessions.load_session(&id).await?;
        meta.model = Some(model.model_iden().into());
        self.host.store().save_session(&meta).await?;
        let model = Arc::new(MeteredModel {
            inner: model,
            budget: selected_budget.clone(),
        });
        if let Some(settings) = settings {
            self.provider_budgets.lock().unwrap().insert(
                (settings.provider, settings.requests),
                selected_budget.clone(),
            );
        }
        *self.current_budget.lock().unwrap() = selected_budget;
        self.host.agent().ctx().update(|providers| {
            providers.context_manager = Some(history);
            providers.model = Some(model);
        });
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

#[allow(clippy::too_many_arguments)]
pub(crate) async fn assemble(
    catalog: &Arc<SessionCatalog>,
    id: &SessionId,
    store: Arc<dyn SessionStore>,
    cwd: &std::path::Path,
    trusted_shell: bool,
    model: Arc<dyn ModelProvider>,
    hooks: Option<Arc<dyn HookManager>>,
    usage: Option<Arc<dyn UsageTracker>>,
    inherited: Option<Arc<dyn ToolRegistry>>,
    policy: ContextPolicy,
) -> Result<(Arc<Agent>, Arc<ToolSet>), YourAiError> {
    let _dir = catalog.directory(id)?;
    let output = crate::tools::ToolOutputStore::new(store.clone(), id.clone());
    let inherited = inherited
        .map(|registry| {
            registry
                .snapshot()
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

    let history = DefaultContext::new(id.clone(), String::new(), Some(store.clone()), policy);
    let mut builder = Agent::builder()
        .agent_loop(Arc::new(crate::default_loop::DefaultLoop::new(
            crate::default_loop::LoopConfig {
                execution: yourai_core::execution::ExecutionConfig {
                    tool_output: Some(output.clone()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )))
        .model(model)
        .context_manager(history)
        .session(store.clone())
        .tools(registry.clone())
        .security(PolicySecurity::open_for_workspace(
            store,
            id.clone(),
            vec![],
            cwd.to_owned(),
            trusted_shell,
        )?);
    if let Some(hooks) = hooks {
        builder = builder.hooks(hooks);
    }
    if let Some(usage) = usage {
        builder = builder.usage(usage);
    }
    Ok((builder.build(), registry))
}

/// One initialization path for Harness, standalone hosts and child sessions.
/// Resume acquires ownership before any prompt preparation or metadata write.
pub(crate) async fn prepare_session(
    catalog: &SessionCatalog,
    config: &HarnessConfig,
    model: &dyn ModelProvider,
) -> Result<
    (
        SessionMeta,
        crate::runtime::SessionLease,
        crate::context::prompt::PreparedPrompt,
    ),
    YourAiError,
> {
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
            &config.prompt,
            &config.cwd,
            config.system_prompt.as_deref(),
            &config.instructions,
            config.skill_provider.as_deref(),
            config.memory_manager.as_deref(),
            config
                .context_policy
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
