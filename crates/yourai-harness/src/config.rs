pub use yourai_core::{
    compactor::ContextPolicy,
    execution::{ExecutionOptions, ImagePolicy},
    model::{ModelLimits, ModelTimeouts},
};
pub type Error = Box<dyn std::error::Error + Send + Sync>;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConnectionTimeout {
    Milliseconds(u64),
    Disabled(bool),
}

/// Handler 配置（discriminated union on `type`）。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum HandlerConfig {
    #[serde(rename = "command")]
    Command {
        command: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        shell: Option<HookShell>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none", rename = "if")]
        if_condition: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        once: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none", rename = "async")]
        is_async: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none", rename = "asyncRewake")]
        async_rewake: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none", rename = "statusMessage")]
        status_message: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", rename = "failurePolicy")]
        failure_policy: Option<FailurePolicyConfig>,
    },
    #[serde(rename = "http")]
    Http {
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        headers: Option<HashMap<String, String>>,
        #[serde(skip_serializing_if = "Option::is_none", rename = "allowedEnvVars")]
        allowed_env_vars: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none", rename = "if")]
        if_condition: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        once: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none", rename = "statusMessage")]
        status_message: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", rename = "failurePolicy")]
        failure_policy: Option<FailurePolicyConfig>,
    },
    #[serde(rename = "prompt")]
    Prompt {
        prompt: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", rename = "if")]
        if_condition: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        once: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none", rename = "statusMessage")]
        status_message: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", rename = "failurePolicy")]
        failure_policy: Option<FailurePolicyConfig>,
    },
    #[serde(rename = "agent")]
    Agent {
        prompt: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", rename = "if")]
        if_condition: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        once: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none", rename = "statusMessage")]
        status_message: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", rename = "failurePolicy")]
        failure_policy: Option<FailurePolicyConfig>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FailurePolicyConfig {
    Open,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HookShell {
    Bash,
    Powershell,
}

/// Application settings. Share with Arc; setters acquire the internal lock.
/// ```compile_fail
/// let config = yourai_harness::config::Config::default();
/// let duplicate = config.clone();
/// ```
/// Setters change configuration data; a running model/security change goes through
/// the session operation that prepares and applies its resources.
/// ```compile_fail
/// let config = yourai_harness::config::Config::default();
/// config.snapshot();
/// ```
#[derive(Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Config {
    data: parking_lot::Mutex<ConfigData>,
    #[serde(skip)]
    path: Option<PathBuf>,
}

type Selector<'a, T> =
    std::sync::Arc<dyn for<'b> Fn(&'b mut ConfigData) -> Option<&'b mut T> + Send + Sync + 'a>;
/// A borrowed path into the one Config. It owns no configuration data or lock.
/// Each getter/setter locks only for that call. Missing indexed entries return errors.
pub struct ConfigSection<'a, T: 'static> {
    config: &'a Config,
    select: Selector<'a, T>,
}
impl<'a, T: 'static> ConfigSection<'a, T> {
    fn access<R>(&self, action: impl FnOnce(&mut T) -> R) -> Result<R, Error> {
        let mut data = self.config.data.lock();
        let value = (self.select)(&mut data).ok_or("configuration entry no longer exists")?;
        Ok(action(value))
    }
    fn root<R>(&self, action: impl FnOnce(&mut T) -> R) -> R {
        self.access(action)
            .expect("root configuration group always exists")
    }
    fn child<U: 'static>(
        &self,
        select: impl for<'b> Fn(&'b mut T) -> Option<&'b mut U> + Send + Sync + 'a,
    ) -> ConfigSection<'a, U> {
        let parent = std::sync::Arc::clone(&self.select);
        ConfigSection {
            config: self.config,
            select: std::sync::Arc::new(move |data| select(parent(data)?)),
        }
    }
}
impl<T: Serialize + 'static> Serialize for ConfigSection<'_, T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.access(|value| value.serialize(serializer))
            .map_err(serde::ser::Error::custom)?
    }
}
macro_rules! view_fields {
    ($mode:ident $name:ident { $($field:ident: $ty:ty => $setter:ident [$($kind:tt)*],)* }) => {
        impl<'a> ConfigSection<'a, $name> { $(
            view_fields!(@get $mode $field: $ty [$($kind)*]);
            pub fn $setter(&self, value: $ty) -> Result<(), Error> {
                self.access(|node| node.$setter(value))
            }
        )* }
    };
    (@get $mode:ident limit: $ty:ty [value]) => {
        pub fn limit(&self) -> ConfigSection<'a, ModelLimits> { self.child(|node| Some(node.limit_mut())) }
    };
    (@get $mode:ident $field:ident: $ty:ty [mut $edit:ident]) => {
        pub fn $field(&self) -> ConfigSection<'a, $ty> { self.child(|node| Some(node.$edit())) }
    };
    (@get root $field:ident: $ty:ty [map $edit:ident $child:ty]) => {
        pub fn $field(&self) -> BTreeMap<String, ConfigSection<'a, $child>> {
            self.root(|node| node.$field().keys().cloned().collect::<Vec<_>>()).into_iter().map(|id| {
                let key = id.clone(); (id, self.child(move |node| node.$edit(&key)))
            }).collect()
        }
    };
    (@get nested $field:ident: $ty:ty [map $edit:ident $child:ty]) => {
        pub fn $field(&self) -> Result<BTreeMap<String, ConfigSection<'a, $child>>, Error> {
            Ok(self.access(|node| node.$field().keys().cloned().collect::<Vec<_>>())?.into_iter().map(|id| {
                let key = id.clone(); (id, self.child(move |node| node.$edit(&key)))
            }).collect())
        }
    };
    (@get root $field:ident: $ty:ty [value]) => {
        pub fn $field(&self) -> $ty { self.root(|node| node.$field()) }
    };
    (@get nested $field:ident: $ty:ty [value]) => {
        pub fn $field(&self) -> Result<$ty, Error> { self.access(|node| node.$field()) }
    };
    (@get root $field:ident: $ty:ty [$kind:ident]) => {
        pub fn $field(&self) -> $ty { self.root(|node| node.$field().clone()) }
    };
    (@get nested $field:ident: $ty:ty [$kind:ident]) => {
        pub fn $field(&self) -> Result<$ty, Error> { self.access(|node| node.$field().clone()) }
    };
}
// JSON structs describe data; shared access is generated from the same fields.
macro_rules! config_fields {
    ($( $(#[$attr:meta])* $vis:vis struct $name:ident
        $(as $group:ident => $get:ident, $set:ident)? {
        $( $(#[$field_attr:meta])* $field:ident: $ty:ty => $setter:ident [$($kind:tt)*], )*
    })+) => {
        yourai_core::settings_fields! { $( $(#[$attr])* $vis struct $name {
            $( $(#[$field_attr])* $field: $ty => $setter [$($kind)*], )*
        })+ }
        $(config_fields!(@shared $name $(, $group, $get, $set)?; $($field: $ty => $setter [$($kind)*],)*);)+
    };
    (@shared $name:ident, $group:ident, $get:ident, $set:ident; $($field:ident: $ty:ty => $setter:ident [$($kind:tt)*],)*) => {
        view_fields! { root $name { $($field: $ty => $setter [$($kind)*],)* } }
        impl Config {
            pub fn $get(&self) -> ConfigSection<'_, $name> {
                ConfigSection { config: self, select: std::sync::Arc::new(|data| Some(&mut data.$group)) }
            }
            pub fn $set(&self, value: $name) { self.data.lock().$set(value); }
            $(pub fn $setter(&self, value: $ty) {
                self.$get().$setter(value).expect("root configuration group always exists");
            })*
        }
    };
    (@shared ConfigData; $($field:ident: $ty:ty => $setter:ident [$($kind:tt)*],)*) => {};
    (@shared $name:ident; $($field:ident: $ty:ty => $setter:ident [$($kind:tt)*],)*) => {
        view_fields! { nested $name { $($field: $ty => $setter [$($kind)*],)* } }
    };
}
config_fields! { @shared ExecutionOptions, execution, execution, set_execution;
    skill_ids: Vec<String> => set_skill_ids [ref],
    memory_search_limit: usize => set_memory_search_limit [value],
    context_policy: ContextPolicy => set_context_policy [mut context_policy_mut],
    steps: Option<u32> => set_steps [value],
    model_timeout: Option<Duration> => set_model_timeout [value],
    allow_background_wake: bool => set_allow_background_wake [value],
    max_followups: usize => set_max_followups [value],
    instruction_paths: Vec<PathBuf> => set_instruction_paths [ref],
    workspace_enabled: bool => set_workspace_enabled [value],
    memory_max_chars: usize => set_memory_max_chars [value],
    attachment_image: ImagePolicy => set_attachment_image [mut attachment_image_mut],
    attachment_text_max_chars: usize => set_attachment_text_max_chars [value],
    max_model_retries: u32 => set_max_model_retries [value],
    max_overflow_compactions: u32 => set_max_overflow_compactions [value],
    max_stop_continuations: u32 => set_max_stop_continuations [value],
    max_permission_rechecks: u32 => set_max_permission_rechecks [value],
    retry_delay: Duration => set_retry_delay [value],
    retry_max_delay: Duration => set_retry_max_delay [value],
    operation_timeout: Option<Duration> => set_operation_timeout [value],
    tool_timeout: Option<Duration> => set_tool_timeout [value],
    approval_timeout: Option<Duration> => set_approval_timeout [value],
    hook_timeout: Option<Duration> => set_hook_timeout [value],
    cleanup_timeout: Option<Duration> => set_cleanup_timeout [value],
    tool_cleanup_timeout: Duration => set_tool_cleanup_timeout [value],
}
view_fields! { root ContextPolicy {
    safety_margin: u64 => set_safety_margin [value],
    advance_tokens: Option<u64> => set_advance_tokens [value],
    keep_recent_tokens: u64 => set_keep_recent_tokens [value],
    summary_tokens: u64 => set_summary_tokens [value],
    summary_min_savings: u64 => set_summary_min_savings [value],
} }
view_fields! { root ImagePolicy {
    auto_resize: bool => set_auto_resize [value],
    max_width: u32 => set_max_width [value],
    max_height: u32 => set_max_height [value],
    max_base64_bytes: usize => set_max_base64_bytes [value],
} }
view_fields! { nested ModelLimits {
    context: Option<u64> => set_context [value],
    input: Option<u64> => set_input [value],
    output: Option<u32> => set_output [value],
} }
config_fields! {
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelSettings as models => models, set_models {
    #[serde(alias = "selected")]
    model: String => set_model [ref],
    #[serde(skip_serializing_if = "Option::is_none")]
    compaction: Option<String> => set_compaction [ref],
    #[serde(skip_serializing_if = "Option::is_none")]
    variant: Option<String> => set_variant [ref],
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    effort_overrides: BTreeMap<String, BTreeMap<String, String>> => set_effort_overrides [ref],
    providers: BTreeMap<String, ProviderSettings> => set_providers [map provider_mut ProviderSettings],
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderSettings {
    #[serde(alias = "npm")]
    api: Option<String> => set_api [ref],
    name: Option<String> => set_name [ref],
    options: ConnectionSettings => set_options [mut options_mut],
    models: BTreeMap<String, ModelDefinition> => set_models [map model_mut ModelDefinition],
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct ConnectionSettings {
    #[serde(rename = "baseURL")]
    base_url: Option<String> => set_base_url [ref],
    api_key: Option<String> => set_api_key [ref],
    timeout: Option<ConnectionTimeout> => set_timeout [ref],
    header_timeout: Option<u64> => set_header_timeout [value],
    chunk_timeout: Option<u64> => set_chunk_timeout [value],
    requests: RequestPolicy => set_requests [mut requests_mut],
    headers: BTreeMap<String, String> => set_headers [ref],
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelDefinition {
    id: Option<String> => set_id [ref],
    name: Option<String> => set_name [ref],
    limit: ModelLimits => set_limit [value],
    options: Map<String, Value> => set_options [ref],
    headers: BTreeMap<String, String> => set_headers [ref],
    variants: BTreeMap<String, VariantSettings> => set_variants [map variant_mut VariantSettings],
    pricing: ModelPricing => set_pricing [mut pricing_mut],
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct VariantSettings {
    #[serde(default)]
    disabled: bool => set_disabled [value],
    #[serde(flatten)]
    options: Map<String, Value> => set_options [ref],
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelPricing {
    input: Option<f64> => set_input [value],
    output: Option<f64> => set_output [value],
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RequestPolicy {
    /// Optional known provider quota. Requests are paced evenly, without a burst allowance.
    rpm: Option<u32> => set_rpm [value],
    /// Opt-in shared fallback for HTTP 429 without a server delay; zero disables it.
    cooldown_seconds: u64 => set_cooldown_seconds [value],
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ConfigData {
    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    schema: Option<String> => set_schema [ref],
    models: ModelSettings => set_models [mut models_mut],
    execution: ExecutionOptions => set_execution [mut execution_mut],
    security: SecuritySettings => set_security [mut security_mut],
    ui: UiSettings => set_ui [mut ui_mut],
    session: SessionSettings => set_session [mut session_mut],
    prompt: PromptSettings => set_prompt [mut prompt_mut],
    hooks: HookSettings => set_hooks [mut hooks_mut],
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecuritySettings as security => security, set_security {
    trusted_shell: bool => set_trusted_shell [value],
    yolo: bool => set_yolo [value],
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiSettings as ui => ui, set_ui {
    theme: String => set_theme [ref],
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionSettings as session => session, set_session {
    directory: PathBuf => set_directory [ref],
    extensions: bool => set_extensions [value],
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PromptSettings as prompt => prompt, set_prompt {
    system_prompt: Option<String> => set_system_prompt [ref],
    soul_file: Option<PathBuf> => set_soul_file [ref],
    memory_file: Option<PathBuf> => set_memory_file [ref],
    profile_file: Option<PathBuf> => set_profile_file [ref],
    append_system_prompt: Option<String> => set_append_system_prompt [ref],
    memory_file_max_chars: usize => set_memory_file_max_chars [value],
    profile_file_max_chars: usize => set_profile_file_max_chars [value],
    provider_max_chars: usize => set_provider_max_chars [value],
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HookSettings as hooks => hooks, set_hooks {
    events: Value => set_events [ref],
}
}
impl Default for UiSettings {
    fn default() -> Self {
        Self {
            theme: "system".into(),
        }
    }
}
impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            directory: ".yourai/sessions".into(),
            extensions: false,
        }
    }
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
impl Default for HookSettings {
    fn default() -> Self {
        Self {
            events: empty_object(),
        }
    }
}
fn empty_object() -> Value {
    Value::Object(Map::new())
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
    (
        "execution",
        "skill_ids memory_search_limit context steps model_timeout allow_background_wake max_followups instruction_paths workspace_enabled memory_max_chars attachment_image attachment_text_max_chars max_model_retries max_overflow_compactions max_stop_continuations max_permission_rechecks retry_delay retry_max_delay operation_timeout tool_timeout approval_timeout hook_timeout cleanup_timeout tool_cleanup_timeout max_model_calls:steps",
    ),
    (
        "prompt",
        "system_prompt soul_file memory_file profile_file append_system_prompt memory_file_max_chars profile_file_max_chars provider_max_chars",
    ),
    (
        "models",
        "model compaction_model:compaction variant effort_overrides provider:providers",
    ),
    ("ui", "theme"),
    ("security", "trusted_shell yolo"),
    ("session", "session_dir:directory extensions"),
    ("hooks", "hooks_file:file"),
];

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

fn read_document(path: &Path) -> Result<Value, Error> {
    let bytes =
        std::fs::read(path).map_err(|_| format!("Cannot read config at {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| format!("Invalid JSON at line {}, column {}", e.line(), e.column()).into())
}
fn document_bytes(value: &impl Serialize) -> Result<Vec<u8>, Error> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}
fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    use std::io::Write;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    if let Err(error) = std::fs::File::open(parent).and_then(|f| f.sync_all()) {
        tracing::warn!(%error, "config directory sync failed after commit");
    }
    Ok(())
}

impl Config {
    /// Write the complete configuration to its one JSON file.
    pub fn save(&self) -> Result<(), Error> {
        self.save_data(&self.data.lock())
    }
    pub(crate) fn save_data(&self, data: &ConfigData) -> Result<(), Error> {
        let path = self
            .path
            .as_deref()
            .ok_or("config has no file path; initialize it before saving")?;
        write_bytes(path, &document_bytes(data)?)
    }
    pub(crate) fn write(&self) -> parking_lot::MutexGuard<'_, ConfigData> {
        self.data.lock()
    }
    // Resource preparation takes coherent input data, never another writable Config.
    pub(crate) fn values(&self) -> ConfigData {
        self.data.lock().clone()
    }
    pub fn set_provider(&self, id: String, provider: ProviderSettings) {
        self.data.lock().models.providers.insert(id, provider);
    }
    pub fn schema(&self) -> Option<String> {
        self.data.lock().schema().clone()
    }
    pub fn set_schema(&self, value: Option<String>) {
        self.data.lock().set_schema(value);
    }
    pub fn theme(&self) -> String {
        self.ui().theme()
    }
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Initialize from JSON. Old split files are read only for migration;
    /// once saved, the main document contains all settings and takes precedence.
    pub fn init(path: &Path) -> Result<Self, Error> {
        let mut document = normalize_document(read_document(path)?)?;
        let path = path.canonicalize()?;
        let base = path.parent().unwrap();
        let legacy_prompt = base.join("prompts.json");
        if document.get("prompt").is_none() && legacy_prompt != path && legacy_prompt.exists() {
            document["prompt"] = read_document(&legacy_prompt)?;
        }
        if let Some(hooks) = document.get_mut("hooks").and_then(Value::as_object_mut) {
            if let Some(file) = hooks.remove("file").filter(|file| !file.is_null()) {
                if hooks
                    .get("events")
                    .is_some_and(|events| !events.as_object().is_some_and(Map::is_empty))
                {
                    return Err("choose either hooks.events or legacy hooks.file, not both".into());
                }
                let file: PathBuf = decode(file, "legacy hooks.file")?;
                let source = base.join(file);
                if source.canonicalize()? == path {
                    return Err("legacy hooks.file must differ from the main config".into());
                }
                let mut wrapper: BTreeMap<String, Value> =
                    decode(read_document(&source)?, "legacy hook file")?;
                if wrapper.len() != 1 || !wrapper.contains_key("hooks") {
                    return Err("legacy hook file must contain only hooks".into());
                }
                let events = wrapper.remove("hooks").unwrap();
                hooks.insert(
                    "events".into(),
                    match events.get("events") {
                        Some(value) if events.as_object().is_some_and(|map| map.len() == 1) => {
                            value.clone()
                        }
                        Some(_) => {
                            return Err("legacy hook wrapper must contain only events".into());
                        }
                        None => events,
                    },
                );
            }
        }
        let data: ConfigData = decode(document, "config")?;
        Ok(Self {
            data: parking_lot::Mutex::new(data),
            path: Some(path),
        })
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
    .join("yourai/yourai.json"))
}
#[cfg(test)]
mod tests;

impl ModelDefinition {
    pub fn limit_mut(&mut self) -> &mut ModelLimits {
        &mut self.limit
    }
}

impl<'a> ConfigSection<'a, ModelSettings> {
    pub fn choices(&self) -> Vec<crate::model::ModelChoice> {
        self.root(|models| models.choices())
    }
    pub fn provider(&self, id: &str) -> Result<ConfigSection<'a, ProviderSettings>, Error> {
        let key = id.to_owned();
        let node = self.child(move |models| models.provider_mut(&key));
        node.access(|_| ())?;
        Ok(node)
    }
    pub fn set_provider(&self, id: String, provider: ProviderSettings) -> Result<(), Error> {
        self.access(|models| {
            models.providers.insert(id, provider);
        })
    }
    pub fn select_model(
        &self,
        id: &str,
        variant: Option<String>,
        effort: Option<&crate::model::EffortChoice>,
    ) -> Result<ModelSettings, Error> {
        self.root(|models| models.select_model(id, variant, effort))
    }
    pub fn with_effort(
        &self,
        id: &str,
        variant: Option<&str>,
        effort: &crate::model::EffortChoice,
    ) -> Result<ModelSettings, Error> {
        self.root(|models| models.with_effort(id, variant, effort))
    }
    pub fn resolve(&self) -> Result<crate::model::ResolvedModel, Error> {
        self.root(|models| models.clone()).resolve()
    }
    pub fn resolve_compaction(&self) -> Result<Option<crate::model::ResolvedModel>, Error> {
        self.root(|models| models.clone()).resolve_compaction()
    }
    pub fn effective_effort(&self, id: impl AsRef<str>, variant: Option<&str>) -> Option<String> {
        self.root(|models| models.effective_effort(id.as_ref(), variant))
    }
    pub fn pricing(&self) -> (Option<f64>, Option<f64>) {
        self.root(|models| models.pricing())
    }
}
impl<'a> ConfigSection<'a, ProviderSettings> {
    pub fn model(&self, id: &str) -> Result<ConfigSection<'a, ModelDefinition>, Error> {
        let key = id.to_owned();
        let node = self.child(move |provider| provider.model_mut(&key));
        node.access(|_| ())?;
        Ok(node)
    }
    pub fn set_model(&self, id: String, model: ModelDefinition) -> Result<(), Error> {
        self.access(|provider| {
            provider.models.insert(id, model);
        })
    }
}
impl<'a> ConfigSection<'a, ModelDefinition> {
    pub fn variant(&self, id: &str) -> Result<ConfigSection<'a, VariantSettings>, Error> {
        let key = id.to_owned();
        let node = self.child(move |model| model.variant_mut(&key));
        node.access(|_| ())?;
        Ok(node)
    }
    pub fn set_variant(&self, id: String, variant: VariantSettings) -> Result<(), Error> {
        self.access(|model| {
            model.variants.insert(id, variant);
        })
    }
    pub fn set_option(&self, key: String, value: Value) -> Result<(), Error> {
        self.access(|model| {
            model.options.insert(key, value);
        })
    }
}
impl ConfigSection<'_, VariantSettings> {
    pub fn set_option(&self, key: String, value: Value) -> Result<(), Error> {
        self.access(|variant| {
            variant.options.insert(key, value);
        })
    }
}
impl ConfigSection<'_, ConnectionSettings> {
    pub fn set_header(&self, key: String, value: String) -> Result<(), Error> {
        self.access(|connection| {
            connection.headers.insert(key, value);
        })
    }
}
impl ConfigSection<'_, ExecutionOptions> {
    /// Prepare per-turn execution parameters from the current shared settings.
    pub fn resolve(&self, limits: &yourai_core::turn::TurnLimits) -> ExecutionOptions {
        self.root(|execution| execution.resolve(limits))
    }
}
impl ConfigSection<'_, ContextPolicy> {
    pub fn input_budget(&self, budget: yourai_core::model::ModelTokenBudget) -> Option<u64> {
        self.root(|policy| policy.input_budget(budget))
    }
    pub fn validate_for(
        &self,
        budget: yourai_core::model::ModelTokenBudget,
    ) -> Result<(), yourai_core::YourAiError> {
        self.root(|policy| policy.validate_for(budget))
    }
    pub fn maintenance_threshold(
        &self,
        budget: yourai_core::model::ModelTokenBudget,
    ) -> Option<u64> {
        self.root(|policy| policy.maintenance_threshold(budget))
    }
}
