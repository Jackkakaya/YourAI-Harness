//! One assembly path used equally by terminal and web drivers.
pub mod model_hooks;
use crate::hooks::{ConcreteHookRuntime, HooksConfig};
use crate::{
    collaboration::*, memory::LocalMemory, skills::LocalSkills, storage::LocalUsage,
    workspace::Workspace, *,
};
use model_hooks::DefaultHookModelExecutor;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
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
    pub memory_provider: Option<Arc<dyn MemoryProvider>>,
    pub skill_provider: Option<Arc<dyn SkillProvider>>,
    pub memory_search_limit: usize,
    pub model_settings: ModelSettings,
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
            memory_provider: None,
            skill_provider: None,
            memory_search_limit: 0,
            model_settings: ModelSettings::default(),
        }
    }
}
/// Runtime settings published with the main model at the same idle boundary.
#[derive(Clone, Default)]
pub struct ModelSettings {
    pub provider: String,
    pub requests: crate::model::RequestPolicy,
}
pub struct Harness {
    normal_security: Arc<dyn SecurityProvider>,
    provider_budgets: Mutex<HashMap<(String, crate::model::RequestPolicy), Arc<ModelBudget>>>,
    current_budget: Mutex<Arc<ModelBudget>>,
    /// Shared persistence interface for frontends reading clean session history.
    pub sessions: Arc<SessionCatalog>,
    pub host: Arc<SessionHost>,
    pub tools: Arc<ToolSet>,
    pub hooks: Arc<ConcreteHookRuntime>,
    pub(crate) budget: Arc<ModelBudget>,
    pub workspace: Option<Arc<Workspace>>,
    pub tasks: Option<Arc<TaskBoard>>,
    pub subagents: Option<Arc<SubagentTool>>,
    /// Local administration is available only when the active provider is local.
    pub local_memory: Option<Arc<LocalMemory>>,
    pub local_skills: Option<Arc<LocalSkills>>,
    pub usage: Arc<LocalUsage>,
}
impl Harness {
    /// The supplied provider is ready to use; configuration is owned by its creator.
    /// Use ConfiguredModel::new once to bind defaults to a raw transport.
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
        let PreparedSession {
            id,
            lease,
            notices: prompt_notices,
        } = prepare_session(&catalog, &config, model.as_ref()).await?;
        let dir = lease.dir.clone();
        let budget = ModelBudget::configured(
            config.model_settings.requests.clone(),
            (*catalog.store).clone(),
        )?;
        let model: Arc<dyn ModelProvider> = Arc::new(MeteredModel {
            inner: model,
            budget: budget.clone(),
        });
        let usage = Arc::new(LocalUsage((*catalog.store).clone()));
        let hooks = Arc::new(ConcreteHookRuntime::new().with_model_executor(Arc::new(
            DefaultHookModelExecutor {
                selection: ModelSelection::Inherit,
                tools: None,
                usage: None,
                timeout: Duration::from_secs(60),
                steps: 8,
            },
        )));
        hooks
            .register_config(&config.hooks, HookSource::Project)
            .await?;
        let (agent, tools) = assemble(
            &catalog,
            &id,
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
        let memory = if config.extensions && config.memory_provider.is_none() {
            Some(Arc::new(LocalMemory::open(dir.join("memory.json"))?))
        } else {
            None
        };
        let skills = if config.extensions && config.skill_provider.is_none() {
            Some(Arc::new(LocalSkills::open(dir.join("skills.json"))?))
        } else {
            None
        };
        if let Some(provider) = config.memory_provider {
            agent.ctx().set_memory(provider);
        } else if let Some(memory) = &memory {
            agent.ctx().set_memory(memory.clone());
        }
        // Install the bridge once; each invocation resolves its execution snapshot.
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
        let mut context = SessionContext::new(id, config.cwd);
        context.transcript_path = Some(crate::SqliteStore::path(&config.root));
        let host = SessionHost::open_owned(
            lease,
            context,
            agent,
            HostConfig {
                input,
                // Preserve initial instruction lifecycle hooks; reopening uses the
                // frozen snapshot and must not require the original files.
                instruction_watch_paths: if source == "startup" {
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
        let tasks = Some(TaskBoard::new(&host, "default")?);
        let subagents = if config.extensions {
            Some(SubagentTool::new(&host, ModelSelection::Inherit, None))
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
                (
                    config.model_settings.provider,
                    config.model_settings.requests,
                ),
                budget.clone(),
            )])),
            current_budget: Mutex::new(budget.clone()),
            sessions: catalog,
            host,
            tools,
            hooks,
            budget,
            workspace,
            tasks,
            subagents,
            local_memory: memory,
            local_skills: skills,
            usage,
        })
    }
    /// Change permissions only between turns, preserving the original policy.
    /// In-flight tool snapshots and approval questions must finish or be cancelled first.
    pub fn set_yolo(&self, enabled: bool) -> Result<(), YourAiError> {
        let _gate = self.host.try_operation()?;
        self.host.agent.ctx().set_security(if enabled {
            Arc::new(crate::security::YoloSecurity)
        } else {
            self.normal_security.clone()
        });
        Ok(())
    }
    /// Replace the main model and its context policy at an idle boundary.
    /// Admission/metrics keep the existing shared budget (including calls already
    /// spent); active turns and compaction reject the switch without changes.
    /// Inherited hooks and newly created children use the new selection; pinned models stay fixed.
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
        let id = self.host.context().id;
        let history = MemoryContext::new(
            id.clone(),
            crate::context::ContextServices {
                store: Some(self.sessions.clone()),
                policy,
                ..Default::default()
            },
        );
        // Prepare everything that can fail before publishing either provider.
        history.restore().await?;
        let mut meta = self.sessions.load_session(&id).await?;
        meta.model = Some(model.model_iden().into());
        self.sessions.save_session(&meta).await?;
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
        self.host.agent.ctx().update(|providers| {
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
    cwd: &std::path::Path,
    trusted_shell: bool,
    model: Arc<dyn ModelProvider>,
    hooks: Option<Arc<dyn HookRuntime>>,
    usage: Option<Arc<dyn UsageTracker>>,
    inherited: Option<Arc<dyn ToolRegistry>>,
    policy: ContextPolicy,
) -> Result<(Arc<Agent>, Arc<ToolSet>), YourAiError> {
    policy.validate_for(model.token_budget())?;
    let dir = catalog.directory(id)?;
    let registry = Arc::new(ToolSet::default());
    if let Some(tools) = inherited {
        for definition in tools.definitions() {
            if !crate::tools::BUILTIN_TOOL_NAMES.contains(&definition.name.as_str()) {
                registry.register_binding(tools.resolve(definition.name.as_str())?);
            }
        }
    }
    registry.extend(crate::tools::coding_tools_with_output(
        cwd,
        Some(catalog.tool_output.clone()),
    )?);
    let services = crate::context::ContextServices {
        store: Some(catalog.clone()),
        policy,
        ..Default::default()
    };
    let history = MemoryContext::new(id.clone(), services);
    let mut builder = Agent::builder()
        .agent_loop(Arc::new(crate::default_loop::DefaultLoop::new(
            crate::default_loop::LoopConfig {
                execution: crate::execution::ExecutionConfig {
                    tool_output: Some(catalog.tool_output.clone()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )))
        .model(model)
        .context_manager(history)
        .session(catalog.clone())
        .tools(registry.clone())
        .security(PolicySecurity::open_for_workspace(
            dir.join("permissions.json"),
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

pub(crate) struct PreparedSession {
    pub id: SessionId,
    pub lease: crate::runtime::SessionLease,
    pub notices: Vec<String>,
}
/// Single owner of prompt freezing, resume leases and session metadata setup.
pub(crate) async fn prepare_session(
    catalog: &SessionCatalog,
    config: &HarnessConfig,
    model: &dyn ModelProvider,
) -> Result<PreparedSession, YourAiError> {
    config.model_settings.requests.validate()?;
    config.context_policy.validate_for(model.token_budget())?;
    // Existing snapshots must not depend on files or external services at resume.
    let existing = match &config.resume {
        Some(id) => Some(catalog.load_session(id).await?),
        None => None,
    };
    // A refused resume must not initialize prompts, rewrite metadata or
    // assemble providers for a session owned by another host.
    let lease = match &existing {
        Some(meta) => Some(crate::runtime::SessionLease::acquire(
            catalog.directory(&meta.id)?,
        )?),
        None => None,
    };
    let mut prompt_notices = vec![];
    let prepared = if existing.as_ref().is_none_or(|m| m.system_prompt.is_none()) {
        let prepared = crate::context::prompt::prepare(
            &config.prompt,
            &config.cwd,
            config.system_prompt.as_deref(),
            &config.instructions,
            config.skill_provider.as_deref(),
            config.memory_provider.as_deref(),
            config
                .context_policy
                .maintenance_threshold(model.token_budget()),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await?;
        prompt_notices = prepared.notices;
        Some(prepared.system)
    } else {
        None
    };
    let id = match existing {
        Some(meta) => {
            if let Some(system) = &prepared {
                catalog.initialize_system(&meta.id, system).await?;
            }
            meta.id
        }
        None => {
            catalog
                .create_session(prepared.as_deref().expect("new session prompt"))
                .await?
                .id
        }
    };
    let lease = match lease {
        Some(lease) => lease,
        None => crate::runtime::SessionLease::acquire(catalog.directory(&id)?)?,
    };
    let mut meta = catalog.load_session(&id).await?;
    meta.model = Some(model.model_iden().into());
    catalog.save_session(&meta).await?;
    Ok(PreparedSession {
        id,
        lease,
        notices: prompt_notices,
    })
}
