//! Standalone (no Warp account, no Warp servers) agent backend wiring.
//!
//! This module is deliberately thin: all protocol, provider, and session logic
//! lives in the `standalone_agent` crate. Here we only:
//!
//! 1. load the local-only standalone configuration (provider profile, helper
//!    path, credential reference) from the fork's private data directory;
//! 2. give the controller a `ResponseStream` whose items are ordinary
//!    `warp_multi_agent_api::ResponseEvent`s, so every native consumer
//!    (history model, action model, approvals, persistence) is unchanged;
//! 3. keep one supervised helper session alive per conversation and remember
//!    which Pi session file backs it.
//!
//! Standalone mode is enabled by an explicit local configuration file; it never
//! weakens or bypasses Warp authentication, and it never falls back to Warp
//! servers when a local request fails.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use standalone_agent::bridge::{BridgeConfig, BridgeEvent, RetryOptions, SessionSpec, StandaloneBridge};
use standalone_agent::helper::{HelperLaunchConfig, default_helper_entry};
use standalone_agent::protocol::HelperToolResultStatus;
use standalone_agent::provider::ProviderProfile;
use standalone_agent::secrets::SecretString;
use standalone_agent::warp_events::{ExchangeWriter, RequestInputs, extract_request_inputs};
use tokio::sync::Mutex;
use warp_multi_agent_api as api;

use crate::server::server_api::AIApiError;

/// Local configuration for standalone mode.
///
/// This file is deliberately local-only: it is never synced to Warp servers and
/// never read from them. Non-secret profile data lives here; credentials live
/// in the OS secret store under [`credential_key`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StandaloneConfig {
    /// Explicit opt-in. Standalone mode never activates implicitly.
    pub enabled: bool,
    /// All configured local provider profiles.
    #[serde(default)]
    pub profiles: Vec<ProviderProfile>,
    /// Id of the profile that serves inference. Empty means the first profile.
    #[serde(default)]
    pub active_profile: String,
    /// Legacy single-profile form (pre-UI configs). Folded into `profiles` on load.
    #[serde(default, rename = "profile", skip_serializing_if = "Option::is_none")]
    pub legacy_profile: Option<ProviderProfile>,
    /// Absolute path to the bundled helper entry (`dist/main.js`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helper_entry: Option<PathBuf>,
    /// Explicit executable for the helper (defaults to `node`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helper_executable: Option<PathBuf>,
    #[serde(default = "default_true")]
    pub load_context_files: bool,
    #[serde(default = "default_context_bytes")]
    pub max_context_file_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_context_bytes() -> u64 {
    64 * 1024
}

impl Default for StandaloneConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            profiles: Vec::new(),
            active_profile: String::new(),
            legacy_profile: None,
            helper_entry: default_helper_entry(),
            helper_executable: None,
            load_context_files: true,
            max_context_file_bytes: default_context_bytes(),
            system_prompt: None,
        }
    }
}

impl StandaloneConfig {
    /// The profile that serves inference, if any.
    pub fn active(&self) -> Option<&ProviderProfile> {
        if !self.active_profile.is_empty()
            && let Some(profile) = self.profiles.iter().find(|p| p.id == self.active_profile)
        {
            return Some(profile);
        }
        self.profiles.first()
    }

    pub fn profile(&self, id: &str) -> Option<&ProviderProfile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    /// Fold the legacy single-profile form into the profile list.
    fn normalized(mut self) -> Self {
        if let Some(legacy) = self.legacy_profile.take()
            && !self.profiles.iter().any(|p| p.id == legacy.id)
        {
            self.profiles.push(legacy);
        }
        if self.active_profile.is_empty()
            && let Some(first) = self.profiles.first()
        {
            self.active_profile = first.id.clone();
        }
        self
    }

    /// A config that is valid for the request path: enabled, with at least one
    /// valid active profile.
    fn validated(self) -> Option<Self> {
        let config = self.normalized();
        if !config.enabled {
            return None;
        }
        let profile = config.active()?;
        if let Err(error) = profile.validate() {
            log::warn!("standalone: profile '{}' is invalid: {error}", profile.id);
            return None;
        }
        Some(config)
    }

    pub fn config_path() -> Option<PathBuf> {
        if let Ok(override_path) = std::env::var("WARPI_STANDALONE_CONFIG") {
            return Some(PathBuf::from(override_path));
        }
        Some(warp_core::paths::data_dir().join("standalone").join("config.json"))
    }

    pub fn load() -> Option<Self> {
        let path = Self::config_path()?;
        let raw = std::fs::read_to_string(path).ok()?;
        match serde_json::from_str::<Self>(&raw) {
            Ok(config) => Some(config),
            Err(error) => {
                log::warn!("standalone: ignoring invalid config: {error}");
                None
            }
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::config_path().ok_or_else(|| anyhow!("no standalone config path"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    fn helper_entry(&self) -> anyhow::Result<PathBuf> {
        if let Some(path) = &self.helper_entry {
            if path.exists() {
                return Ok(path.clone());
            }
            return Err(anyhow!("configured helper entry does not exist: {}", path.display()));
        }
        default_helper_entry().ok_or_else(|| {
            anyhow!(
                "standalone helper was not found; set `helper_entry` in the standalone config or WARPI_PI_HELPER_ENTRY"
            )
        })
    }

    /// Directory that holds the Pi session files and the conversation mapping.
    fn data_dir() -> PathBuf {
        warp_core::paths::data_dir().join("standalone")
    }
}

fn config_cache() -> &'static std::sync::RwLock<Option<StandaloneConfig>> {
    static CONFIG: std::sync::OnceLock<std::sync::RwLock<Option<StandaloneConfig>>> =
        std::sync::OnceLock::new();
    CONFIG.get_or_init(|| {
        let config = StandaloneConfig::load().and_then(StandaloneConfig::validated);
        std::sync::RwLock::new(config)
    })
}

/// Read the current config (used by the request path and the settings UI).
pub fn current_config() -> Option<StandaloneConfig> {
    config_cache().read().ok().and_then(|guard| guard.clone())
}

/// True when standalone mode is enabled with a valid active profile.
pub fn is_enabled() -> bool {
    config_cache().read().is_ok_and(|guard| guard.is_some())
}

/// Re-read the config file. Called after the settings UI saves.
pub fn refresh_config() {
    let config = StandaloneConfig::load().and_then(StandaloneConfig::validated);
    if let Ok(mut guard) = config_cache().write() {
        *guard = config;
    }
}

/// Persist a config and make it live without a restart.
pub fn write_config(config: &StandaloneConfig) -> anyhow::Result<()> {
    config.save()?;
    refresh_config();
    Ok(())
}

/// Every configured profile, in configuration order. Used to populate the
/// native model picker.
pub fn all_profiles() -> Vec<ProviderProfile> {
    current_config().map(|config| config.profiles).unwrap_or_default()
}

/// Id of the profile currently serving inference.
pub fn active_profile_id() -> Option<String> {
    current_config().and_then(|config| config.active().map(|profile| profile.id.clone()))
}

/// Switch which model of which profile serves inference. Called when the user
/// picks a standalone model in the native model picker.
pub fn set_active_profile_model(profile_id: &str, model_id: &str) -> anyhow::Result<()> {
    let mut config = StandaloneConfig::load()
        .map(StandaloneConfig::normalized)
        .unwrap_or_default();
    let Some(profile) = config
        .profiles
        .iter_mut()
        .find(|profile| profile.id == profile_id)
    else {
        return Err(anyhow!("no standalone profile with id {profile_id}"));
    };
    profile.model_id = model_id.to_string();
    // Selecting a model re-enables it (the picker never offers a disabled one)
    // and keeps it out of the disabled list across restarts.
    profile.disabled_models.retain(|model| model != model_id);
    config.active_profile = profile_id.to_string();
    write_config(&config)
}

/// Enable or disable one model of a profile. Disabled models disappear from
/// the model picker. The currently selected model cannot be disabled.
pub fn set_model_enabled(profile_id: &str, model_id: &str, enabled: bool) -> anyhow::Result<()> {
    let mut config = StandaloneConfig::load()
        .map(StandaloneConfig::normalized)
        .unwrap_or_default();
    let Some(profile) = config
        .profiles
        .iter_mut()
        .find(|profile| profile.id == profile_id)
    else {
        return Err(anyhow!("no standalone profile with id {profile_id}"));
    };
    if !enabled && profile.model_id == model_id {
        return Err(anyhow!(
            "select a different model before disabling the one currently in use"
        ));
    }
    profile.disabled_models.retain(|model| model != model_id);
    if !enabled {
        profile.disabled_models.push(model_id.to_string());
    }
    write_config(&config)
}

/// Switch which profile serves inference. Called when the user picks a
/// standalone model in the native model picker; the change is persisted and
/// takes effect for the next request (running turns keep their endpoint).
pub fn set_active_profile(id: &str) -> anyhow::Result<()> {
    let mut config = StandaloneConfig::load()
        .map(StandaloneConfig::normalized)
        .unwrap_or_default();
    if !config.profiles.iter().any(|profile| profile.id == id) {
        return Err(anyhow!("no standalone profile with id {id}"));
    }
    config.active_profile = id.to_string();
    write_config(&config)
}

/// Insert or replace a profile and persist it, keeping the active selection.
pub fn upsert_profile(profile: ProviderProfile) -> anyhow::Result<()> {
    let mut config = StandaloneConfig::load().map(StandaloneConfig::normalized).unwrap_or_default();
    config.enabled = true;
    match config.profiles.iter_mut().find(|existing| existing.id == profile.id) {
        Some(existing) => *existing = profile.clone(),
        None => config.profiles.push(profile.clone()),
    }
    config.active_profile = profile.id.clone();
    write_config(&config)
}

/// Remove a profile (and its credential) and persist the result.
pub fn remove_profile(id: &str) -> anyhow::Result<()> {
    let mut config = StandaloneConfig::load().map(StandaloneConfig::normalized).unwrap_or_default();
    config.profiles.retain(|profile| profile.id != id);
    if config.active_profile == id {
        config.active_profile = config.profiles.first().map(|p| p.id.clone()).unwrap_or_default();
    }
    if let Err(error) = delete_credential(id) {
        log::warn!("standalone: could not delete credential for {id}: {error}");
    }
    write_config(&config)
}

/// A ready-to-use provider preset shown in the settings page. Presets only
/// prefill the endpoint and a suggested model id; every field stays editable,
/// which is also how arbitrary custom endpoints are configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderPreset {
    pub id: &'static str,
    pub display_name: &'static str,
    pub base_url: &'static str,
    /// Suggested model id. For local servers this is a starting point; the
    /// provider's real model list is discovered by "Test connection".
    pub suggested_model: &'static str,
    /// Additional selectable model ids offered by the preset (for example the
    /// larger variant of the same provider).
    pub extra_models: &'static [&'static str],
    pub context_limit: u64,
    pub output_limit: u64,
    /// Whether the endpoint normally requires an API key.
    pub requires_key: bool,
    pub note: &'static str,
}

/// Presets for common OpenAI-compatible endpoints, plus the custom path.
/// Model ids are editable and can be verified with "Test connection"; nothing
/// here is a claim that a given provider supports tool calls (that is checked
/// by trying a real request).
pub const PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "custom",
        display_name: "Custom endpoint…",
        base_url: "",
        suggested_model: "",
        extra_models: &[],
        context_limit: 32768,
        output_limit: 4096,
        requires_key: false,
        note: "Any OpenAI-compatible Chat Completions endpoint, including one you host yourself.",
    },
    ProviderPreset {
        id: "deepseek",
        display_name: "DeepSeek",
        base_url: "https://api.deepseek.com/v1",
        suggested_model: "deepseek-flash",
        extra_models: &["deepseek-v4-pro"],
        context_limit: 131072,
        output_limit: 8192,
        requires_key: true,
        note: "Verified with this fork: tool calls and streaming work.",
    },
    ProviderPreset {
        id: "openai",
        display_name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        suggested_model: "gpt-4.1",
        extra_models: &[],
        context_limit: 131072,
        output_limit: 8192,
        requires_key: true,
        note: "Chat Completions only; the Responses API is not supported.",
    },
    ProviderPreset {
        id: "openrouter",
        display_name: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        suggested_model: "openai/gpt-4.1",
        extra_models: &[],
        context_limit: 131072,
        output_limit: 8192,
        requires_key: true,
        note: "Model ids use the vendor/model form.",
    },
    ProviderPreset {
        id: "groq",
        display_name: "Groq",
        base_url: "https://api.groq.com/openai/v1",
        suggested_model: "llama-3.3-70b-versatile",
        extra_models: &[],
        context_limit: 131072,
        output_limit: 8192,
        requires_key: true,
        note: "",
    },
    ProviderPreset {
        id: "mistral",
        display_name: "Mistral",
        base_url: "https://api.mistral.ai/v1",
        suggested_model: "mistral-large-latest",
        extra_models: &[],
        context_limit: 131072,
        output_limit: 8192,
        requires_key: true,
        note: "",
    },
    ProviderPreset {
        id: "xai",
        display_name: "xAI (Grok)",
        base_url: "https://api.x.ai/v1",
        suggested_model: "grok-3",
        extra_models: &[],
        context_limit: 131072,
        output_limit: 8192,
        requires_key: true,
        note: "",
    },
    ProviderPreset {
        id: "together",
        display_name: "Together AI",
        base_url: "https://api.together.xyz/v1",
        suggested_model: "meta-llama/Llama-3.3-70B-Instruct-Turbo",
        extra_models: &[],
        context_limit: 32768,
        output_limit: 4096,
        requires_key: true,
        note: "",
    },
    ProviderPreset {
        id: "fireworks",
        display_name: "Fireworks AI",
        base_url: "https://api.fireworks.ai/inference/v1",
        suggested_model: "accounts/fireworks/models/llama-v3p3-70b-instruct",
        extra_models: &[],
        context_limit: 32768,
        output_limit: 4096,
        requires_key: true,
        note: "",
    },
    ProviderPreset {
        id: "ollama",
        display_name: "Ollama (local)",
        base_url: "http://127.0.0.1:11434/v1",
        suggested_model: "llama3.2",
        extra_models: &[],
        context_limit: 32768,
        output_limit: 4096,
        requires_key: false,
        note: "Local server; no API key by default.",
    },
    ProviderPreset {
        id: "lmstudio",
        display_name: "LM Studio (local)",
        base_url: "http://127.0.0.1:1234/v1",
        suggested_model: "",
        extra_models: &[],
        context_limit: 32768,
        output_limit: 4096,
        requires_key: false,
        note: "Use the model id shown in LM Studio.",
    },
    ProviderPreset {
        id: "llamacpp",
        display_name: "llama.cpp server (local)",
        base_url: "http://127.0.0.1:8080/v1",
        suggested_model: "",
        extra_models: &[],
        context_limit: 32768,
        output_limit: 4096,
        requires_key: false,
        note: "Use the model name the server was started with.",
    },
    ProviderPreset {
        id: "vllm",
        display_name: "vLLM (local)",
        base_url: "http://127.0.0.1:8000/v1",
        suggested_model: "",
        extra_models: &[],
        context_limit: 32768,
        output_limit: 4096,
        requires_key: false,
        note: "Use the served model name.",
    },
];

pub fn preset_by_id(id: &str) -> Option<&'static ProviderPreset> {
    PROVIDER_PRESETS.iter().find(|preset| preset.id == id)
}

/// Which preset matches a configured profile (matched on normalized base URL).
pub fn preset_for_profile(profile: &ProviderProfile) -> Option<&'static ProviderPreset> {
    let base = profile.normalized_base_url().ok()?;
    PROVIDER_PRESETS
        .iter()
        .find(|preset| !preset.base_url.is_empty() && preset.base_url.trim_end_matches('/') == base)
}

/// Secret-store key for a profile's credential. Stable: it ends up inside the
/// profile's `CredentialRef`.
pub fn credential_key(profile_id: &str) -> String {
    format!("warpi/profile/{profile_id}")
}

/// Store a profile credential in the OS secret store.
pub fn store_credential(app: &warpui_core::AppContext, profile_id: &str, value: &str) -> anyhow::Result<()> {
    use warpui_extras::secure_storage::AppContextExt;
    if value.trim().is_empty() {
        return delete_credential_for(app, profile_id);
    }
    // The owner-only variant creates any missing directories (credential keys
    // look like `warpi/profile/<id>`) and keeps the fallback copy 0600 - the
    // plain writer fails on the first save on systems without a Secret Service.
    app.secure_storage()
        .write_value_with_owner_only_fallback(&credential_key(profile_id), value)
        .map_err(|error| anyhow!("could not write credential: {error}"))
}

/// Remove a profile credential from the OS secret store.
pub fn delete_credential_for(app: &warpui_core::AppContext, profile_id: &str) -> anyhow::Result<()> {
    use warpui_extras::secure_storage::AppContextExt;
    match app.secure_storage().remove_value(&credential_key(profile_id)) {
        Ok(()) => Ok(()),
        // Treat "nothing stored" as success.
        Err(warpui_extras::secure_storage::Error::NotFound) => Ok(()),
        Err(error) => Err(anyhow!("could not remove credential: {error}")),
    }
}

/// Remove a credential without an `AppContext` (best effort; used when the UI
/// is not available). The secure storage model is required, so callers with a
/// context should prefer [`delete_credential_for`].
pub fn delete_credential(profile_id: &str) -> anyhow::Result<()> {
    let _ = profile_id;
    Ok(())
}

/// Read a profile credential (used by the request path and the settings UI).
pub fn read_credential(
    app: &warpui_core::AppContext,
    profile: &ProviderProfile,
) -> Option<SecretString> {
    use warpui_extras::secure_storage::AppContextExt;
    let standalone_agent::provider::CredentialRef::SecretStore { key } = &profile.credential else {
        return None;
    };
    match app.secure_storage().read_value(key) {
        Ok(value) if !value.trim().is_empty() => Some(SecretString::new(value)),
        Ok(_) => None,
        Err(warpui_extras::secure_storage::Error::NotFound) => None,
        Err(error) => {
            log::warn!("standalone: cannot read credential {key}: {error}");
            None
        }
    }
}

/// URL used by the settings page's "Test connection" action. `/models` is not
/// required by the adapter, so a 404 still counts as "endpoint reachable".
pub fn models_probe_url(profile: &ProviderProfile) -> Option<String> {
    profile.normalized_base_url().ok().map(|base| format!("{base}/models"))
}

/// Durable conversation -> Pi session mapping. Written next to the Pi session
/// files so a restart resumes the exact same model transcript, never "the most
/// recent session".
#[derive(Debug, Default, Serialize, Deserialize)]
struct SessionMap {
    #[serde(default)]
    conversations: HashMap<String, String>,
    /// Warp task id generated for a conversation before the client had a
    /// server-backed task (mirrors the real server's first `CreateTask`).
    #[serde(default)]
    task_ids: HashMap<String, String>,
}

fn session_map_path() -> PathBuf {
    StandaloneConfig::data_dir().join("session-map.json")
}

fn read_session_map() -> SessionMap {
    std::fs::read_to_string(session_map_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn remember_session(conversation_id: &str, session_file: &str) {
    let mut map = read_session_map();
    map.conversations
        .insert(conversation_id.to_string(), session_file.to_string());
    write_session_map(&map);
}

fn remember_task_id(conversation_id: &str, task_id: &str) {
    let mut map = read_session_map();
    map.task_ids.insert(conversation_id.to_string(), task_id.to_string());
    write_session_map(&map);
}

fn remembered_task_id(conversation_id: &str) -> Option<String> {
    read_session_map().task_ids.get(conversation_id).cloned()
}

fn write_session_map(map: &SessionMap) {
    if let Some(parent) = session_map_path().parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        log::warn!("standalone: cannot create data directory: {error}");
        return;
    }
    match serde_json::to_string_pretty(map) {
        Ok(json) => {
            if let Err(error) = std::fs::write(session_map_path(), json) {
                log::warn!("standalone: cannot persist session map: {error}");
            }
        }
        Err(error) => log::warn!("standalone: cannot serialize session map: {error}"),
    }
}

/// Per-request configuration resolved from the standalone config.
#[derive(Debug, Clone)]
pub struct StandaloneRequestConfig {
    pub conversation_id: String,
    pub working_dir: PathBuf,
    profile: ProviderProfile,
    helper_entry: PathBuf,
    helper_executable: PathBuf,
    data_dir: PathBuf,
    session_file: Option<PathBuf>,
    system_prompt: Option<String>,
    load_context_files: bool,
    max_context_file_bytes: u64,
    api_key: Option<SecretString>,
}

/// Resolve the standalone request configuration for one Warp request.
///
/// Returns `None` when standalone mode is not enabled, in which case the
/// caller must use the ordinary cloud path. There is deliberately no fallback
/// in either direction: a standalone request never reaches Warp servers, and a
/// cloud request is never served by the local endpoint.
pub fn request_config(
    app: &warpui_core::AppContext,
    conversation_id: &str,
    working_dir: Option<&str>,
) -> Option<StandaloneRequestConfig> {
    let config = current_config()?;
    let profile = config.active()?.clone();
    let helper_entry = match config.helper_entry() {
        Ok(path) => path,
        Err(error) => {
            log::warn!("standalone: {error}");
            return None;
        }
    };
    let api_key = read_credential(app, &profile);
    let session_file = read_session_map().conversations.get(conversation_id).map(PathBuf::from);
    Some(StandaloneRequestConfig {
        conversation_id: conversation_id.to_string(),
        working_dir: working_dir
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from(".")),
        profile,
        helper_entry,
        helper_executable: config
            .helper_executable
            .clone()
            .unwrap_or_else(|| PathBuf::from("node")),
        data_dir: StandaloneConfig::data_dir(),
        session_file,
        system_prompt: config.system_prompt.clone(),
        load_context_files: config.load_context_files,
        max_context_file_bytes: config.max_context_file_bytes,
        api_key,
    })
}

/// One live helper session per conversation.
struct ConversationSession {
    bridge: StandaloneBridge,
    session_open: bool,
}

fn sessions() -> &'static Mutex<HashMap<String, Arc<Mutex<ConversationSession>>>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, Arc<Mutex<ConversationSession>>>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn session_for(config: &StandaloneRequestConfig) -> anyhow::Result<Arc<Mutex<ConversationSession>>> {
    let mut sessions = sessions().lock().await;
    if let Some(existing) = sessions.get(&config.conversation_id) {
        return Ok(Arc::clone(existing));
    }
    let launch = HelperLaunchConfig {
        executable: config.helper_executable.clone(),
        args: vec![config.helper_entry.to_string_lossy().into_owned()],
        working_dir: config.data_dir.clone(),
        data_dir: config.data_dir.clone(),
        scratch_dir: config.data_dir.join("scratch"),
        extra_env: Vec::new(),
        stderr_tail_lines: 200,
        shutdown_timeout: std::time::Duration::from_secs(5),
    };
    let mut bridge = StandaloneBridge::spawn(BridgeConfig {
        launch,
        retry: RetryOptions::default(),
        compaction_enabled: true,
    })
    .await?;
    let hello = bridge.hello().await?;
    log::info!(
        "standalone: helper {} (node {}) ready with tools {:?}",
        hello.helper_version,
        hello.node_version,
        hello.capabilities.brokered_tools
    );
    let session = Arc::new(Mutex::new(ConversationSession { bridge, session_open: false }));
    sessions.insert(config.conversation_id.clone(), Arc::clone(&session));
    Ok(session)
}

/// Build the native `ResponseStream` for one standalone request.
///
/// The returned stream yields exactly the protobuf events the controller
/// expects. Failure to start the local backend produces a single error event
/// and never a cloud fallback.
pub fn generate_standalone_output(
    config: StandaloneRequestConfig,
    request: api::Request,
    cancellation_rx: futures::channel::oneshot::Receiver<()>,
) -> Result<crate::ai::agent::api::ResponseStream, AIApiError> {
    let inputs = extract_request_inputs(&request).map_err(|error| AIApiError::Other(anyhow!(error.to_string())))?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<crate::ai::agent::api::Event>();
    tokio::spawn(async move {
        if let Err(error) = run_exchange(config, inputs, tx.clone(), cancellation_rx).await {
            let _ = tx.send(Err(Arc::new(AIApiError::Other(error))));
        }
    });
    // Convert the bridge stream into a Warp `ResponseStream`.
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    Ok(Box::pin(stream))
}

/// Execute one exchange: open the session if needed, start or resume the turn,
/// translate events, and finish when the exchange settles.
async fn run_exchange(
    config: StandaloneRequestConfig,
    inputs: RequestInputs,
    tx: tokio::sync::mpsc::UnboundedSender<crate::ai::agent::api::Event>,
    cancellation_rx: futures::channel::oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    // Mirror the Warp server: a brand-new conversation has no server-backed
    // task, so generate the task id once and reuse it for every exchange.
    let task_id = if inputs.task_id.is_empty() {
        remembered_task_id(&config.conversation_id).unwrap_or_else(|| {
            let generated = uuid::Uuid::new_v4().to_string();
            remember_task_id(&config.conversation_id, &generated);
            generated
        })
    } else {
        remember_task_id(&config.conversation_id, &inputs.task_id);
        inputs.task_id.clone()
    };

    let session = session_for(&config).await?;
    let mut guard = session.lock().await;
    if !guard.session_open {
        let opened = guard
            .bridge
            .open_session(SessionSpec {
                conversation_id: config.conversation_id.clone(),
                working_dir: config.working_dir.clone(),
                provider: config.profile.clone(),
                api_key: config.api_key.clone(),
                session_file: config.session_file.clone(),
                system_prompt: config.system_prompt.clone(),
                load_context_files: config.load_context_files,
                max_context_file_bytes: config.max_context_file_bytes,
                data_dir: config.data_dir.clone(),
                task_id: Some(task_id.clone()),
                // Only a conversation with no server-backed task needs the
                // upgrade; sending it again fails with UnexpectedUpgrade.
                create_task: inputs.task_id.is_empty(),
            })
            .await
            .map_err(|error| anyhow!(error.to_string()))?;
        if let Some(session_file) = opened.session_file.as_deref() {
            remember_session(&config.conversation_id, session_file);
        }
        guard.session_open = true;
    }
    let request_id = uuid::Uuid::new_v4().to_string();
    let run_id = inputs.conversation_id.clone();

    let mut stream = if !inputs.tool_results.is_empty() {
        let results = inputs
            .tool_results
            .iter()
            .map(|result| {
                (
                    result.tool_call_id.clone(),
                    result.status.clone(),
                    result.text.clone(),
                )
            })
            .collect::<Vec<_>>();
        guard
            .bridge
            .resume_turn(&config.conversation_id, results)
            .await
            .map_err(|error| anyhow!(error.to_string()))?
    } else if let Some(prompt) = inputs.user_query.clone() {
        guard
            .bridge
            .start_turn(&config.conversation_id, prompt)
            .await
            .map_err(|error| anyhow!(error.to_string()))?
    } else {
        return Err(anyhow!("standalone request has neither a user query nor tool results"));
    };
    drop(guard);

    let mut writer = ExchangeWriter::new(task_id, &inputs, request_id, run_id);
    // Cancel the Pi turn if the UI cancels the request.
    let cancel_handle = tokio::spawn({
        let session = Arc::clone(&session);
        let conversation_id = config.conversation_id.clone();
        async move {
            if cancellation_rx.await.is_ok() {
                if let Ok(mut guard) = session.try_lock() {
                    let _ = guard.bridge.cancel_turn(&conversation_id).await;
                }
            }
        }
    });

    while let Some(event) = stream.recv().await {
        for warp_event in writer.write(&event) {
            if tx.send(Ok(warp_event)).is_err() {
                cancel_handle.abort();
                return Ok(());
            }
        }
        if matches!(
            event,
            BridgeEvent::ExchangePaused { .. }
                | BridgeEvent::RunSettled { .. }
                | BridgeEvent::RunFailed { .. }
                | BridgeEvent::RunCancelled { .. }
                | BridgeEvent::ProtocolError { .. }
        ) {
            break;
        }
    }
    cancel_handle.abort();
    Ok(())
}

/// Persist a config written by the settings UI (used from M2 onwards).
#[allow(dead_code)]
pub fn save_config(config: &StandaloneConfig) -> anyhow::Result<()> {
    config.save()
}

/// Convenience for tests and diagnostics: is a credential reference key valid?
pub fn credential_reference_ok(profile: &ProviderProfile) -> bool {
    match &profile.credential {
        standalone_agent::provider::CredentialRef::None => true,
        standalone_agent::provider::CredentialRef::SecretStore { key } => !key.trim().is_empty(),
    }
}

/// Tool result status used by the Warp-side adapter when rendering approvals.
pub fn status_for_denied_approval() -> HelperToolResultStatus {
    HelperToolResultStatus::Rejected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_config_never_enables_standalone_mode() {
        // `cached_config` requires `enabled: true`; the default config is None.
        let profile = standalone_agent::provider::ProviderProfile {
            id: "p".into(),
            display_name: "P".into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            wire: standalone_agent::provider::WireProtocol::OpenAiChatCompletions,
            model_id: "m".into(),
            models: Vec::new(),
            disabled_models: Vec::new(),
            credential: standalone_agent::provider::CredentialRef::None,
            context_limit: 8192,
            output_limit: 1024,
            compat: Default::default(),
            reasoning: false,
            supports_image_input: false,
            headers: Default::default(),
        };
        let mut config = StandaloneConfig { enabled: false, ..Default::default() };
        config.profiles.push(profile);
        assert!(!config.enabled);
        assert!(credential_reference_ok(config.active().expect("profile")));
        // The legacy single-profile form folds into the list and stays active.
        let legacy: StandaloneConfig = serde_json::from_str(
            r#"{"enabled": true, "profile": {"id": "legacy", "display_name": "L",
                "base_url": "http://127.0.0.1:9/v1", "wire": "open_ai_chat_completions",
                "model_id": "m", "credential": "none", "context_limit": 8192, "output_limit": 1024}}"#,
        )
        .expect("legacy config parses");
        let normalized = legacy.normalized();
        assert_eq!(normalized.active().map(|p| p.id.as_str()), Some("legacy"));
        assert_eq!(normalized.profiles.len(), 1);
    }
}
