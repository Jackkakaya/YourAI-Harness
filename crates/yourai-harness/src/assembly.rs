//! One assembly path used equally by terminal and web drivers.
pub mod model_hooks;
use crate::hooks::{DefaultHookRuntime, HooksConfig};
use crate::{
    collaboration::*, memory::LocalMemory, skills::LocalSkills, storage::LocalUsage,
    workspace::Workspace, *,
};
use model_hooks::DefaultHookEvaluator;
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
    pub request_policy: crate::model::RequestPolicy,
    /// Stable provider key used to retain admission state across model switches.
    pub model_provider: String,
    pub model_header_timeout: Option<Duration>,
    pub model_chunk_timeout: Option<Duration>,
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
            request_policy: Default::default(),
            model_provider: String::new(),
            model_header_timeout: None,
            model_chunk_timeout: None,
        }
    }
}
/// Runtime settings published with the main model at the same idle boundary.
#[derive(Clone)]
pub struct ModelSettings {
    pub provider: String,
    pub requests: crate::model::RequestPolicy,
    pub header_timeout: Option<Duration>,
    pub chunk_timeout: Option<Duration>,
}
pub struct Harness {
    normal_security: Arc<dyn SecurityProvider>,
    provider_budgets: Mutex<HashMap<(String, crate::model::RequestPolicy), Arc<ModelBudget>>>,
    current_budget: Mutex<Arc<ModelBudget>>,
    /// Shared persistence interface for frontends reading clean session history.
    pub sessions: Arc<dyn SessionManager>,
    pub host: Arc<SessionHost>,
    pub tools: Arc<ToolSet>,
    pub hooks: Arc<DefaultHookRuntime>,
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
        config.context_policy.validate()?;
        let model = crate::model::ConfiguredModel::wrap(
            model,
            config.model_header_timeout,
            config.model_chunk_timeout,
        )?;
        let catalog = Arc::new(SessionCatalog::new(&config.root)?);
        let source = if config.resume.is_some() {
            "resume"
        } else {
            "startup"
        };
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
        let mut initial_instructions = Default::default();
        let prepared = if existing.as_ref().is_none_or(|m| m.system_prompt.is_none()) {
            let prepared = crate::context::prompt::prepare(
                &config.prompt,
                &config.cwd,
                config.system_prompt.as_deref(),
                &config.instructions,
                config.skill_provider.as_deref(),
                config.memory_provider.as_deref(),
                &config.context_policy,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await?;
            prompt_notices = prepared.notices;
            initial_instructions = prepared.instructions;
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
                    .create_session(
                        SessionId::new(),
                        prepared.as_deref().expect("new session prompt"),
                    )
                    .await?
                    .id
            }
        };
        let lease = match lease {
            Some(lease) => lease,
            None => crate::runtime::SessionLease::acquire(catalog.directory(&id)?)?,
        };
        let dir = lease.dir.clone();
        let mut meta = catalog.load_session(&id).await?;
        meta.model = Some(model.model_iden().into());
        catalog.save_session(&meta).await?;
        let budget =
            ModelBudget::configured(config.request_policy.clone(), (*catalog.store).clone())?;
        let model: Arc<dyn ModelProvider> = Arc::new(MeteredModel {
            inner: model,
            budget: budget.clone(),
        });
        let usage = Arc::new(LocalUsage((*catalog.store).clone()));
        let hooks = Arc::new(DefaultHookRuntime::new().with_evaluator(Arc::new(
            DefaultHookEvaluator {
                model: Arc::new(crate::model::SourceModel {
                    inner: model.clone(),
                    source: "hook",
                }),
                tools: None,
                usage: Some(usage.clone()),
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
        let memory = if config.extensions {
            Some(Arc::new(LocalMemory::open(dir.join("memory.json"))?))
        } else {
            None
        };
        let skills = if config.extensions {
            Some(Arc::new(LocalSkills::open(dir.join("skills.json"))?))
        } else {
            None
        };
        if let Some(provider) = config.memory_provider {
            agent.ctx().set_memory(provider);
        } else if let Some(memory) = &memory {
            agent.ctx().set_memory(memory.clone());
        }
        if let Some(provider) = agent.ctx().try_memory() {
            crate::memory::register(hooks.as_ref(), provider, catalog.clone(), id.clone()).await?;
        }
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
        context.instructions = initial_instructions;
        let host = SessionHost::open_owned(
            lease,
            context,
            agent,
            HostConfig {
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
                Some(subagent(&host, model, None))
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
                sessions: catalog,
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
    /// Existing hook and subagent executors retain their configured models.
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
        if [settings.header_timeout, settings.chunk_timeout]
            .into_iter()
            .flatten()
            .any(|d| d.is_zero())
        {
            return Err(ErrorKind::Config("model timeouts must be positive".into()).into());
        }
        self.switch_model_inner(model, policy, Some(settings)).await
    }
    async fn switch_model_inner(
        &self,
        model: Arc<dyn ModelProvider>,
        policy: ContextPolicy,
        settings: Option<ModelSettings>,
    ) -> Result<(), YourAiError> {
        policy.validate()?;
        let model = if let Some(settings) = &settings {
            crate::model::ConfiguredModel::wrap(
                model,
                settings.header_timeout,
                settings.chunk_timeout,
            )?
        } else {
            model
        };
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
        let history = DefaultContext::new(
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
        self.host.agent().ctx().set_context_manager(history);
        self.host.agent().ctx().set_model(model);
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
    let dir = catalog.directory(id)?;
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
        Some(catalog.tool_output.clone()),
    )?);
    let services = crate::context::ContextServices {
        store: Some(catalog.clone()),
        policy,
        ..Default::default()
    };
    let history = DefaultContext::new(id.clone(), services);
    let mut builder = Agent::builder()
        .agent_loop(Arc::new(crate::default_loop::DefaultLoop::new(
            crate::default_loop::LoopConfig {
                execution: yourai_core::execution::ExecutionConfig {
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
