//! One assembly path used equally by terminal and web drivers.
use crate::{extensions::*, local::*, model_hooks::DefaultHookModelExecutor, *};
use std::{path::PathBuf, sync::Arc, time::Duration};
use yourai_core::prelude::*;
use yourai_hooks::{ConcreteHookRuntime, HooksConfig};

pub struct HarnessConfig {
    pub root: PathBuf,
    pub cwd: PathBuf,
    pub resume: Option<SessionId>,
    pub hooks: HooksConfig,
    pub instructions: Vec<PathBuf>,
    pub context_policy: ContextPolicy,
    pub extensions: bool,
    pub system_prompt: Option<String>,
    pub max_shared_model_calls: Option<u64>,
    pub max_known_tokens: Option<u64>,
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
            system_prompt: None,
            max_shared_model_calls: Some(256),
            max_known_tokens: None,
        }
    }
}
pub struct Harness {
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
        let id = match config.resume {
            Some(id) => {
                catalog.load_session(&id).await?;
                id
            }
            None => catalog.create_session().await?.id,
        };
        let mut meta = catalog.load_session(&id).await?;
        meta.model = Some(model.model_iden().into());
        catalog.save_session(&meta).await?;
        let dir = catalog.directory(&id)?;
        let budget = ModelBudget::new(config.max_shared_model_calls, config.max_known_tokens);
        let model: Arc<dyn ModelProvider> = Arc::new(MeteredModel {
            inner: model,
            budget: budget.clone(),
        });
        let usage = Arc::new(LocalUsage((*catalog.store).clone()));
        let hooks = Arc::new(ConcreteHookRuntime::new().with_model_executor(Arc::new(
            DefaultHookModelExecutor {
                model: model.clone(),
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
            model.clone(),
            Some(hooks.clone()),
            Some(usage.clone()),
            None,
            config.context_policy,
            config.system_prompt,
        )
        .await?;
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
        if let Some(memory) = &memory {
            agent.ctx().set_memory(memory.clone());
        }
        if let Some(skills) = &skills {
            agent.ctx().set_skills(skills.clone());
        }
        let mut context = SessionContext::new(id, config.cwd);
        context.transcript_path = Some(crate::SqliteStore::path(&config.root));
        let host = SessionHost::open(
            dir,
            context,
            agent,
            HostConfig {
                instruction_paths: config.instructions,
                workspace_enabled: config.extensions,
                ..Default::default()
            },
            source,
        )
        .await?;
        let workspace = if config.extensions {
            Some(host.workspace()?)
        } else {
            None
        };
        let tasks = if config.extensions {
            Some(TaskBoard::new(&host, "default")?)
        } else {
            None
        };
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
    model: Arc<dyn ModelProvider>,
    hooks: Option<Arc<dyn HookRuntime>>,
    usage: Option<Arc<dyn UsageTracker>>,
    inherited: Option<Arc<dyn ToolRegistry>>,
    policy: ContextPolicy,
    system_prompt: Option<String>,
) -> Result<(Arc<Agent>, Arc<ToolSet>), YourAiError> {
    let dir = catalog.directory(id)?;
    let registry = Arc::new(ToolSet::default());
    if let Some(tools) = inherited {
        for definition in tools.definitions() {
            if !matches!(definition.name.as_str(), "read_tool_result") {
                registry.register(tools.resolve(definition.name.as_str())?);
            }
        }
    }
    registry.register(Arc::new(crate::tool_result::ReadToolResult {
        session: id.clone(),
        store: catalog.clone(),
        max_chars: policy.tool_output_chars,
    }));
    let services = crate::memory_context::ContextServices {
        store: Some(catalog.clone()),
        policy,
        ..Default::default()
    };
    let history = MemoryContext::new(id.clone(), services);
    let mut builder = Agent::builder()
        .agent_loop(Arc::new(yourai_loop::DefaultLoop::new(
            yourai_loop::LoopConfig {
                system_prompt,
                ..Default::default()
            },
        )))
        .model(model)
        .context_manager(history)
        .session(catalog.clone())
        .tools(registry.clone())
        .security(PolicySecurity::open(dir.join("permissions.json"), vec![])?);
    if let Some(hooks) = hooks {
        builder = builder.hooks(hooks);
    }
    if let Some(usage) = usage {
        builder = builder.usage(usage);
    }
    Ok((builder.build(), registry))
}
