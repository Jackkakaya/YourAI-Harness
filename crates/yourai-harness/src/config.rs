use crate::model::configured::model_timeouts;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use yourai_core::prelude::{ModelLimits, ModelProvider};
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
    pub providers: BTreeMap<String, ProviderSettings>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderSettings {
    #[serde(alias = "npm")]
    pub api: Option<String>,
    pub name: Option<String>,
    pub options: ConnectionSettings,
    pub models: BTreeMap<String, ModelDefinition>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct ConnectionSettings {
    #[serde(rename = "baseURL")]
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub timeout: Option<ConnectionTimeout>,
    pub header_timeout: Option<u64>,
    pub chunk_timeout: Option<u64>,
    pub requests: crate::model::RequestPolicy,
    pub headers: BTreeMap<String, String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConnectionTimeout {
    Milliseconds(u64),
    Disabled(bool),
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelDefinition {
    pub id: Option<String>,
    pub name: Option<String>,
    pub limit: ModelLimits,
    pub options: Map<String, Value>,
    pub headers: BTreeMap<String, String>,
    pub variants: BTreeMap<String, VariantSettings>,
    pub pricing: ModelPricing,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct VariantSettings {
    #[serde(default)]
    pub disabled: bool,
    #[serde(flatten)]
    pub options: Map<String, Value>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelPricing {
    pub input: Option<f64>,
    pub output: Option<f64>,
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
            theme: "system".into(),
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

// Derive the grouped JSON format; legacy keys are normalized only at the boundary.
#[derive(Default, Deserialize)]
#[serde(remote = "Config", default, deny_unknown_fields)]
struct ConfigDocument {
    #[serde(rename = "$schema")]
    schema: Option<String>,
    models: ModelSettings,
    execution: yourai_core::execution::ExecutionOptions,
    security: SecuritySettings,
    ui: UiSettings,
    session: SessionSettings,
    prompt: PromptSettings,
    hooks: HookSettings,
    #[serde(skip)]
    path: Option<PathBuf>,
}
impl<'de> Deserialize<'de> for Config {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = normalize_document(Value::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)?;
        ConfigDocument::deserialize(value).map_err(serde::de::Error::custom)
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
    for (category, fields) in LEGACY_FIELDS {
        for field in fields.split_whitespace() {
            let (old, key) = field.split_once(':').unwrap_or((field, field));
            if let Some(value) = object.remove(old) {
                let group = object
                    .entry(*category)
                    .or_insert_with(empty_object)
                    .as_object_mut()
                    .ok_or_else(|| format!("{category} must be an object"))?;
                if group.insert(key.into(), value).is_some() {
                    return Err(format!("duplicate config: {old} and {category}.{key}").into());
                }
            }
        }
    }
    Ok(value)
}
const LEGACY_FIELDS: &[(&str, &str)] = &[
    ("execution", "skill_ids memory_search_limit context steps model_timeout allow_background_wake max_followups instruction_paths workspace_enabled memory_max_chars attachment_image attachment_text_max_chars max_model_retries max_overflow_compactions max_stop_continuations max_permission_rechecks retry_delay retry_max_delay operation_timeout tool_timeout approval_timeout hook_timeout cleanup_timeout tool_cleanup_timeout max_model_calls:steps"),
    ("prompt", "system_prompt soul_file memory_file profile_file append_system_prompt memory_file_max_chars profile_file_max_chars provider_max_chars"),
    ("models", "model:selected compaction_model:compaction variant effort_overrides provider:providers"),
    ("ui", "theme"),
    ("security", "trusted_shell yolo"),
    ("session", "session_dir:directory extensions"),
    ("hooks", "hooks_file:file"),
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

pub(crate) fn decode<T: serde::de::DeserializeOwned>(value: Value, kind: &str) -> Result<T, Error> {
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
    parse_document(
        &std::fs::read(path).map_err(|_| format!("Cannot read config at {}", path.display()))?,
    )
}
fn write_document(path: &Path, value: &Value) -> Result<(), Error> {
    use std::io::Write;
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(&bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    if let Err(error) = std::fs::File::open(parent).and_then(|f| f.sync_all()) {
        tracing::warn!(%error, "config directory sync failed after commit");
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
    /// Load the JSON configuration and adjacent prompts.json once.
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
        if self.execution.steps == Some(0) {
            return Err("steps must be a positive integer".into());
        }
        self.execution.context_policy.validate()?;
        for provider in self.models.providers.values() {
            provider.options.requests.validate()?;
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
            let model = provider.models.get(key).ok_or("Unknown model")?;
            if !model.variants.contains_key(variant) {
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
    fn selected_provider(&self) -> Result<&ProviderSettings, Error> {
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
        let Some(model) = self
            .models
            .providers
            .get(provider)
            .and_then(|p| p.models.get(key))
        else {
            return (None, None);
        };
        (model.pricing.input, model.pricing.output)
    }
    fn model_entry(&self, id: &str, variant: Option<&str>) -> Result<EntryOptions<'_>, Error> {
        let (provider_id, model_key) = id.split_once('/').ok_or("model must be provider/model")?;
        let provider = self
            .models
            .providers
            .get(provider_id)
            .ok_or("Unknown provider")?;
        let model = provider.models.get(model_key).ok_or("Unknown model")?;
        let variant_options = match variant {
            Some(name) => Some(
                &model
                    .variants
                    .get(name)
                    .ok_or("Unknown model variant")?
                    .options,
            ),
            None => None,
        };
        Ok((&model.options, variant_options))
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
        let policy = self.selected_provider()?.options.requests.clone();
        policy.validate()?;
        Ok(policy)
    }
    pub fn model_timeouts(&self) -> Result<(Option<Duration>, Option<Duration>), Error> {
        model_timeouts(&self.selected_provider()?.options)
    }
}
/// Resolve the standard XDG config directory.
fn config_dir(xdg_config_home: Option<&Path>, home: Option<&Path>) -> Result<PathBuf, Error> {
    if let Some(dir) = xdg_config_home.filter(|dir| dir.is_absolute()) {
        return Ok(dir.to_path_buf());
    }
    let home = home
        .filter(|home| home.is_absolute())
        .ok_or("Cannot locate config directory: set XDG_CONFIG_HOME or HOME")?;
    Ok(home.join(".config"))
}

/// Default location; --config overrides this path.
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

#[cfg(test)]
mod tests;

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
