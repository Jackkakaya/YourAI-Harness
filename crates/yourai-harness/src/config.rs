use crate::GenaiModel;
use genai::{
    adapter::AdapterKind,
    chat::ChatOptions,
    resolver::{AuthData, Endpoint},
    Client,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use yourai_core::prelude::{ModelLimits, ModelProvider, ModelTimeouts, ModelTokenBudget};
pub type Error = Box<dyn std::error::Error + Send + Sync>;
/// Borrowed option maps for one picker entry.
type EntryOptions<'a> = (&'a Map<String, Value>, Option<&'a Map<String, Value>>);

/// The sole shared configuration. Categories are data, with no separate storage.
#[derive(Clone, Default, Serialize)]
pub struct Config {
    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub models: ModelSettings,
    pub execution: yourai_core::execution::ExecutionOptions,
    pub security: SecuritySettings,
    pub ui: UiSettings,
    pub session: SessionSettings,
    pub prompt: PromptSettings,
    pub hooks: HookSettings,
    #[serde(skip)]
    path: Option<PathBuf>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelSettings {
    pub selected: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub effort_overrides: BTreeMap<String, BTreeMap<String, String>>,
    pub providers: BTreeMap<String, Value>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecuritySettings {
    pub trusted_shell: bool,
    pub yolo: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiSettings {
    pub theme: String,
}
impl Default for UiSettings {
    fn default() -> Self {
        Self {
            theme: default_theme(),
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionSettings {
    pub directory: PathBuf,
    pub extensions: bool,
}
impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            directory: sessions(),
            extensions: false,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PromptSettings {
    pub system_prompt: Option<String>,
    pub soul_file: Option<PathBuf>,
    pub memory_file: Option<PathBuf>,
    pub profile_file: Option<PathBuf>,
    pub append_system_prompt: Option<String>,
    pub memory_file_max_chars: usize,
    pub profile_file_max_chars: usize,
    pub provider_max_chars: usize,
}
impl Default for PromptSettings {
    fn default() -> Self {
        Self {
            system_prompt: None,
            soul_file: None,
            memory_file: None,
            profile_file: None,
            append_system_prompt: None,
            memory_file_max_chars: 12000,
            profile_file_max_chars: 6000,
            provider_max_chars: 8000,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HookSettings {
    pub events: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
}
impl Default for HookSettings {
    fn default() -> Self {
        Self {
            events: empty_object(),
            file: None,
        }
    }
}
fn empty_object() -> Value {
    Value::Object(Map::new())
}

impl<'de> Deserialize<'de> for Config {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut value = normalize_document(Value::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)?;
        let object = value.as_object_mut().unwrap();
        macro_rules! category {
            ($key:literal) => {
                serde_json::from_value(object.remove($key).unwrap_or_else(empty_object))
                    .map_err(serde::de::Error::custom)?
            };
        }
        Ok(Self {
            schema: serde_json::from_value(object.remove("$schema").unwrap_or(Value::Null))
                .map_err(serde::de::Error::custom)?,
            models: category!("models"),
            execution: category!("execution"),
            security: category!("security"),
            ui: category!("ui"),
            session: category!("session"),
            prompt: category!("prompt"),
            hooks: category!("hooks"),
            path: None,
        })
    }
}

/// Convert legacy flat keys once; grouped values cannot be silently overridden.
fn normalize_document(mut value: Value) -> Result<Value, Error> {
    let object = value.as_object_mut().ok_or("config must be an object")?;
    // Old hooks is an event map; new hooks contains events and an optional file.
    if let Some(hooks) = object.get_mut("hooks") {
        let map = hooks.as_object().ok_or("hooks must be an object")?;
        if !map.contains_key("events") && !map.contains_key("file") {
            *hooks = serde_json::json!({"events": hooks.clone()});
        }
    }
    for (old, category, key) in LEGACY_FIELDS {
        if let Some(value) = object.remove(*old) {
            let group = object
                .entry(*category)
                .or_insert_with(empty_object)
                .as_object_mut()
                .ok_or_else(|| format!("{category} must be an object"))?;
            if group.contains_key(*key) {
                return Err(format!("duplicate config: {old} and {category}.{key}").into());
            }
            group.insert((*key).into(), value);
        }
    }
    check_fields(
        &value,
        &[
            "$schema",
            "models",
            "execution",
            "security",
            "ui",
            "session",
            "prompt",
            "hooks",
        ],
        "config",
    )?;
    Ok(value)
}
const LEGACY_FIELDS: &[(&str, &str, &str)] = &[
    ("skill_ids", "execution", "skill_ids"),
    ("memory_search_limit", "execution", "memory_search_limit"),
    ("context", "execution", "context"),
    ("steps", "execution", "steps"),
    ("model_timeout", "execution", "model_timeout"),
    (
        "allow_background_wake",
        "execution",
        "allow_background_wake",
    ),
    ("max_followups", "execution", "max_followups"),
    ("instruction_paths", "execution", "instruction_paths"),
    ("workspace_enabled", "execution", "workspace_enabled"),
    ("memory_max_chars", "execution", "memory_max_chars"),
    ("attachment_image", "execution", "attachment_image"),
    (
        "attachment_text_max_chars",
        "execution",
        "attachment_text_max_chars",
    ),
    ("max_model_retries", "execution", "max_model_retries"),
    (
        "max_overflow_compactions",
        "execution",
        "max_overflow_compactions",
    ),
    (
        "max_stop_continuations",
        "execution",
        "max_stop_continuations",
    ),
    (
        "max_permission_rechecks",
        "execution",
        "max_permission_rechecks",
    ),
    ("retry_delay", "execution", "retry_delay"),
    ("retry_max_delay", "execution", "retry_max_delay"),
    ("operation_timeout", "execution", "operation_timeout"),
    ("tool_timeout", "execution", "tool_timeout"),
    ("approval_timeout", "execution", "approval_timeout"),
    ("hook_timeout", "execution", "hook_timeout"),
    ("cleanup_timeout", "execution", "cleanup_timeout"),
    ("tool_cleanup_timeout", "execution", "tool_cleanup_timeout"),
    ("system_prompt", "prompt", "system_prompt"),
    ("soul_file", "prompt", "soul_file"),
    ("memory_file", "prompt", "memory_file"),
    ("profile_file", "prompt", "profile_file"),
    ("append_system_prompt", "prompt", "append_system_prompt"),
    ("memory_file_max_chars", "prompt", "memory_file_max_chars"),
    ("profile_file_max_chars", "prompt", "profile_file_max_chars"),
    ("provider_max_chars", "prompt", "provider_max_chars"),
    ("model", "models", "selected"),
    ("compaction_model", "models", "compaction"),
    ("variant", "models", "variant"),
    ("effort_overrides", "models", "effort_overrides"),
    ("provider", "models", "providers"),
    ("theme", "ui", "theme"),
    ("trusted_shell", "security", "trusted_shell"),
    ("yolo", "security", "yolo"),
    ("session_dir", "session", "directory"),
    ("extensions", "session", "extensions"),
    ("hooks_file", "hooks", "file"),
    ("max_model_calls", "execution", "steps"),
];

/// Edit the cached configuration and synchronously save the changed sources.
/// No file is re-read. Validation or saving failure leaves shared memory unchanged.
pub fn update(
    config: &Arc<std::sync::Mutex<Config>>,
    change: impl FnOnce(&mut Config) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut current = config.lock().map_err(|_| "config lock poisoned")?;
    let mut next = current.clone();
    change(&mut next)?;
    next.validate()?;
    let path = next
        .path
        .as_deref()
        .ok_or("config has no storage path; load it before updating")?;
    let prompt_changed = next.prompt_document()? != current.prompt_document()?;
    if prompt_changed {
        write_document(
            &path.parent().unwrap().join("prompts.json"),
            &next.prompt_document()?,
        )?;
    }
    if next.hooks.events != current.hooks.events || next.hooks.file != current.hooks.file {
        if let Some(path) = next.hooks_path() {
            write_document(
                &path,
                &serde_json::json!({"hooks": {"events": next.hooks.events}}),
            )?;
        }
    }
    if next.main_document()? != current.main_document()? {
        next.save()?;
    }
    *current = next;
    Ok(())
}

fn storage_path(path: &Path) -> PathBuf {
    if let Ok(path) = path.canonicalize() {
        return path;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(parent) = parent.canonicalize() {
            return parent.join(name);
        }
    }
    // Missing sources still need lexical alias checks before any write.
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn is_sqlite(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("db" | "sqlite" | "sqlite3")
    )
}
fn decode<T: serde::de::DeserializeOwned>(value: Value, kind: &str) -> Result<T, Error> {
    serde_json::from_value(value).map_err(|e| {
        let message = e.to_string();
        if message.starts_with("unknown field") || message.starts_with("missing field") {
            format!("Invalid {kind}: {message}").into()
        } else {
            format!("Invalid {kind} value type").into()
        }
    })
}
fn parse_document(bytes: &[u8]) -> Result<Value, Error> {
    serde_json::from_slice(bytes)
        .map_err(|e| format!("Invalid JSON at line {}, column {}", e.line(), e.column()).into())
}
fn read_document(path: &Path) -> Result<Value, Error> {
    if is_sqlite(path) {
        use rusqlite::OptionalExtension;
        let db = rusqlite::Connection::open(path)?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS app_config(id INTEGER PRIMARY KEY CHECK(id=1),config_json TEXT NOT NULL);")?;
        let raw: Option<String> = db
            .query_row("SELECT config_json FROM app_config WHERE id=1", [], |r| {
                r.get(0)
            })
            .optional()?;
        match raw {
            Some(raw) => parse_document(raw.as_bytes()),
            None => {
                let source = path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join("yourai.json");
                let value = parse_document(&std::fs::read(&source).map_err(|_| {
                    format!(
                        "No config in {}; import yourai.json beside it first",
                        path.display()
                    )
                })?)?;
                write_document(path, &value)?;
                Ok(value)
            }
        }
    } else {
        parse_document(
            &std::fs::read(path)
                .map_err(|_| format!("Cannot read config at {}", path.display()))?,
        )
    }
}
fn write_document(path: &Path, value: &Value) -> Result<(), Error> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    if is_sqlite(path) {
        let mut db = rusqlite::Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS app_config(id INTEGER PRIMARY KEY CHECK(id=1),config_json TEXT NOT NULL);")?;
        let tx = db.transaction()?;
        tx.execute("INSERT INTO app_config(id,config_json) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET config_json=excluded.config_json", [String::from_utf8(bytes)?])?;
        tx.commit()?;
    } else {
        use std::io::Write;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.write_all(&bytes)?;
        temp.as_file().sync_all()?;
        temp.persist(path).map_err(|e| e.error)?;
        // Rename has committed: a directory-sync diagnostic must not leave the
        // published in-memory value different from the newly written file.
        if let Err(error) = std::fs::File::open(parent).and_then(|f| f.sync_all()) {
            tracing::warn!(%error, "config directory sync failed after commit");
        }
    }
    Ok(())
}

fn sessions() -> PathBuf {
    ".yourai/sessions".into()
}
fn data_session_dir(legacy_base: &Path) -> PathBuf {
    let data = match std::env::var_os("XDG_DATA_HOME") {
        Some(v) if !v.is_empty() && Path::new(&v).is_absolute() => PathBuf::from(v),
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
#[derive(Clone)]
pub struct ResolvedModel {
    pub model: Arc<dyn ModelProvider>,
    pub provider: String,
    pub requests: crate::model::RequestPolicy,
}
impl ResolvedModel {
    pub fn apply_to(&self, config: &mut crate::SessionOptions) {
        config.model_provider = self.provider.clone();
        config.request_policy = self.requests.clone();
    }
}

impl Config {
    /// The one application config maps to startup resources and execution snapshots.
    pub fn apply_to(&self, options: &mut crate::SessionOptions) {
        options.config = self.resolved_paths();
    }
    /// Load the selected JSON or SQLite source and adjacent prompts.json once.
    /// Editing either file outside the process takes effect on the next load.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let mut main = normalize_document(read_document(path)?)?;
        let path = path.canonicalize()?;
        let prompt_path = path.parent().unwrap().join("prompts.json");
        if prompt_path.exists() {
            let prompt: PromptSettings = decode(read_document(&prompt_path)?, "prompt")?;
            main["prompt"] = serde_json::to_value(prompt)?;
        }
        for key in ["selected", "providers"] {
            if main
                .get("models")
                .and_then(|models| models.get(key))
                .is_none()
            {
                return Err(format!("missing field models.{key} in config").into());
            }
        }
        let mut config: Self = decode(main, "config")?;
        config.path = Some(path.clone());
        config.validate_storage_paths()?;
        if let Some(hooks_file) = &config.hooks.file {
            if !config.hooks.events.as_object().is_some_and(Map::is_empty) {
                return Err("choose either hooks.events or hooks.file, not both".into());
            }
            let hooks_path = path.parent().unwrap().join(hooks_file);
            let document = parse_document(&std::fs::read(&hooks_path)?)?;
            if document.as_object().is_none_or(|object| {
                !object.contains_key("hooks") || object.keys().any(|key| key != "hooks")
            }) {
                return Err("hook file must contain a single hooks object".into());
            }
            let hooks = &document["hooks"];
            config.hooks.events = if hooks.get("events").is_some() {
                check_fields(hooks, &["events"], "hook file")?;
                hooks["events"].clone()
            } else {
                // Existing standalone event maps continue to load.
                hooks.clone()
            };
        }
        config.validate()?;
        Ok(config)
    }
    fn hooks_path(&self) -> Option<PathBuf> {
        self.hooks.file.as_ref().map(|file| {
            self.path
                .as_deref()
                .and_then(Path::parent)
                .unwrap_or_else(|| Path::new("."))
                .join(file)
        })
    }
    /// Sources must be separate: otherwise saving one category destroys another.
    fn validate_storage_paths(&self) -> Result<(), Error> {
        let Some(main) = self.path.as_deref() else {
            return Ok(());
        };
        let prompt = main.parent().unwrap().join("prompts.json");
        if storage_path(main) == storage_path(&prompt) {
            return Err("main config and prompts.json must be separate files".into());
        }
        if let Some(hooks) = self.hooks_path() {
            if storage_path(&hooks) == storage_path(main)
                || storage_path(&hooks) == storage_path(&prompt)
            {
                return Err(
                    "hooks.file must be separate from the main config and prompts.json".into(),
                );
            }
        }
        Ok(())
    }
    pub fn session_dir(&self) -> PathBuf {
        let base = self
            .path
            .as_deref()
            .and_then(Path::parent)
            .unwrap_or_else(|| Path::new("."));
        let root = if self.session.directory == sessions() {
            data_session_dir(base)
        } else {
            self.session.directory.clone()
        };
        if root.is_relative() {
            base.join(root)
        } else {
            root
        }
    }
    pub fn resolved_paths(&self) -> Self {
        let mut prompt = self.clone();
        let base = self
            .path
            .as_deref()
            .and_then(Path::parent)
            .unwrap_or_else(|| Path::new("."));
        for path in [
            &mut prompt.prompt.soul_file,
            &mut prompt.prompt.memory_file,
            &mut prompt.prompt.profile_file,
        ]
        .into_iter()
        .flatten()
        {
            if path.is_relative() {
                *path = base.join(&*path);
            }
        }
        prompt
    }
    pub fn validate(&self) -> Result<(), Error> {
        self.validate_storage_paths()?;
        if self.hooks.file.as_deref().is_some_and(is_sqlite) {
            return Err("hooks.file must be a JSON file, not SQLite".into());
        }
        if self.execution.steps == Some(0) {
            return Err("steps must be a positive integer".into());
        }
        self.execution.context_policy.validate()?;
        for provider in self.models.providers.values() {
            validate_provider(provider)?;
        }
        crate::hooks::build_registrations(
            &self.hooks.events,
            crate::hooks::config::HookSource::Project,
        )?;
        for (id, variants) in &self.models.effort_overrides {
            for (variant, effort) in variants {
                let mut probe = self.clone();
                probe.set_entry_effort(
                    id,
                    (!variant.is_empty()).then_some(variant.as_str()),
                    &EffortChoice::Set(effort.clone()),
                )?;
            }
        }
        Ok(())
    }
    /// Persist only ordinary config. The caller replaces shared memory after success.
    pub fn save(&self) -> Result<(), Error> {
        self.validate()?;
        let path = self
            .path
            .as_deref()
            .ok_or("config has no storage path; load it before updating")?;
        // If this is an old combined file, retain its prompt values until explicitly
        // saved to prompts.json; ordinary changes must never silently discard them.
        if !path.parent().unwrap().join("prompts.json").exists() {
            write_document(
                &path.parent().unwrap().join("prompts.json"),
                &self.prompt_document()?,
            )?;
        }
        write_document(path, &self.main_document()?)
    }
    fn prompt_document(&self) -> Result<Value, Error> {
        Ok(serde_json::to_value(&self.prompt)?)
    }
    fn main_document(&self) -> Result<Value, Error> {
        let mut document = serde_json::to_value(self)?;
        document.as_object_mut().unwrap().remove("prompt");
        if self.hooks.file.is_some() {
            document["hooks"].as_object_mut().unwrap().remove("events");
        }
        Ok(document)
    }
    pub fn apply_execution_options(&mut self, execution: yourai_core::execution::ExecutionOptions) {
        self.execution = execution;
    }
    pub fn execution_options(&self) -> yourai_core::execution::ExecutionOptions {
        self.execution.clone()
    }
    pub fn set_entry_effort(
        &mut self,
        id: &str,
        variant: Option<&str>,
        choice: &EffortChoice,
    ) -> Result<(), Error> {
        let (provider, key) = id.split_once('/').ok_or("model must be provider/model")?;
        let provider = self
            .models
            .providers
            .get(provider)
            .ok_or("Unknown provider")?;
        if let Some(variant) = variant {
            let model = provider
                .get("models")
                .and_then(|models| models.get(key))
                .ok_or("Unknown model")?;
            if model
                .get("variants")
                .and_then(|variants| variants.get(variant))
                .is_none()
            {
                return Err("Unknown model variant".into());
            }
        }
        match choice {
            EffortChoice::Config => {
                if let Some(entries) = self.models.effort_overrides.get_mut(id) {
                    entries.remove(variant.unwrap_or(""));
                }
                if self
                    .models
                    .effort_overrides
                    .get(id)
                    .is_some_and(BTreeMap::is_empty)
                {
                    self.models.effort_overrides.remove(id);
                }
            }
            EffortChoice::Set(value) => {
                if !EFFORT_CHOICES
                    .iter()
                    .flatten()
                    .any(|allowed| *allowed == value)
                {
                    return Err(format!("Invalid reasoningEffort: {value}").into());
                }
                self.models
                    .effort_overrides
                    .entry(id.into())
                    .or_default()
                    .insert(variant.unwrap_or("").into(), value.clone());
            }
        }
        Ok(())
    }
    fn selected_provider(&self) -> Result<&Value, Error> {
        let (provider, _) = self
            .models
            .selected
            .split_once('/')
            .ok_or("model must be provider/model")?;
        self.models
            .providers
            .get(provider)
            .ok_or("Unknown provider".into())
    }
    /// Returns the pricing for the currently selected model, if configured.
    pub fn pricing(&self) -> (Option<f64>, Option<f64>) {
        let Some((provider, key)) = self.models.selected.split_once('/') else {
            return (None, None);
        };
        let pricing =
            &self.models.providers.get(provider).unwrap_or(&Value::Null)["models"][key]["pricing"];
        (pricing["input"].as_f64(), pricing["output"].as_f64())
    }
    fn model_entry(&self, id: &str, variant: Option<&str>) -> Result<EntryOptions<'_>, Error> {
        let (provider_id, model_key) = id.split_once('/').ok_or("model must be provider/model")?;
        let provider = self
            .models
            .providers
            .get(provider_id)
            .ok_or("Unknown provider")?;
        let model = provider
            .get("models")
            .and_then(|models| models.get(model_key))
            .ok_or("Unknown model")?;
        let variant_options = match variant {
            Some(name) => Some(
                model
                    .get("variants")
                    .and_then(|variants| variants.get(name))
                    .and_then(Value::as_object)
                    .ok_or("Unknown model variant")?,
            ),
            None => None,
        };
        Ok((object(model, "options"), variant_options))
    }
    /// Variant overrides/defaults precede model overrides/defaults.
    pub fn effective_effort(&self, id: &str, variant: Option<&str>) -> Option<String> {
        let overrides = self.models.effort_overrides.get(id);
        if let Some(variant) = variant {
            if let Some(value) = overrides.and_then(|v| v.get(variant)) {
                return Some(value.clone());
            }
            if let Ok((_, Some(options))) = self.model_entry(id, Some(variant)) {
                if let Some(value) = options.get("reasoningEffort").and_then(Value::as_str) {
                    return Some(value.into());
                }
            }
        }
        overrides.and_then(|v| v.get("")).cloned().or_else(|| {
            self.model_entry(id, None)
                .ok()?
                .0
                .get("reasoningEffort")?
                .as_str()
                .map(str::to_owned)
        })
    }
    pub fn request_policy(&self) -> Result<crate::model::RequestPolicy, Error> {
        let policy: crate::model::RequestPolicy =
            field(&self.selected_provider()?["options"], "requests")?;
        policy.validate()?;
        Ok(policy)
    }
    pub fn model_timeouts(&self) -> Result<(Option<Duration>, Option<Duration>), Error> {
        model_timeouts(&self.selected_provider()?["options"])
    }
    /// Resolve one model/variant with its capacity, output budget and timeouts.
    /// Context maintenance policy is independent of model selection.
    pub fn resolve(&self, variant: Option<&str>) -> Result<ResolvedModel, Error> {
        self.resolve_selection(
            &self.models.selected,
            variant,
            self.effective_effort(&self.models.selected, variant)
                .as_deref(),
        )
    }
    /// Resolve a selection without changing the loaded file configuration.
    pub fn resolve_selection(
        &self,
        id: &str,
        variant: Option<&str>,
        effort: Option<&str>,
    ) -> Result<ResolvedModel, Error> {
        if self.execution.steps == Some(0) {
            return Err("steps must be a positive integer".into());
        }
        let (provider_id, model_key) = id
            .split_once('/')
            .ok_or("model must be provider_id/model_id")?;
        let provider = self
            .models
            .providers
            .get(provider_id)
            .ok_or("Unknown provider in model selection")?;
        let requests: crate::model::RequestPolicy = field(&provider["options"], "requests")?;
        requests.validate()?;
        let defaults = empty_object();
        let model = provider
            .get("models")
            .and_then(|models| models.get(model_key))
            .unwrap_or(&defaults);
        let name = model["id"].as_str().unwrap_or(model_key);
        if name.trim().is_empty() {
            return Err("Model id is empty".into());
        }
        let mut options = Value::Object(object(model, "options").clone());
        if let Some(variant) = variant {
            let selected = model
                .get("variants")
                .and_then(|variants| variants.get(variant))
                .and_then(Value::as_object)
                .ok_or("Unknown model variant")?;
            if selected.get("disabled") == Some(&Value::Bool(true)) {
                return Err("Model variant is disabled".into());
            }
            let mut selected = selected.clone();
            selected.remove("disabled");
            merge(&mut options, Value::Object(selected));
        }
        if let Some(effort) = effort {
            options["reasoningEffort"] = Value::String(effort.into());
        }
        let mut options = options.as_object().unwrap().clone();
        let requested = options
            .remove("maxOutputTokens")
            .map(|v| {
                v.as_u64()
                    .filter(|v| *v > 0 && *v <= u32::MAX as u64)
                    .map(|n| n as u32)
                    .ok_or("maxOutputTokens must be a positive integer")
            })
            .transpose()?;
        let budget = ModelTokenBudget::resolve(field::<ModelLimits>(model, "limit")?, requested)?;
        self.execution.context_policy.validate_for(budget)?;
        let mut chat = ChatOptions::default();
        if let Some(v) = options.remove("temperature") {
            chat.temperature = Some(
                v.as_f64()
                    .filter(|n| n.is_finite() && *n >= 0.0)
                    .ok_or("Invalid temperature")?,
            );
        }
        if let Some(v) = options.remove("topP") {
            chat.top_p = Some(
                v.as_f64()
                    .filter(|n| (0.0..=1.0).contains(n))
                    .ok_or("Invalid topP")?,
            );
        }
        if let Some(v) = options.remove("reasoningEffort") {
            use genai::chat::ReasoningEffort;
            chat.reasoning_effort = Some(match v.as_str() {
                Some("none") => ReasoningEffort::Zero,
                Some("minimal") => ReasoningEffort::Minimal,
                Some("low") => ReasoningEffort::Low,
                Some("medium") => ReasoningEffort::Medium,
                Some("high") => ReasoningEffort::High,
                Some("xhigh") => ReasoningEffort::XHigh,
                Some("max") => ReasoningEffort::Max,
                _ => return Err("Invalid reasoningEffort".into()),
            });
        }
        // Other options are provider-native request body fields. Structural fields remain runtime-owned.
        for reserved in [
            "model",
            "messages",
            "input",
            "system",
            "tools",
            "stream",
            "max_tokens",
            "max_completion_tokens",
            "max_output_tokens",
        ] {
            if options.contains_key(reserved) {
                return Err(format!(
                    "Reserved model option: {reserved}; use maxOutputTokens for output limits"
                )
                .into());
            }
        }
        if !options.is_empty() {
            chat.extra_body = Some(Value::Object(options));
        }
        let mut headers: BTreeMap<String, String> = field(&provider["options"], "headers")?;
        headers.extend(field::<BTreeMap<String, String>>(model, "headers")?);
        let headers: Vec<(String, String)> = headers
            .into_iter()
            .map(|(k, v)| Ok((k, expand(&v)?)))
            .collect::<Result<_, Error>>()?;
        let client = provider_client(provider, provider_id, chat)?;
        let (header_timeout, chunk_timeout) = model_timeouts(&provider["options"])?;
        let defaults = ModelTimeouts::default();
        let model = Arc::new(
            GenaiModel::new(client, name)
                .with_headers(headers.into())
                .with_settings(
                    budget,
                    ModelTimeouts {
                        headers: header_timeout.unwrap_or(defaults.headers),
                        read: chunk_timeout.unwrap_or(defaults.read),
                    },
                )?,
        );
        Ok(ResolvedModel {
            model,
            provider: provider_id.into(),
            requests,
        })
    }

    pub fn resolve_compaction(&self) -> Result<Option<ResolvedModel>, Error> {
        self.models
            .compaction
            .as_ref()
            .map(|id| {
                let mut selected = self.clone();
                selected.models.selected = id.clone();
                selected.resolve(None)
            })
            .transpose()
    }
}
/// Resolves the XDG base directory for configuration.
///
/// Returns `$XDG_CONFIG_HOME` when it is set and absolute, otherwise
/// `$HOME/.config`. Per the XDG Base Directory Specification, a relative or
/// empty `XDG_CONFIG_HOME` is ignored in favour of the home fallback. A
/// missing or non-absolute home with no usable `XDG_CONFIG_HOME` is an error.
fn config_dir(xdg_config_home: Option<&Path>, home: Option<&Path>) -> Result<PathBuf, Error> {
    if let Some(dir) = xdg_config_home.filter(|dir| dir.is_absolute()) {
        return Ok(dir.to_path_buf());
    }
    let home = home
        .filter(|home| home.is_absolute())
        .ok_or("Cannot locate config directory: set XDG_CONFIG_HOME or HOME")?;
    Ok(home.join(".config"))
}

/// Returns the default config file path per the XDG Base Directory
/// Specification: `$XDG_CONFIG_HOME/yourai/yourai.json`, or
/// `$HOME/.config/yourai/yourai.json` when `XDG_CONFIG_HOME` is unset or
/// relative. Override the location with `--config`.
pub fn default_config_path() -> Result<PathBuf, Error> {
    Ok(config_dir(
        std::env::var_os("XDG_CONFIG_HOME")
            .as_deref()
            .map(Path::new),
        std::env::var_os("HOME").as_deref().map(Path::new),
    )?
    .join("yourai")
    .join("yourai.json"))
}

fn merge(base: &mut Value, other: Value) {
    match (base, other) {
        (Value::Object(base), Value::Object(other)) => {
            for (k, v) in other {
                merge(base.entry(k).or_insert(Value::Null), v);
            }
        }
        (base, other) => *base = other,
    }
}
fn expand(value: &str) -> Result<String, Error> {
    if let Some(key) = value
        .strip_prefix("{env:")
        .and_then(|s| s.strip_suffix('}'))
    {
        return std::env::var(key)
            .map_err(|_| format!("Missing environment variable: {key}").into());
    }
    Ok(value.into())
}
fn object<'a>(value: &'a Value, key: &str) -> &'a Map<String, Value> {
    static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
    value
        .get(key)
        .and_then(Value::as_object)
        .unwrap_or_else(|| EMPTY.get_or_init(Map::new))
}
fn field<T: serde::de::DeserializeOwned + Default>(value: &Value, key: &str) -> Result<T, Error> {
    match value.get(key) {
        Some(value) => decode(value.clone(), key),
        None => Ok(T::default()),
    }
}
fn model_timeouts(options: &Value) -> Result<(Option<Duration>, Option<Duration>), Error> {
    let timeout = |key: &str| -> Result<Option<Duration>, Error> {
        field::<Option<u64>>(options, key)?
            .map(|n| {
                if n == 0 {
                    Err(format!("{key} must be positive milliseconds").into())
                } else {
                    Ok(Duration::from_millis(n))
                }
            })
            .transpose()
    };
    Ok((timeout("headerTimeout")?, timeout("chunkTimeout")?))
}
fn check_fields(value: &Value, allowed: &[&str], kind: &str) -> Result<(), Error> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{kind} must be an object"))?;
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("unknown field {key} in {kind}").into());
        }
    }
    Ok(())
}
fn validate_provider(provider: &Value) -> Result<(), Error> {
    if provider.get("api").is_some() && provider.get("npm").is_some() {
        return Err("duplicate api/npm fields".into());
    }
    check_fields(
        provider,
        &["api", "npm", "name", "options", "models"],
        "provider",
    )?;
    for key in ["api", "npm", "name"] {
        field::<Option<String>>(provider, key)?;
    }
    if let Some(options) = provider.get("options") {
        check_fields(
            options,
            &[
                "baseURL",
                "apiKey",
                "timeout",
                "headerTimeout",
                "chunkTimeout",
                "requests",
                "headers",
            ],
            "connection options",
        )?;
        for key in ["baseURL", "apiKey"] {
            field::<Option<String>>(options, key)?;
        }
        field::<Option<u64>>(options, "headerTimeout")?;
        field::<Option<u64>>(options, "chunkTimeout")?;
        field::<crate::model::RequestPolicy>(options, "requests")?.validate()?;
        field::<BTreeMap<String, String>>(options, "headers")?;
    }
    if let Some(models) = provider.get("models") {
        for model in models
            .as_object()
            .ok_or("models must be an object")?
            .values()
        {
            check_fields(
                model,
                &[
                    "id", "name", "limit", "options", "headers", "variants", "pricing",
                ],
                "model",
            )?;
            for key in ["id", "name"] {
                field::<Option<String>>(model, key)?;
            }
            field::<ModelLimits>(model, "limit")?;
            field::<Map<String, Value>>(model, "options")?;
            field::<BTreeMap<String, String>>(model, "headers")?;
            field::<BTreeMap<String, Map<String, Value>>>(model, "variants")?;
            if let Some(pricing) = model.get("pricing") {
                check_fields(pricing, &["input", "output"], "pricing")?;
                field::<Option<f64>>(pricing, "input")?;
                field::<Option<f64>>(pricing, "output")?;
            }
        }
    }
    Ok(())
}
fn provider_client(
    provider: &Value,
    provider_id: &str,
    chat: ChatOptions,
) -> Result<Client, Error> {
    let adapter = match provider
        .get("api")
        .or_else(|| provider.get("npm"))
        .and_then(Value::as_str)
        .unwrap_or(match provider_id {
            "openai" => "openai-responses",
            "anthropic" => "anthropic",
            "google" => "gemini",
            "ollama" => "ollama",
            _ => "openai-compatible",
        }) {
        "openai-compatible" | "@ai-sdk/openai-compatible" => AdapterKind::OpenAI,
        "openai-responses" | "@ai-sdk/openai" | "openai" => AdapterKind::OpenAIResp,
        "@ai-sdk/anthropic" | "anthropic" => AdapterKind::Anthropic,
        "gemini" | "@ai-sdk/google" | "google" => AdapterKind::Gemini,
        "ollama-ai-provider" | "ollama" => AdapterKind::Ollama,
        _ => return Err("Unsupported provider api; see docs/tui.md#模型配置".into()),
    };
    if chat.extra_body.is_some()
        && !matches!(adapter, AdapterKind::OpenAI | AdapterKind::OpenAIResp)
    {
        return Err("Extra model options are currently supported only for OpenAI-compatible/Responses adapters".into());
    }
    let mut builder = Client::builder()
        .with_adapter_kind(adapter)
        .with_chat_options(chat);
    if let Some(key) = provider["options"]["apiKey"].as_str() {
        let key = expand(key)?;
        if key.trim().is_empty() {
            return Err("API key is empty".into());
        }
        builder =
            builder.with_auth_resolver_fn(move |_| Ok(Some(AuthData::from_single(key.clone()))));
    }
    if let Some(base) = provider["options"]["baseURL"].as_str() {
        let mut url = reqwest::Url::parse(&expand(base)?).map_err(|_| "Invalid baseURL")?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("baseURL must be HTTP(S), without credentials, query or fragment".into());
        }
        if !url.path().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }
        let endpoint = Endpoint::from_owned(url.to_string());
        builder =
            builder.with_service_target_resolver_fn(move |mut target: genai::ServiceTarget| {
                target.endpoint = endpoint.clone();
                Ok(target)
            });
    }
    if let Some(timeout) = provider["options"].get("timeout") {
        let duration = if timeout == &Value::Bool(false) {
            None
        } else {
            Some(Duration::from_millis(
                timeout
                    .as_u64()
                    .filter(|n| *n > 0)
                    .ok_or("timeout must be positive milliseconds or false")?,
            ))
        };
        builder = builder.with_web_config(genai::WebConfig {
            timeout: duration,
            ..Default::default()
        });
    }
    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;
    #[test]
    fn current_model_without_a_declared_entry_can_override_and_restore_effort() {
        let mut cfg = config();
        cfg.models.providers.get_mut("gateway").unwrap()["models"] = empty_object();
        let id = cfg.models.selected.clone();
        cfg.set_entry_effort(&id, None, &crate::config::EffortChoice::Set("high".into()))
            .unwrap();
        assert_eq!(cfg.effective_effort(&id, None).as_deref(), Some("high"));
        assert!(cfg.resolve(None).is_ok());
        cfg.set_entry_effort(&id, None, &crate::config::EffortChoice::Config)
            .unwrap();
        assert_eq!(cfg.effective_effort(&id, None), None);
        assert!(cfg.resolve(None).is_ok());
        assert!(cfg
            .set_entry_effort(
                "gateway/unknown",
                Some("missing"),
                &crate::config::EffortChoice::Set("high".into())
            )
            .is_err());
        assert!(!cfg.models.providers["gateway"]["models"]
            .get("unknown")
            .is_some());
    }
    use serde_json::json;
    fn config() -> Config {
        let mut config: Config = serde_json::from_value(json!({
            "model": "gateway/alias/with/slash",
            "provider": {
                "gateway": {"npm": "@ai-sdk/openai-compatible", "options": {"baseURL": "http://localhost:8080/v1", "apiKey": "test-only"},
                    "models": {"alias/with/slash": {"id": "actual-api-model", "limit": {"context": 64000, "output": 16000}, "options": {"maxOutputTokens": 8000}, "variants": {"short": {"maxOutputTokens": 4000, "reasoningEffort": "high"}, "off": {"disabled": true}}}}},
                "unused": {"npm": "@ai-sdk/anthropic", "options": {"apiKey": "{env:YOURAI_UNUSED_PROVIDER_MISSING}"}, "models": {}}
            },
            "yolo": true
        })).unwrap();
        config.prompt.soul_file = Some("./SOUL.md".into());
        config
    }
    #[test]
    fn compaction_selection_is_optional_independent_and_validated() {
        let mut cfg = config();
        assert!(cfg.resolve_compaction().unwrap().is_none());
        cfg.models.compaction = Some("gateway/alias/with/slash".into());
        cfg.models.selected = "unused/main".into();
        let summary = cfg.resolve_compaction().unwrap().unwrap();
        assert_eq!(summary.model.model_iden(), "actual-api-model");
        assert_eq!(summary.model.token_budget().limits().context, Some(64_000));
        assert_eq!(summary.model.token_budget().max_output_tokens(), 8000);
        assert_eq!(summary.provider, "gateway");
        assert_eq!(cfg.models.selected, "unused/main");
        cfg.models.compaction = Some("missing/model".into());
        assert!(cfg.resolve_compaction().is_err());
    }
    #[test]
    fn config_effort_restores_original_values_after_repeated_overrides() {
        use crate::config::EffortChoice;
        let mut cfg = config();
        let id = "gateway/alias/with/slash";
        // Choosing Config without an override must preserve the configured variant.
        cfg.set_entry_effort(id, Some("short"), &EffortChoice::Config)
            .unwrap();
        assert_eq!(
            cfg.effective_effort(id, Some("short")).as_deref(),
            Some("high")
        );
        for keyword in ["low", "medium"] {
            cfg.set_entry_effort(id, Some("short"), &EffortChoice::Set(keyword.into()))
                .unwrap();
        }
        cfg.set_entry_effort(id, Some("short"), &EffortChoice::Config)
            .unwrap();
        assert_eq!(
            cfg.effective_effort(id, Some("short")).as_deref(),
            Some("high")
        );
        // Entries without an original value restore inheritance instead of erasing it.
        cfg.set_entry_effort(id, None, &EffortChoice::Set("low".into()))
            .unwrap();
        cfg.set_entry_effort(id, Some("off"), &EffortChoice::Set("medium".into()))
            .unwrap();
        cfg.set_entry_effort(id, Some("off"), &EffortChoice::Config)
            .unwrap();
        assert_eq!(
            cfg.effective_effort(id, Some("off")).as_deref(),
            Some("low")
        );
        cfg.set_entry_effort(id, None, &EffortChoice::Config)
            .unwrap();
        assert_eq!(cfg.effective_effort(id, None), None);
        assert_eq!(
            cfg.effective_effort(id, Some("short")).as_deref(),
            Some("high")
        );
    }
    #[tokio::test]
    async fn minimal_gateway_config_uses_model_name_and_default_prompt() {
        let config: Config = serde_json::from_value(json!({
            "model":"gateway/glm",
            "provider":{"gateway":{"options":{"baseURL":"http://localhost:8080/v1","apiKey":"test-only"}}}
        })).unwrap();
        assert!(config.prompt.system_prompt.is_none());
        assert_eq!(config.session.directory, PathBuf::from(".yourai/sessions"));
        assert_eq!(config.resolve(None).unwrap().model.model_iden(), "glm");
        let client = provider_client(
            &config.models.providers["gateway"],
            "gateway",
            ChatOptions::default(),
        )
        .unwrap();
        let target = client
            .resolve_service_target(genai::ModelIden::new(AdapterKind::OpenAI, "glm"))
            .await
            .unwrap();
        assert_eq!(target.model.adapter_kind, AdapterKind::OpenAI);
        assert_eq!(target.endpoint.base_url(), "http://localhost:8080/v1/");
        let explicit: Value =
            serde_json::from_value(json!({"api":"anthropic","options":{"apiKey":"test-only"}}))
                .unwrap();
        assert!(provider_client(&explicit, "custom", ChatOptions::default()).is_ok());
        assert!(validate_provider(&json!({"api":"anthropic","npm":"@ai-sdk/openai"})).is_err());
    }
    #[test]
    fn selects_model_and_variant_without_resolving_unused_credentials() {
        let config = config();
        let ResolvedModel { model, .. } = config.resolve(Some("short")).unwrap();
        assert_eq!(model.model_iden(), "actual-api-model");
        // Capacity and the variant's output budget belong to the resolved model.
        assert_eq!(model.token_budget().limits().context, Some(64000));
        assert_eq!(model.token_budget().max_output_tokens(), 4000);
        assert!(config.resolve(Some("off")).is_err());
        assert!(config.resolve(Some("missing")).is_err());
        let mut config = config;
        config.models.selected = "absent/model".into();
        assert!(config.resolve(None).is_err());
    }
    #[test]
    fn validates_output_and_protects_runtime_fields() {
        let mut config = config();
        config.models.providers.get_mut("gateway").unwrap()["models"]["alias/with/slash"]
            ["options"]["maxOutputTokens"] = json!(20000);
        assert!(config.resolve(None).is_err());
        let mut config = self::config();
        config.models.providers.get_mut("gateway").unwrap()["models"]["alias/with/slash"]
            ["options"]["messages"] = json!([]);
        assert!(config.resolve(None).is_err());
    }
    #[tokio::test]
    async fn endpoint_and_protocol_are_explicit() {
        let config = config();
        let client = provider_client(
            &config.models.providers["gateway"],
            "gateway",
            ChatOptions::default(),
        )
        .unwrap();
        let target = client
            .resolve_service_target(genai::ModelIden::new(
                AdapterKind::OpenAI,
                "actual-api-model",
            ))
            .await
            .unwrap();
        assert_eq!(target.endpoint.base_url(), "http://localhost:8080/v1/");
        assert_eq!(target.model.adapter_kind, AdapterKind::OpenAI);
    }
    #[test]
    fn json_example_and_prompt_fields_parse_and_unknown_fields_fail() {
        let config = config();
        assert!(config.security.yolo);
        assert_eq!(config.prompt.soul_file, Some(PathBuf::from("./SOUL.md")));
        let mut example: Value =
            serde_json::from_str(include_str!("../../../yourai.example.json")).unwrap();
        assert!(serde_json::from_value::<Config>(example.clone()).is_ok());
        example["typo"] = json!(true);
        assert!(serde_json::from_value::<Config>(example).is_err());
    }
    #[test]
    fn config_dir_prefers_absolute_xdg_config_home() {
        let dir = config_dir(Some(Path::new("/tmp/xdg")), Some(Path::new("/home/u"))).unwrap();
        assert_eq!(dir, PathBuf::from("/tmp/xdg"));
    }
    #[test]
    fn config_dir_ignores_relative_xdg_config_home() {
        let dir = config_dir(Some(Path::new("rel")), Some(Path::new("/home/u"))).unwrap();
        assert_eq!(dir, PathBuf::from("/home/u/.config"));
    }
    #[test]
    fn config_dir_falls_back_to_home() {
        let dir = config_dir(None, Some(Path::new("/home/u"))).unwrap();
        assert_eq!(dir, PathBuf::from("/home/u/.config"));
    }
    #[test]
    fn config_dir_errors_without_home_or_xdg() {
        assert!(config_dir(None, None).is_err());
    }
    #[test]
    fn effort_override_does_not_materialize_an_undeclared_model_entry() {
        use crate::config::EffortChoice;
        let mut cfg = config();
        let id = "gateway/undeclared";
        cfg.models.selected = id.to_string();
        cfg.resolve(None).unwrap();
        // Restoring defaults requires no synthetic entry. An invalid variant
        // must also leave the model table unchanged.
        cfg.set_entry_effort(id, None, &EffortChoice::Config)
            .unwrap();
        assert!(!cfg.models.providers["gateway"]["models"]
            .get("undeclared")
            .is_some());
        assert!(cfg
            .set_entry_effort(id, Some("missing"), &EffortChoice::Set("high".into()))
            .is_err());
        assert!(!cfg.models.providers["gateway"]["models"]
            .get("undeclared")
            .is_some());
        cfg.set_entry_effort(id, None, &EffortChoice::Set("high".into()))
            .unwrap();
        assert_eq!(cfg.effective_effort(id, None).as_deref(), Some("high"));
        cfg.resolve(None).unwrap();
        // Config restores the absent hint; the in-memory entry stays valid.
        cfg.set_entry_effort(id, None, &EffortChoice::Config)
            .unwrap();
        assert_eq!(cfg.effective_effort(id, None), None);
        cfg.resolve(None).unwrap();
    }
}

fn default_theme() -> String {
    "system".into()
}
pub const EFFORT_CHOICES: [Option<&str>; 8] = [
    None,
    Some("none"),
    Some("minimal"),
    Some("low"),
    Some("medium"),
    Some("high"),
    Some("xhigh"),
    Some("max"),
];

/// Runtime thinking-effort selection for one `/models` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffortChoice {
    /// Keep what the (merged) config options specify; removes any runtime
    /// override previously applied through the picker.
    Config,
    /// Set the effort keyword for this entry.
    Set(String),
}
