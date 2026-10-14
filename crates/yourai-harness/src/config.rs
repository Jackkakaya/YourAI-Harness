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

yourai_core::settings_fields! {
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct ProviderConfig {
    #[serde(alias = "npm")]
    api: Option<String> => set_api [ref],
    name: Option<String> => set_name [ref],
    #[serde(rename = "baseURL")]
    base_url: Option<String> => set_base_url [ref],
    api_key: Option<String> => set_api_key [ref],
    timeout: Option<ConnectionTimeout> => set_timeout [ref],
    header_timeout: Option<u64> => set_header_timeout [value],
    chunk_timeout: Option<u64> => set_chunk_timeout [value],
    rpm: Option<u32> => set_rpm [value],
    cooldown_seconds: u64 => set_cooldown_seconds [value],
    headers: BTreeMap<String, String> => set_headers [ref],
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelConfig {
    provider: String => set_provider [ref],
    id: Option<String> => set_id [ref],
    name: Option<String> => set_name [ref],
    limit: ModelLimits => set_limit [value],
    input_price: Option<f64> => set_input_price [value],
    output_price: Option<f64> => set_output_price [value],
    options: Map<String, Value> => set_options [ref],
    headers: BTreeMap<String, String> => set_headers [ref],
    variants: BTreeMap<String, Map<String, Value>> => set_variants [ref],
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PromptSettings {
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
pub struct HookSettings {
    events: Value => set_events [ref],
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
/// ```compile_fail
/// let config = yourai_harness::config::Config::default();
/// config.clone();
/// ```
/// One JSON document, shared through Arc. All mutations use internally locked setters.
/// ```compile_fail
/// let config = yourai_harness::config::Config::default();
/// config.snapshot();
/// ```
#[derive(Serialize)]
#[serde(transparent)]
pub struct Config {
    document: parking_lot::Mutex<Value>,
    #[serde(skip)]
    path: Option<PathBuf>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            document: parking_lot::Mutex::new(default_document()),
            path: None,
        }
    }
}
impl<'de> Deserialize<'de> for Config {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::from_document(Value::deserialize(d)?, None).map_err(serde::de::Error::custom)
    }
}
fn default_document() -> Value {
    serde_json::json!({
        "model":"", "variant":null, "compaction_model":null, "effort_overrides":{},
        "providers":{}, "models":{}, "execution":ExecutionOptions::default(),
        "theme":"system", "yolo":false, "trusted_shell":false,
        "directory":".yourai/sessions", "extensions":false,
        "prompt":PromptSettings::default(), "hooks":HookSettings::default()
    })
}
// Only validated JSON is stored; callers never receive the document or a lock.
pub(crate) fn read<T: serde::de::DeserializeOwned>(data: &Value, key: &str) -> T {
    serde_json::from_value(data[key].clone()).expect("validated configuration field")
}
pub(crate) fn put(data: &mut Value, key: &str, value: impl Serialize) {
    data[key] = serde_json::to_value(value).expect("configuration value is serializable");
}
macro_rules! fields {
    ($path:expr; $($get:ident, $set:ident: $ty:ty => $key:literal;)*) => {
        impl Config { $(
            pub fn $get(&self) -> $ty {
                let data = self.document.lock();
                let mut node = &*data;
                for key in $path { node = &node[key]; }
                read(node, $key)
            }
            pub fn $set(&self, value: $ty) {
                let mut data = self.document.lock();
                let mut node = &mut *data;
                for key in $path { node = &mut node[key]; }
                put(node, $key, value);
            }
        )* }
    };
}
fields! { [] as [&str; 0];
    model, set_model: String => "model";
    variant, set_variant: Option<String> => "variant";
    compaction_model, set_compaction_model: Option<String> => "compaction_model";
    theme, set_theme: String => "theme";
    yolo, set_yolo: bool => "yolo";
    trusted_shell, set_trusted_shell: bool => "trusted_shell";
    directory, set_directory: PathBuf => "directory";
    extensions, set_extensions: bool => "extensions";
    schema, set_schema: Option<String> => "$schema";
    execution_options, set_execution: ExecutionOptions => "execution";
    prompt, set_prompt: PromptSettings => "prompt";
    hooks, set_hooks: HookSettings => "hooks";
}
fields! { ["execution"];
    skill_ids, set_skill_ids: Vec<String> => "skill_ids";
    memory_search_limit, set_memory_search_limit: usize => "memory_search_limit";
    context_policy, set_context_policy: ContextPolicy => "context";
    steps, set_steps: Option<u32> => "steps";
    model_timeout, set_model_timeout: Option<Duration> => "model_timeout";
    allow_background_wake, set_allow_background_wake: bool => "allow_background_wake";
    max_followups, set_max_followups: usize => "max_followups";
    instruction_paths, set_instruction_paths: Vec<PathBuf> => "instruction_paths";
    workspace_enabled, set_workspace_enabled: bool => "workspace_enabled";
    memory_max_chars, set_memory_max_chars: usize => "memory_max_chars";
    attachment_image, set_attachment_image: ImagePolicy => "attachment_image";
    attachment_text_max_chars, set_attachment_text_max_chars: usize => "attachment_text_max_chars";
    max_model_retries, set_max_model_retries: u32 => "max_model_retries";
    max_overflow_compactions, set_max_overflow_compactions: u32 => "max_overflow_compactions";
    max_stop_continuations, set_max_stop_continuations: u32 => "max_stop_continuations";
    max_permission_rechecks, set_max_permission_rechecks: u32 => "max_permission_rechecks";
    retry_delay, set_retry_delay: Duration => "retry_delay";
    retry_max_delay, set_retry_max_delay: Duration => "retry_max_delay";
    operation_timeout, set_operation_timeout: Option<Duration> => "operation_timeout";
    tool_timeout, set_tool_timeout: Option<Duration> => "tool_timeout";
    approval_timeout, set_approval_timeout: Option<Duration> => "approval_timeout";
    hook_timeout, set_hook_timeout: Option<Duration> => "hook_timeout";
    cleanup_timeout, set_cleanup_timeout: Option<Duration> => "cleanup_timeout";
    tool_cleanup_timeout, set_tool_cleanup_timeout: Duration => "tool_cleanup_timeout";
}
fields! { ["prompt"];
    system_prompt, set_system_prompt: Option<String> => "system_prompt";
    soul_file, set_soul_file: Option<PathBuf> => "soul_file";
    memory_file, set_memory_file: Option<PathBuf> => "memory_file";
    profile_file, set_profile_file: Option<PathBuf> => "profile_file";
    append_system_prompt, set_append_system_prompt: Option<String> => "append_system_prompt";
    memory_file_max_chars, set_memory_file_max_chars: usize => "memory_file_max_chars";
    profile_file_max_chars, set_profile_file_max_chars: usize => "profile_file_max_chars";
    provider_max_chars, set_provider_max_chars: usize => "provider_max_chars";
}
fields! { ["hooks"];
    events, set_events: Value => "events";
}
fn normalize_document(mut data: Value) -> Result<Value, Error> {
    let object = data.as_object_mut().ok_or("config must be an object")?;
    for (group, fields) in [
        ("ui", &["theme"][..]),
        ("security", &["yolo", "trusted_shell"][..]),
        ("session", &["directory", "extensions"][..]),
    ] {
        if let Some(value) = object.remove(group) {
            let map = value
                .as_object()
                .ok_or_else(|| format!("{group} must be an object"))?;
            for (key, value) in map {
                if !fields.contains(&key.as_str()) {
                    return Err(format!("unknown {group} field: {key}").into());
                }
                if object.insert(key.clone(), value.clone()).is_some() {
                    return Err(format!("duplicate config: {group}.{key}").into());
                }
            }
        }
    }
    if object
        .get("models")
        .and_then(Value::as_object)
        .is_some_and(|m| {
            m.contains_key("providers") || m.contains_key("model") || m.contains_key("selected")
        })
    {
        let mut group = object
            .remove("models")
            .unwrap()
            .as_object()
            .unwrap()
            .clone();
        if let Some(v) = group.remove("selected") {
            if group.insert("model".into(), v).is_some() {
                return Err("duplicate model and selected".into());
            }
        }
        if let Some(v) = group.remove("compaction") {
            group.insert("compaction_model".into(), v);
        }
        for (key, value) in group {
            if ![
                "model",
                "variant",
                "compaction_model",
                "effort_overrides",
                "providers",
            ]
            .contains(&key.as_str())
            {
                return Err(format!("unknown models field: {key}").into());
            }
            if object.insert(key.clone(), value).is_some() {
                return Err(format!("duplicate config: models.{key}").into());
            }
        }
    }
    for (old, new) in [
        ("provider", "providers"),
        ("compaction", "compaction_model"),
        ("session_dir", "directory"),
    ] {
        if let Some(v) = object.remove(old) {
            if object.insert(new.into(), v).is_some() {
                return Err(format!("duplicate config: {old}").into());
            }
        }
    }
    for (group, fields) in LEGACY_FIELDS {
        for field in fields.split_whitespace() {
            let (old, key) = field.split_once(':').unwrap_or((field, field));
            if let Some(value) = object.remove(old) {
                let map = object
                    .entry(*group)
                    .or_insert_with(empty_object)
                    .as_object_mut()
                    .ok_or_else(|| format!("{group} must be an object"))?;
                if map.insert(key.into(), value).is_some() {
                    return Err(format!("duplicate config: {old} and {group}.{key}").into());
                }
            }
        }
    }
    let mut models = object
        .remove("models")
        .unwrap_or_else(empty_object)
        .as_object()
        .ok_or("models must be an object")?
        .clone();
    if let Some(providers) = object.get_mut("providers") {
        for (provider_id, provider) in providers
            .as_object_mut()
            .ok_or("providers must be an object")?
        {
            let provider = provider
                .as_object_mut()
                .ok_or("provider must be an object")?;
            if let Some(options) = provider.remove("options") {
                for (key, value) in options
                    .as_object()
                    .ok_or("provider options must be an object")?
                {
                    if ![
                        "baseURL",
                        "apiKey",
                        "timeout",
                        "headerTimeout",
                        "chunkTimeout",
                        "requests",
                        "headers",
                    ]
                    .contains(&key.as_str())
                    {
                        return Err(format!("unknown provider options field: {key}").into());
                    }
                    if key == "requests" {
                        for (key, value) in value.as_object().ok_or("requests must be an object")? {
                            if !["rpm", "cooldown_seconds"].contains(&key.as_str()) {
                                return Err(format!("unknown requests field: {key}").into());
                            }
                            let key = if key == "cooldown_seconds" {
                                "cooldownSeconds"
                            } else {
                                key
                            };
                            if provider.insert(key.into(), value.clone()).is_some() {
                                return Err("duplicate provider request field".into());
                            }
                        }
                    } else if provider.insert(key.clone(), value.clone()).is_some() {
                        return Err(format!("duplicate provider field: {key}").into());
                    }
                }
            }
            if let Some(entries) = provider.remove("models") {
                for (id, model) in entries
                    .as_object()
                    .ok_or("provider models must be an object")?
                {
                    let mut model = model.as_object().ok_or("model must be an object")?.clone();
                    if model
                        .get("provider")
                        .is_some_and(|value| value.as_str() != Some(provider_id))
                    {
                        return Err("conflicting model provider".into());
                    }
                    model.insert("provider".into(), Value::String(provider_id.clone()));
                    if models
                        .insert(format!("{provider_id}/{id}"), Value::Object(model))
                        .is_some()
                    {
                        return Err("duplicate model entry".into());
                    }
                }
            }
        }
    }
    for (id, model) in &mut models {
        let model = model.as_object_mut().ok_or("model must be an object")?;
        if !model.contains_key("provider") {
            if let Some((provider, _)) = id.split_once('/') {
                model.insert("provider".into(), Value::String(provider.into()));
            }
        }
        if let Some(pricing) = model.remove("pricing") {
            for (key, value) in pricing.as_object().ok_or("pricing must be an object")? {
                let key = match key.as_str() {
                    "input" => "input_price",
                    "output" => "output_price",
                    _ => return Err("unknown pricing field".into()),
                };
                if model.insert(key.into(), value.clone()).is_some() {
                    return Err("duplicate model price".into());
                }
            }
        }
    }
    object.insert("models".into(), Value::Object(models));
    if let Some(hooks) = object.get_mut("hooks") {
        let map = hooks.as_object().ok_or("hooks must be an object")?;
        if !map.contains_key("events") && !map.contains_key("file") {
            *hooks = serde_json::json!({"events":hooks.clone()});
        }
    }
    Ok(data)
}
const LEGACY_FIELDS: &[(&str,&str)] = &[
    ("execution", "skill_ids memory_search_limit context steps model_timeout allow_background_wake max_followups instruction_paths workspace_enabled memory_max_chars attachment_image attachment_text_max_chars max_model_retries max_overflow_compactions max_stop_continuations max_permission_rechecks retry_delay retry_max_delay operation_timeout tool_timeout approval_timeout hook_timeout cleanup_timeout tool_cleanup_timeout max_model_calls:steps"),
    ("prompt", "system_prompt soul_file memory_file profile_file append_system_prompt memory_file_max_chars profile_file_max_chars provider_max_chars"),
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

fn canonical<T: serde::de::DeserializeOwned + Serialize>(
    data: &mut Value,
    key: &str,
) -> Result<(), Error> {
    data[key] = serde_json::to_value(decode::<T>(data[key].clone(), key)?)?;
    Ok(())
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
    fn from_document(document: Value, path: Option<PathBuf>) -> Result<Self, Error> {
        let document = normalize_document(document)?;
        let mut data = default_document();
        for (key, value) in document.as_object().unwrap() {
            if key != "$schema" && data.get(key).is_none() {
                return Err(format!("unknown config field: {key}").into());
            }
            data[key] = value.clone();
        }
        canonical::<ExecutionOptions>(&mut data, "execution")?;
        canonical::<PromptSettings>(&mut data, "prompt")?;
        canonical::<HookSettings>(&mut data, "hooks")?;
        canonical::<BTreeMap<String, ProviderConfig>>(&mut data, "providers")?;
        canonical::<BTreeMap<String, ModelConfig>>(&mut data, "models")?;
        for key in ["model", "theme"] {
            decode::<String>(data[key].clone(), key)?;
        }
        for key in ["variant", "compaction_model", "$schema"] {
            decode::<Option<String>>(data[key].clone(), key)?;
        }
        for key in ["yolo", "trusted_shell", "extensions"] {
            decode::<bool>(data[key].clone(), key)?;
        }
        decode::<PathBuf>(data["directory"].clone(), "directory")?;
        decode::<BTreeMap<String, BTreeMap<String, String>>>(
            data["effort_overrides"].clone(),
            "effort_overrides",
        )?;
        for model in data["models"].as_object().unwrap().values() {
            for variant in model["variants"].as_object().unwrap().values() {
                if let Some(value) = variant.get("disabled") {
                    decode::<bool>(value.clone(), "variant.disabled")?;
                }
            }
        }
        Ok(Self {
            document: parking_lot::Mutex::new(data),
            path,
        })
    }
    pub fn save(&self) -> Result<(), Error> {
        self.save_data(&self.document.lock())
    }
    pub(crate) fn save_data(&self, data: &Value) -> Result<(), Error> {
        let path = self
            .path
            .as_deref()
            .ok_or("config has no file path; initialize it before saving")?;
        write_bytes(path, &document_bytes(data)?)
    }
    pub(crate) fn write(&self) -> parking_lot::MutexGuard<'_, Value> {
        self.document.lock()
    }
    pub(crate) fn values(&self) -> Value {
        self.document.lock().clone()
    }
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    pub fn set_provider(&self, id: String, value: ProviderConfig) {
        self.document.lock()["providers"][id] = serde_json::to_value(value).unwrap();
    }
    pub fn set_model_config(&self, id: String, value: ModelConfig) -> Result<(), Error> {
        for options in value.variants().values() {
            if let Some(disabled) = options.get("disabled") {
                decode::<bool>(disabled.clone(), "variant.disabled")?;
            }
        }
        self.document.lock()["models"][id] = serde_json::to_value(value)?;
        Ok(())
    }
    pub fn remove_provider(&self, id: &str) -> Result<(), Error> {
        self.document.lock()["providers"]
            .as_object_mut()
            .unwrap()
            .remove(id)
            .ok_or("Unknown provider")?;
        Ok(())
    }
    pub fn remove_model(&self, id: &str) -> Result<(), Error> {
        self.document.lock()["models"]
            .as_object_mut()
            .unwrap()
            .remove(id)
            .ok_or("Unknown model")?;
        Ok(())
    }
    pub fn provider_ids(&self) -> Vec<String> {
        self.document.lock()["providers"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }
    pub fn model_ids(&self) -> Vec<String> {
        self.document.lock()["models"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }
    fn set_entry(
        &self,
        group: &str,
        id: &str,
        path: &[&str],
        value: impl Serialize,
    ) -> Result<(), Error> {
        let mut data = self.document.lock();
        let mut node = data[group]
            .get_mut(id)
            .ok_or("Unknown configuration entry")?;
        for key in &path[..path.len() - 1] {
            node = node.get_mut(*key).ok_or("Unknown configuration field")?;
        }
        let object = node
            .as_object_mut()
            .ok_or("configuration field must be an object")?;
        object.insert(
            path.last().unwrap().to_string(),
            serde_json::to_value(value)?,
        );
        Ok(())
    }
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
        Self::from_document(document, Some(path))
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
macro_rules! entry_fields {
    ($group:literal; $( $get:ident, $set:ident: $ty:ty => [$($key:literal),+]; )*) => {
        impl Config { $(
            pub fn $get(&self, id: &str) -> Result<$ty,Error> {
                let data = self.document.lock();
                let mut node = data[$group].get(id).ok_or("Unknown configuration entry")?;
                $(node = node.get($key).ok_or("Unknown configuration field")?;)+
                decode(node.clone(), "configuration field")
            }
            pub fn $set(&self, id: &str, value: $ty) -> Result<(),Error> { self.set_entry($group,id,&[$($key),+],value) }
        )* }
    };
}
entry_fields! { "providers";
    provider_api, set_provider_api: Option<String> => ["api"];
    provider_name, set_provider_name: Option<String> => ["name"];
    provider_base_url, set_provider_base_url: Option<String> => ["baseURL"];
    provider_api_key, set_provider_api_key: Option<String> => ["apiKey"];
    provider_timeout, set_provider_timeout: Option<ConnectionTimeout> => ["timeout"];
    provider_header_timeout, set_provider_header_timeout: Option<u64> => ["headerTimeout"];
    provider_chunk_timeout, set_provider_chunk_timeout: Option<u64> => ["chunkTimeout"];
    provider_rpm, set_provider_rpm: Option<u32> => ["rpm"];
    provider_cooldown_seconds, set_provider_cooldown_seconds: u64 => ["cooldownSeconds"];
    provider_headers, set_provider_headers: BTreeMap<String,String> => ["headers"];
}
entry_fields! { "models";
    model_provider, set_model_provider: String => ["provider"];
    model_id, set_model_id: Option<String> => ["id"];
    model_name, set_model_name: Option<String> => ["name"];
    model_context, set_model_context: Option<u64> => ["limit", "context"];
    model_input, set_model_input: Option<u64> => ["limit", "input"];
    model_output, set_model_output: Option<u32> => ["limit", "output"];
    model_input_price, set_model_input_price: Option<f64> => ["input_price"];
    model_output_price, set_model_output_price: Option<f64> => ["output_price"];
    model_options, set_model_options: Map<String,Value> => ["options"];
    model_headers, set_model_headers: BTreeMap<String,String> => ["headers"];
}
impl Config {
    pub fn set_provider_header(&self, id: &str, key: String, value: String) -> Result<(), Error> {
        self.set_entry("providers", id, &["headers", &key], value)
    }
    pub fn set_model_option(&self, id: &str, key: String, value: Value) -> Result<(), Error> {
        self.set_entry("models", id, &["options", &key], value)
    }
    pub fn set_model_header(&self, id: &str, key: String, value: String) -> Result<(), Error> {
        self.set_entry("models", id, &["headers", &key], value)
    }
    pub fn variant_options(&self, id: &str, variant: &str) -> Result<Map<String, Value>, Error> {
        let data = self.document.lock();
        let node = data["models"]
            .get(id)
            .and_then(|m| m["variants"].get(variant))
            .ok_or("Unknown variant")?;
        decode(node.clone(), "variant")
    }
    pub fn set_model_variant(
        &self,
        id: &str,
        variant: String,
        options: Map<String, Value>,
    ) -> Result<(), Error> {
        if let Some(disabled) = options.get("disabled") {
            decode::<bool>(disabled.clone(), "variant.disabled")?;
        }
        self.set_entry("models", id, &["variants", &variant], options)
    }
    pub fn set_variant_option(
        &self,
        id: &str,
        variant: &str,
        key: String,
        value: Value,
    ) -> Result<(), Error> {
        if key == "disabled" {
            decode::<bool>(value.clone(), "variant.disabled")?;
        }
        self.set_entry("models", id, &["variants", variant, &key], value)
    }
}
fields! { ["execution","context"];
    context_safety_margin, set_context_safety_margin: u64 => "safety_margin";
    context_advance_tokens, set_context_advance_tokens: Option<u64> => "advance_tokens";
    context_keep_recent_tokens, set_context_keep_recent_tokens: u64 => "keep_recent_tokens";
    context_summary_tokens, set_context_summary_tokens: u64 => "summary_tokens";
    context_summary_min_savings, set_context_summary_min_savings: u64 => "summary_min_savings";
}
fields! { ["execution","attachment_image"];
    image_auto_resize, set_image_auto_resize: bool => "auto_resize";
    image_max_width, set_image_max_width: u32 => "max_width";
    image_max_height, set_image_max_height: u32 => "max_height";
    image_max_base64_bytes, set_image_max_base64_bytes: usize => "max_base64_bytes";
}
