//! One assembly path used equally by terminal and web drivers.
pub mod model_hooks;
use crate::hooks::{ConcreteHookRuntime, HooksConfig};
use crate::{
    collaboration::*, memory::LocalMemory, skills::LocalSkills, storage::LocalUsage,
    workspace::Workspace, *,
};
use model_hooks::DefaultHookModelExecutor;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use yourai_core::prelude::*;

pub struct HarnessConfig {
    pub root: PathBuf,
    pub cwd: PathBuf,
    pub resume: Option<SessionId>,
    pub hooks: HooksConfig,
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
    pub max_shared_model_calls: Option<u64>,
    pub max_known_tokens: Option<u64>,
    pub request_policy: crate::model::RequestPolicy,
}
impl HarnessConfig {
    pub fn new(root: PathBuf, cwd: PathBuf) -> Self {
        Self {
            root,
            cwd,
            resume: None,
            hooks: HooksConfig::default(),
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
            max_shared_model_calls: Some(256),
            max_known_tokens: None,
            request_policy: Default::default(),
        }
    }
}
pub struct Harness {
    /// Shared persistence interface for frontends reading clean session history.
    pub sessions: Arc<dyn SessionManager>,
    pub host: Arc<SessionHost>,
    pub tools: Arc<ToolSet>,
    pub hooks: Arc<ConcreteHookRuntime>,
    pub budget: Arc<ModelBudget>,
    pub workspace: Option<Arc<Workspace>>,
    pub tasks: Option<Arc<TaskBoard>>,
    pub subagents: Option<Arc<SubagentTool>>,
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
        let mut prompt_notices = vec![];
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
        let mut meta = catalog.load_session(&id).await?;
        meta.model = Some(model.model_iden().into());
        catalog.save_session(&meta).await?;
        let dir = catalog.directory(&id)?;
        let budget = ModelBudget::configured(
            config.max_shared_model_calls,
            config.max_known_tokens,
            config.request_policy,
            (*catalog.store).clone(),
        )?;
        let model: Arc<dyn ModelProvider> = Arc::new(MeteredModel {
            inner: model,
            budget: budget.clone(),
        });
        let usage = Arc::new(LocalUsage((*catalog.store).clone()));
        let hooks = Arc::new(ConcreteHookRuntime::new().with_model_executor(Arc::new(
            DefaultHookModelExecutor {
                model: Arc::new(crate::model::SourceModel {
                    inner: model.clone(),
                    source: "hook",
                }),
                tools: None,
                usage: Some(usage.clone()),
                timeout: Duration::from_secs(60),
                max_model_calls: 8,
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
        agent
            .ctx()
            .set_agent_loop(Arc::new(crate::default_loop::DefaultLoop::new(
                crate::default_loop::LoopConfig {
                    memory_search_limit: config.memory_search_limit,
                    ..Default::default()
                },
            )));
        if let Some(provider) = config.skill_provider {
            agent.ctx().set_skills(provider);
        } else if let Some(skills) = &skills {
            agent.ctx().set_skills(skills.clone());
        }
        let mut context = SessionContext::new(id, config.cwd);
        context.transcript_path = Some(crate::SqliteStore::path(&config.root));
        let host = SessionHost::open(
            dir,
            context,
            agent,
            HostConfig {
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
        for notice in prompt_notices {
            host.post_event(yourai_core::runtime_event::RuntimeEvent {
                id: uuid::Uuid::new_v4().to_string(),
                context: None,
                notice: Some(notice),
                wake: false,
            })?;
        }
        let workspace = if config.extensions {
            Some(host.workspace()?)
        } else {
            None
        };
        // Task progress is a basic coding capability, independent of workspace/subagents.
        let tasks = Some(TaskBoard::new(&host, "default")?);
        let subagents = if config.extensions {
            Some(SubagentTool::new(&host, model, None))
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
            sessions: catalog,
            host,
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
    pub async fn close(&self) -> Result<Vec<In>, YourAiError> {
        self.host.close(Duration::from_secs(15)).await
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
    let registry = Arc::new(ToolSet::default());
    if let Some(tools) = inherited {
        for definition in tools.definitions() {
            if !matches!(
                definition.name.as_str(),
                "read_tool_result" | "read" | "write" | "edit" | "shell"
            ) {
                registry.register(tools.resolve(definition.name.as_str())?);
            }
        }
    }
    registry.extend(crate::tools::coding_tools(cwd)?);
    registry.register(Arc::new(crate::tools::result::ReadToolResult {
        session: id.clone(),
        store: catalog.clone(),
        max_chars: policy.tool_output_chars,
    }));
    let services = crate::context::ContextServices {
        store: Some(catalog.clone()),
        policy,
        ..Default::default()
    };
    let history = MemoryContext::new(id.clone(), services);
    let mut builder = Agent::builder()
        .agent_loop(Arc::new(crate::default_loop::DefaultLoop::default()))
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

// Public convenience entry points share the same concrete assembly code.
impl SessionHost {
    pub async fn create(
        root: &Path,
        cwd: PathBuf,
        model: Arc<dyn ModelProvider>,
        hooks: Option<Arc<dyn HookRuntime>>,
        tools: Option<Arc<dyn ToolRegistry>>,
    ) -> Result<Arc<Self>, YourAiError> {
        let catalog = Arc::new(crate::SessionCatalog::new(root)?);
        let prompt = crate::context::prompt::prepare(
            &crate::PromptConfig::default(),
            &cwd,
            None,
            &[],
            None,
            None,
            &ContextPolicy::default(),
            &CancellationToken::new(),
        )
        .await?;
        let meta = catalog.create_session(&prompt.system).await?;
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
        if meta.system_prompt.is_none() {
            let prompt = crate::context::prompt::prepare(
                &crate::PromptConfig::default(),
                &cwd,
                None,
                &[],
                None,
                None,
                &policy,
                &CancellationToken::new(),
            )
            .await?;
            meta.system_prompt = Some(catalog.initialize_system(&id, &prompt.system).await?);
        }
        meta.model = Some(model.model_iden().into());
        catalog.save_session(&meta).await?;
        let dir = catalog.directory(&id)?;
        let usage = Arc::new(crate::storage::LocalUsage((*catalog.store).clone()));
        let (agent, _) = crate::assembly::assemble(
            &catalog,
            &id,
            &cwd,
            false,
            model,
            hooks,
            Some(usage),
            tools,
            policy,
        )
        .await?;
        let mut context = SessionContext::new(id, cwd);
        context.transcript_path = Some(crate::SqliteStore::path(root));
        Self::open(dir, context, agent, HostConfig::default(), source).await
    }
}
