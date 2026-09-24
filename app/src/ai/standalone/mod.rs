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

pub mod usage_model;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use standalone_agent::bridge::{
    BridgeConfig, BridgeError, BridgeEvent, BridgeTimeouts, OrphanEvent, RetryOptions, SessionSpec,
    StandaloneBridge,
};
use standalone_agent::helper::{
    HelperLaunchConfig, default_helper_entry, default_helper_executable,
};
use standalone_agent::protocol::{AgentUsage, HelperSubagents};
use standalone_agent::provider::ProviderProfile;
use standalone_agent::secrets::SecretString;
use standalone_agent::usage_ledger::{
    CompactionRecord, ContextSource, ContextUsage, LEDGER_VERSION, RecordCategory, RecordSource,
    UsageCounters, UsageLedger, UsageRecord, UsageTiming, now_rfc3339,
};
use standalone_agent::warp_events::{ExchangeWriter, RequestInputs, extract_request_inputs};
use tokio::sync::Mutex;
use warp_multi_agent_api as api;

use crate::server::server_api::AIApiError;

/// How long an exchange that lost its receiver waits for an in-flight
/// `turn.cancel` to reach the bridge before detaching the cancel task.
const CANCEL_DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// Explicit executable for the helper. When unset, warpi prefers the Node
    /// runtime the installer bundled next to the executable and only then falls
    /// back to `node` on `PATH`; the helper requires Node.js >= 22.19.0. Set
    /// this to force a specific runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helper_executable: Option<PathBuf>,
    #[serde(default = "default_true")]
    pub load_context_files: bool,
    #[serde(default = "default_context_bytes")]
    pub max_context_file_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Opt-in helper-internal read-only subagents (`task` tool). Absent or
    /// `enabled: false` keeps the helper inert; a bridge also requires the
    /// helper to advertise the `subagents` capability before forwarding it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagents: Option<HelperSubagents>,
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
            subagents: None,
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

    /// Return this config with the mode switch set, preserving every profile.
    fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
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
        Some(
            warp_core::paths::data_dir()
                .join("standalone")
                .join("config.json"),
        )
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
            return Err(anyhow!(
                "configured helper entry does not exist: {}",
                path.display()
            ));
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
    current_config()
        .map(|config| config.profiles)
        .unwrap_or_default()
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

/// Fold an upserted profile into the config: the profile becomes active and
/// standalone mode is enabled, because configuring a provider is the user's
/// opt-in to the local backend. Pure so the enable-on-save contract is testable.
fn apply_upsert(mut config: StandaloneConfig, profile: ProviderProfile) -> StandaloneConfig {
    config.enabled = true;
    match config
        .profiles
        .iter_mut()
        .find(|existing| existing.id == profile.id)
    {
        Some(existing) => *existing = profile.clone(),
        None => config.profiles.push(profile.clone()),
    }
    config.active_profile = profile.id.clone();
    config
}

/// Insert or replace a profile and persist it. Standalone mode is enabled as a
/// side effect: a user who configures a provider intends to use it.
pub fn upsert_profile(profile: ProviderProfile) -> anyhow::Result<()> {
    let config = StandaloneConfig::load()
        .map(StandaloneConfig::normalized)
        .unwrap_or_default();
    write_config(&apply_upsert(config, profile))
}

/// Enable or disable standalone mode, preserving every configured profile.
/// This is the explicit mode switch, distinct from [`upsert_profile`], which
/// always enables.
pub fn set_enabled(enabled: bool) -> anyhow::Result<()> {
    let config = StandaloneConfig::load()
        .map(StandaloneConfig::normalized)
        .unwrap_or_default()
        .with_enabled(enabled);
    write_config(&config)
}

/// Remove a profile (and its credential) and persist the result.
pub fn remove_profile(id: &str) -> anyhow::Result<()> {
    let mut config = StandaloneConfig::load()
        .map(StandaloneConfig::normalized)
        .unwrap_or_default();
    config.profiles.retain(|profile| profile.id != id);
    if config.active_profile == id {
        config.active_profile = config
            .profiles
            .first()
            .map(|p| p.id.clone())
            .unwrap_or_default();
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
        id: "kimi",
        display_name: "Kimi (Moonshot)",
        base_url: "https://api.moonshot.ai/v1",
        suggested_model: "kimi-k2-0905-preview",
        extra_models: &["kimi-k2-turbo-preview"],
        context_limit: 262144,
        output_limit: 8192,
        requires_key: true,
        note: "Kimi K2 models; tool calls are supported by the OpenAI-compatible endpoint.",
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
pub fn store_credential(
    app: &warpui_core::AppContext,
    profile_id: &str,
    value: &str,
) -> anyhow::Result<()> {
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
pub fn delete_credential_for(
    app: &warpui_core::AppContext,
    profile_id: &str,
) -> anyhow::Result<()> {
    use warpui_extras::secure_storage::AppContextExt;
    match app
        .secure_storage()
        .remove_value(&credential_key(profile_id))
    {
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
    profile
        .normalized_base_url()
        .ok()
        .map(|base| format!("{base}/models"))
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

fn read_session_map_from(path: &Path) -> SessionMap {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn read_session_map() -> SessionMap {
    read_session_map_from(&session_map_path())
}

/// Serializes every read-modify-write of the session map. The map is written
/// from several tokio workers; without this lock two concurrent first requests
/// can each overwrite the other's entry.
fn session_map_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

fn update_session_map(update: impl FnOnce(&mut SessionMap)) {
    update_session_map_at(&session_map_path(), update);
}

fn update_session_map_at(path: &Path, update: impl FnOnce(&mut SessionMap)) {
    let _guard = session_map_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut map = read_session_map_from(path);
    // The read-modify-write window is made observable in tests so the
    // concurrency regression test fails without the lock above.
    #[cfg(test)]
    std::thread::sleep(Duration::from_millis(5));
    update(&mut map);
    write_session_map_to(path, &map);
}

fn remember_session(conversation_id: &str, session_file: &str) {
    update_session_map(|map| {
        map.conversations
            .insert(conversation_id.to_string(), session_file.to_string());
    });
}

fn remember_task_id(conversation_id: &str, task_id: &str) {
    update_session_map(|map| {
        map.task_ids
            .insert(conversation_id.to_string(), task_id.to_string());
    });
}

fn remembered_task_id(conversation_id: &str) -> Option<String> {
    read_session_map().task_ids.get(conversation_id).cloned()
}

/// Write the map atomically: a crash or a concurrent reader never observes a
/// half-written file, and the rename keeps the previous version until the new
/// one is complete.
fn write_session_map_to(path: &Path, map: &SessionMap) {
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        log::warn!("standalone: cannot create data directory: {error}");
        return;
    }
    let json = match serde_json::to_string_pretty(map) {
        Ok(json) => json,
        Err(error) => {
            log::warn!("standalone: cannot serialize session map: {error}");
            return;
        }
    };
    let temp = path.with_extension("json.tmp");
    if let Err(error) = std::fs::write(&temp, json) {
        log::warn!("standalone: cannot write session map: {error}");
        return;
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        log::warn!("standalone: cannot replace session map: {error}");
        let _ = std::fs::remove_file(&temp);
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
    subagents: Option<HelperSubagents>,
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
    let session_file = read_session_map()
        .conversations
        .get(conversation_id)
        .map(PathBuf::from);
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
            .or_else(default_helper_executable)
            .unwrap_or_else(|| PathBuf::from("node")),
        data_dir: StandaloneConfig::data_dir(),
        session_file,
        system_prompt: config.system_prompt.clone(),
        load_context_files: config.load_context_files,
        max_context_file_bytes: config.max_context_file_bytes,
        api_key,
        subagents: config.subagents.clone(),
    })
}

/// Identity of the request inputs a session was opened with. A change here on a
/// fresh prompt means the conversation must reopen so the new provider, model,
/// credential, or working directory actually serves inference.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionFingerprint {
    profile_id: String,
    model_id: String,
    base_url: String,
    working_dir: PathBuf,
    /// Hash of the credential value (never the value itself) so a rotated key
    /// reopens the session.
    credential_hash: u64,
    /// Enabling, disabling, or reconfiguring subagents reopens the session so
    /// the helper's `task` tool set tracks the config.
    subagents: Option<HelperSubagents>,
}

impl SessionFingerprint {
    fn from_request(config: &StandaloneRequestConfig) -> Self {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        match config.api_key.as_ref() {
            Some(key) => key.expose_secret().hash(&mut hasher),
            None => 0u8.hash(&mut hasher),
        }
        Self {
            profile_id: config.profile.id.clone(),
            model_id: config.profile.model_id.clone(),
            base_url: config.profile.normalized_base_url().unwrap_or_default(),
            working_dir: config.working_dir.clone(),
            credential_hash: hasher.finish(),
            subagents: config.subagents.clone(),
        }
    }
}

/// One live helper session per conversation.
struct ConversationSession {
    bridge: StandaloneBridge,
    session_open: bool,
    /// The inputs the session was last opened with; used to reopen when they
    /// change. `None` while the session is not open yet.
    fingerprint: Option<SessionFingerprint>,
    /// Pi session file the helper is using, so a reopen keeps the transcript.
    session_file: Option<PathBuf>,
    /// Exchange id of the most recently started exchange. A stop cancels that
    /// exchange specifically, so a prompt that starts in the meantime (for
    /// example the one "send now" queues) cannot be cancelled by a stale stop.
    last_exchange_id: Option<String>,
}

fn sessions() -> &'static Mutex<HashMap<String, Arc<Mutex<ConversationSession>>>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, Arc<Mutex<ConversationSession>>>>> =
        OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn session_for_id(conversation_id: &str) -> Option<Arc<Mutex<ConversationSession>>> {
    let sessions = sessions().lock().await;
    sessions.get(conversation_id).map(Arc::clone)
}

/// Cancel the running Pi turn for a conversation.
///
/// Stop and "send now" use this when the paused exchange left no live response
/// stream to cancel: without it the bridge never sees `turn.cancel` and the next
/// prompt waits behind the parked approval until the pending deadline. The
/// returned future must be spawned on the app's background executor.
pub fn cancel_active_turn(
    conversation_id: &str,
) -> impl std::future::Future<Output = ()> + Send + use<> {
    #[cfg(test)]
    record_cancel_call(conversation_id);
    let conversation_id = conversation_id.to_string();
    async move {
        let Some(session) = session_for_id(&conversation_id).await else {
            return;
        };
        let mut guard = session.lock().await;
        let exchange_id = guard.last_exchange_id.clone();
        if let Err(error) = guard
            .bridge
            .cancel_turn(&conversation_id, exchange_id.as_deref())
            .await
        {
            log::warn!("standalone: could not cancel the turn for {conversation_id}: {error}");
        }
    }
}

/// Record that the user rejected one tool call on an approval card. The bridge
/// answers it with a `Rejected` tool result the next time the turn resumes,
/// instead of telling the model its call was "cancelled by Warp". The returned
/// future must be spawned on the app's background executor.
pub fn record_user_denial(
    conversation_id: &str,
    tool_call_id: &str,
) -> impl std::future::Future<Output = ()> + Send + use<> {
    let conversation_id = conversation_id.to_string();
    let tool_call_id = tool_call_id.to_string();
    async move {
        let Some(session) = session_for_id(&conversation_id).await else {
            return;
        };
        let mut guard = session.lock().await;
        if let Err(error) = guard
            .bridge
            .deny_tool_call(&conversation_id, &tool_call_id)
            .await
        {
            log::warn!("standalone: could not record the rejection for {tool_call_id}: {error}");
        }
    }
}

/// A standalone turn that ended while no exchange stream was attached. The UI
/// uses it to withdraw approval cards and other work still waiting on the turn.
#[derive(Debug, Clone)]
pub struct TurnDeath {
    pub conversation_id: String,
    pub code: String,
    pub message: String,
}

fn turn_deaths() -> &'static async_channel::Sender<TurnDeath> {
    static DEATHS: OnceLock<async_channel::Sender<TurnDeath>> = OnceLock::new();
    DEATHS.get_or_init(|| {
        let (tx, rx) = async_channel::unbounded();
        // The single receiver is handed out by `subscribe_turn_deaths`; keep it
        // alive here so senders never fail.
        let _ = TURN_DEATH_RX.set(rx);
        tx
    })
}

static TURN_DEATH_RX: OnceLock<async_channel::Receiver<TurnDeath>> = OnceLock::new();

/// Subscribe to turns that ended with no exchange stream. Callers filter by
/// conversation id.
pub fn subscribe_turn_deaths() -> async_channel::Receiver<TurnDeath> {
    let _ = turn_deaths();
    TURN_DEATH_RX
        .get()
        .cloned()
        .expect("turn death receiver is initialized with the sender")
}

fn turn_death_from_event(conversation_id: &str, event: &BridgeEvent) -> Option<TurnDeath> {
    let (code, message) = match event {
        BridgeEvent::RunFailed { code, message, .. } => (code.clone(), message.clone()),
        BridgeEvent::ProtocolError { code, message } => (code.clone(), message.clone()),
        BridgeEvent::RunCancelled { reason } => ("cancelled".to_string(), reason.clone()),
        _ => return None,
    };
    Some(TurnDeath {
        conversation_id: conversation_id.to_string(),
        code,
        message,
    })
}

impl ConversationSession {
    /// Open the helper session (or reopen it when the request inputs changed).
    /// A reopen is skipped while a turn or a queued prompt is live in the
    /// bridge; the old endpoint serves that turn and the next fresh prompt
    /// retries the reopen.
    async fn ensure_open(
        &mut self,
        config: &StandaloneRequestConfig,
        task_id: &str,
        fingerprint: &SessionFingerprint,
        create_task: bool,
    ) -> anyhow::Result<()> {
        let changed = self
            .fingerprint
            .as_ref()
            .is_some_and(|current| current != fingerprint);
        if self.session_open && !changed {
            return Ok(());
        }
        let spec = SessionSpec {
            conversation_id: config.conversation_id.clone(),
            working_dir: config.working_dir.clone(),
            provider: config.profile.clone(),
            api_key: config.api_key.clone(),
            session_file: config
                .session_file
                .clone()
                .or_else(|| self.session_file.clone()),
            system_prompt: config.system_prompt.clone(),
            load_context_files: config.load_context_files,
            max_context_file_bytes: config.max_context_file_bytes,
            data_dir: config.data_dir.clone(),
            task_id: Some(task_id.to_string()),
            create_task: create_task && !self.session_open,
            subagents: config.subagents.clone(),
        };
        match self.bridge.open_session(spec).await {
            Ok(opened) => {
                if let Some(session_file) = opened.session_file.as_deref() {
                    remember_session(&config.conversation_id, session_file);
                    self.session_file = Some(PathBuf::from(session_file));
                }
                self.session_open = true;
                self.fingerprint = Some(fingerprint.clone());
                Ok(())
            }
            Err(BridgeError::SessionBusy(_)) => {
                // A turn (or queued prompt) is still live: keep the endpoint it
                // started on and retry the reopen on the next fresh prompt.
                log::info!(
                    "standalone: session for {} is busy; keeping the current endpoint",
                    config.conversation_id
                );
                Ok(())
            }
            Err(error) => Err(anyhow!(error.to_string())),
        }
    }
}

async fn session_for(
    config: &StandaloneRequestConfig,
) -> anyhow::Result<Arc<Mutex<ConversationSession>>> {
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
        timeouts: BridgeTimeouts::default(),
    })
    .await?;
    let hello = bridge.hello().await?;
    log::info!(
        "standalone: helper {} (node {}) ready with tools {:?}",
        hello.helper_version,
        hello.node_version,
        hello.capabilities.brokered_tools
    );
    if let Some(mut orphan_events) = bridge.take_orphan_events() {
        tokio::spawn(async move {
            while let Some(OrphanEvent {
                conversation_id,
                event,
            }) = orphan_events.recv().await
            {
                if let Some(death) = turn_death_from_event(&conversation_id, &event) {
                    let _ = turn_deaths().send(death).await;
                }
            }
        });
    }
    let session = Arc::new(Mutex::new(ConversationSession {
        bridge,
        session_open: false,
        fingerprint: None,
        session_file: None,
        last_exchange_id: None,
    }));
    sessions.insert(config.conversation_id.clone(), Arc::clone(&session));
    Ok(session)
}

/// Synchronous, best-effort teardown for `on_will_terminate`: sends the
/// cooperative shutdown frame and drops the bridge, which closes the helper's
/// stdin. The helper also exits on stdin EOF if the frame never lands.
pub fn shutdown_all_blocking() {
    let Ok(mut sessions) = sessions().try_lock() else {
        return;
    };
    for (_, session) in sessions.drain() {
        if let Ok(mut guard) = session.try_lock() {
            guard.bridge.shutdown_now();
        }
    }
}

#[cfg(test)]
fn recorded_cancel_calls() -> &'static std::sync::Mutex<Vec<String>> {
    static CALLS: OnceLock<std::sync::Mutex<Vec<String>>> = OnceLock::new();
    CALLS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

#[cfg(test)]
fn record_cancel_call(conversation_id: &str) {
    recorded_cancel_calls()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(conversation_id.to_string());
}

/// Test hook: take the conversation ids `cancel_active_turn` has been called
/// with since the last take.
#[cfg(test)]
pub(crate) fn take_recorded_cancel_calls() -> Vec<String> {
    let mut calls = recorded_cancel_calls()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::mem::take(&mut *calls)
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
    let inputs = extract_request_inputs(&request)
        .map_err(|error| AIApiError::Other(anyhow!(error.to_string())))?;
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

/// Per-message facts carried by [`BridgeEvent::MessageUsage`].
struct MessageUsageFacts<'a> {
    message_id: &'a str,
    model_id: &'a str,
    usage: &'a AgentUsage,
    duration_ms: u64,
    first_token_ms: Option<u64>,
    stop_reason: &'a str,
}

/// Compaction facts carried by [`BridgeEvent::CompactionFinished`].
struct CompactionFacts<'a> {
    reason: &'a str,
    summarized: bool,
    tokens_before: Option<u64>,
    tokens_after: Option<u64>,
    summary_usage: Option<&'a AgentUsage>,
    duration_ms: Option<u64>,
}

/// Child `task` facts carried by [`BridgeEvent::TaskCompleted`].
struct SubagentUsageFacts<'a> {
    status: &'a str,
    usage: &'a AgentUsage,
    wall_ms: u64,
}

/// Map a helper usage payload onto the durable ledger counters. `reasoning` is
/// a reported subset of `output` (Pi SDK semantics) and is never billed on top
/// of it.
fn ledger_counters(usage: &AgentUsage) -> UsageCounters {
    UsageCounters {
        input: usage.input_tokens,
        output: usage.output_tokens,
        cache_read: usage.cache_read_tokens.unwrap_or_default(),
        cache_write: usage.cache_write_tokens.unwrap_or_default(),
        reasoning: usage.reasoning_tokens.unwrap_or_default(),
        total: usage.total_tokens.unwrap_or_default(),
    }
}

fn ledger_context_source(source: &str) -> ContextSource {
    match source {
        "estimate" => ContextSource::Estimate,
        "compaction_estimate" => ContextSource::CompactionEstimate,
        _ => ContextSource::Usage,
    }
}

/// One live `assistant.usage` fact as a ledger record, priced against the
/// active profile. `context` is the reading that was current when the call
/// started; a missing pricing row leaves `cost` as `None`.
fn live_usage_record(
    config: &StandaloneRequestConfig,
    exchange_id: &str,
    facts: MessageUsageFacts<'_>,
    context: Option<ContextUsage>,
) -> UsageRecord {
    let model_id = if facts.model_id.is_empty() {
        config.profile.model_id.clone()
    } else {
        facts.model_id.to_string()
    };
    let mut record = UsageRecord {
        v: LEDGER_VERSION,
        ts: now_rfc3339(),
        source: RecordSource::Live,
        conversation_id: config.conversation_id.clone(),
        exchange_id: Some(exchange_id.to_string()),
        message_id: Some(facts.message_id.to_string()),
        profile_id: config.profile.id.clone(),
        provider_id: Some(format!("warpi-{}", config.profile.id)),
        model_id,
        stop_reason: Some(facts.stop_reason.to_string()),
        category: RecordCategory::PrimaryAgent,
        usage: ledger_counters(facts.usage),
        timing: Some(UsageTiming {
            generation_ms: Some(facts.duration_ms),
            first_token_ms: facts.first_token_ms,
            wall_ms: None,
        }),
        cost: None,
        context,
        compaction: None,
        session_file: None,
    };
    record.apply_pricing(&config.profile.pricing);
    record
}

/// One completed compaction as a ledger record. Only a reported summary usage
/// is priced: without one the cost stays unknown rather than a false $0.
fn compaction_usage_record(
    config: &StandaloneRequestConfig,
    exchange_id: &str,
    facts: CompactionFacts<'_>,
) -> UsageRecord {
    let mut record = UsageRecord {
        v: LEDGER_VERSION,
        ts: now_rfc3339(),
        source: RecordSource::Live,
        conversation_id: config.conversation_id.clone(),
        exchange_id: Some(exchange_id.to_string()),
        message_id: None,
        profile_id: config.profile.id.clone(),
        provider_id: Some(format!("warpi-{}", config.profile.id)),
        model_id: config.profile.model_id.clone(),
        stop_reason: None,
        category: RecordCategory::Compaction,
        usage: facts.summary_usage.map(ledger_counters).unwrap_or_default(),
        timing: None,
        cost: None,
        context: None,
        compaction: Some(CompactionRecord {
            reason: facts.reason.to_string(),
            summarized: facts.summarized,
            tokens_before: facts.tokens_before,
            tokens_after: facts.tokens_after,
            summary_usage: facts.summary_usage.map(ledger_counters),
            duration_ms: facts.duration_ms,
        }),
        session_file: None,
    };
    if facts.summary_usage.is_some() {
        record.apply_pricing(&config.profile.pricing);
    }
    record
}

/// One completed child `task` run as a ledger record. The child's usage is
/// priced under [`RecordCategory::Subagent`] so it stays separable from the
/// parent and is never folded into (or double-counted with) it.
fn subagent_usage_record(
    config: &StandaloneRequestConfig,
    exchange_id: &str,
    facts: SubagentUsageFacts<'_>,
) -> UsageRecord {
    let mut record = UsageRecord {
        v: LEDGER_VERSION,
        ts: now_rfc3339(),
        source: RecordSource::Live,
        conversation_id: config.conversation_id.clone(),
        exchange_id: Some(exchange_id.to_string()),
        message_id: None,
        profile_id: config.profile.id.clone(),
        provider_id: Some(format!("warpi-{}", config.profile.id)),
        model_id: config.profile.model_id.clone(),
        stop_reason: Some(facts.status.to_string()),
        category: RecordCategory::Subagent,
        usage: ledger_counters(facts.usage),
        timing: Some(UsageTiming {
            generation_ms: None,
            first_token_ms: None,
            wall_ms: Some(facts.wall_ms),
        }),
        cost: None,
        context: None,
        compaction: None,
        session_file: None,
    };
    record.apply_pricing(&config.profile.pricing);
    record
}

/// Append the ledger records for the usage-bearing bridge events of one
/// exchange. The ledger is best-effort: a write failure is logged and can never
/// fail the turn.
fn record_usage_event(
    ledger: &UsageLedger,
    config: &StandaloneRequestConfig,
    request_id: &str,
    exchange_id: &str,
    event: &BridgeEvent,
    last_context: &mut Option<ContextUsage>,
) {
    match event {
        BridgeEvent::MessageUsage {
            message_id,
            model_id,
            usage,
            duration_ms,
            first_token_ms,
            output_tokens_per_second: _,
            stop_reason,
        } => {
            let record = live_usage_record(
                config,
                exchange_id,
                MessageUsageFacts {
                    message_id,
                    model_id,
                    usage,
                    duration_ms: *duration_ms,
                    first_token_ms: *first_token_ms,
                    stop_reason,
                },
                *last_context,
            );
            usage_model::record_message_usage(
                request_id,
                usage_model::StandaloneUsageEntry::from_ledger_record(&record),
            );
            ledger.append_best_effort(&record);
        }
        BridgeEvent::ContextUpdated {
            tokens,
            context_window,
            percent,
            source,
        } => {
            *last_context = Some(ContextUsage {
                tokens: *tokens,
                context_window: *context_window,
                percent: *percent,
                source: ledger_context_source(source),
            });
            usage_model::record_context(
                &config.conversation_id,
                usage_model::StandaloneContextReading {
                    tokens: *tokens,
                    context_window: *context_window,
                    percent: *percent,
                    source: Some(ledger_context_source(source)),
                },
            );
        }
        BridgeEvent::CompactionFinished {
            reason,
            summarized,
            tokens_before,
            tokens_after,
            summary_usage,
            duration_ms,
        } => {
            let record = compaction_usage_record(
                config,
                exchange_id,
                CompactionFacts {
                    reason,
                    summarized: *summarized,
                    tokens_before: *tokens_before,
                    tokens_after: *tokens_after,
                    summary_usage: summary_usage.as_ref(),
                    duration_ms: *duration_ms,
                },
            );
            ledger.append_best_effort(&record);
        }
        BridgeEvent::TaskCompleted {
            usage,
            status,
            wall_ms,
            ..
        } => {
            // A task that failed before any model call reports zero usage; it
            // is not spend, and recording it would add a false $0 line.
            if !ledger_counters(usage).is_zero() {
                let record = subagent_usage_record(
                    config,
                    exchange_id,
                    SubagentUsageFacts {
                        status: status.as_str(),
                        usage,
                        wall_ms: *wall_ms,
                    },
                );
                ledger.append_best_effort(&record);
            }
        }
        _ => {}
    }
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
    let fingerprint = SessionFingerprint::from_request(&config);
    // A resume must keep the endpoint the turn started on; only a fresh prompt
    // may reopen the session with changed inputs.
    if inputs.tool_results.is_empty() || !guard.session_open {
        guard
            .ensure_open(&config, &task_id, &fingerprint, inputs.task_id.is_empty())
            .await?;
    }
    let request_id = uuid::Uuid::new_v4().to_string();
    let run_id = inputs.conversation_id.clone();

    let exchange = if !inputs.tool_results.is_empty() {
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
        return Err(anyhow!(
            "standalone request has neither a user query nor tool results"
        ));
    };
    // The exchange id makes cancellation target this exact exchange: a queued
    // prompt that has not started yet, or the running turn it belongs to. A
    // stale cancel after the exchange settled is a no-op in the bridge. A
    // queued prompt keeps the running turn's id: a stop still cancels the turn
    // it was pressed for, not the prompt waiting behind it.
    let exchange_id = exchange.exchange_id.clone();
    if !exchange.queued {
        guard.last_exchange_id = Some(exchange_id.clone());
    }
    let mut stream = exchange.stream;
    drop(guard);

    let mut writer = ExchangeWriter::new(task_id, &inputs, request_id.clone(), run_id);
    let ledger = UsageLedger::for_data_dir(StandaloneConfig::data_dir());
    let mut last_context: Option<ContextUsage> = None;
    // Cancel the Pi turn if the UI cancels the request. `lock` (not `try_lock`)
    // so a cancel racing a follow-up submission is still delivered; the
    // exchange id keeps it from cancelling the wrong turn.
    let cancel_exchange_id = exchange_id.clone();
    let mut cancel_handle = tokio::spawn({
        let session = Arc::clone(&session);
        let conversation_id = config.conversation_id.clone();
        async move {
            if cancellation_rx.await.is_ok() {
                let mut guard = session.lock().await;
                let _ = guard
                    .bridge
                    .cancel_turn(&conversation_id, Some(&cancel_exchange_id))
                    .await;
            }
        }
    });

    while let Some(event) = stream.recv().await {
        if matches!(event, BridgeEvent::Init { .. }) {
            // A queued prompt starts on this exchange only when its Init
            // arrives (the request was accepted long before). Record it now so
            // a stop targets the turn that is actually running.
            let mut guard = session.lock().await;
            guard.last_exchange_id = Some(exchange_id.clone());
            drop(guard);
        }
        record_usage_event(
            &ledger,
            &config,
            &request_id,
            &exchange_id,
            &event,
            &mut last_context,
        );
        for warp_event in writer.write(&event) {
            if tx.send(Ok(warp_event)).is_err() {
                // The app dropped the exchange. A cancel may already be in
                // flight; give it a bounded window to reach the bridge instead
                // of aborting it, so the helper is not left running a turn
                // nobody is watching. The bridge's cancel deadline settles the
                // turn either way.
                if tokio::time::timeout(CANCEL_DELIVERY_TIMEOUT, &mut cancel_handle)
                    .await
                    .is_err()
                {
                    cancel_handle.abort();
                }
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
    // The cancel task is deliberately not aborted: it exits on its own when the
    // response stream drops its cancellation sender. Keeping it alive through
    // the pause closes the window where the app cancels a just-paused exchange
    // before its stream is cleaned up (the bridge then still receives
    // `turn.cancel` for the paused exchange).
    drop(cancel_handle);
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

#[cfg(test)]
mod tests {
    use standalone_agent::usage_ledger::ModelPricing;

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
            pricing: Default::default(),
        };
        let mut config = StandaloneConfig {
            enabled: false,
            ..Default::default()
        };
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

    fn test_profile(model_id: &str) -> ProviderProfile {
        ProviderProfile {
            id: "p".into(),
            display_name: "P".into(),
            base_url: "http://127.0.0.1:9/v1".into(),
            wire: standalone_agent::provider::WireProtocol::OpenAiChatCompletions,
            model_id: model_id.into(),
            models: Vec::new(),
            disabled_models: Vec::new(),
            credential: standalone_agent::provider::CredentialRef::None,
            context_limit: 8192,
            output_limit: 1024,
            compat: Default::default(),
            reasoning: false,
            supports_image_input: false,
            headers: Default::default(),
            pricing: Default::default(),
        }
    }

    /// Regression: saving a provider profile must enable the local backend.
    /// The settings page used to re-apply a stale `enabled = false` after the
    /// upsert, so a fresh install with a configured profile and API key stayed
    /// on the Warp-account path and told the user to create an account.
    #[test]
    fn upserting_a_profile_enables_standalone_mode() {
        let config = apply_upsert(StandaloneConfig::default(), test_profile("m"));
        assert!(
            config.enabled,
            "configuring a provider is the opt-in to the local backend"
        );
        assert_eq!(config.active_profile, "p");
        assert_eq!(config.profiles.len(), 1);
        assert_eq!(config.active().map(|p| p.id.as_str()), Some("p"));
    }

    #[test]
    fn toggling_standalone_mode_keeps_configured_profiles() {
        let enabled = apply_upsert(StandaloneConfig::default(), test_profile("m"));
        let disabled = enabled.clone().with_enabled(false);
        assert!(!disabled.enabled);
        assert_eq!(disabled.profiles.len(), 1, "a disable keeps the profiles");
        assert!(disabled.active().is_some());
        let re_enabled = disabled.with_enabled(true);
        assert!(re_enabled.enabled);
        assert_eq!(re_enabled.active_profile, "p");
    }

    fn test_request_config(
        model_id: &str,
        working_dir: &str,
        api_key: Option<&str>,
    ) -> StandaloneRequestConfig {
        StandaloneRequestConfig {
            conversation_id: "conv-1".into(),
            working_dir: PathBuf::from(working_dir),
            profile: test_profile(model_id),
            helper_entry: PathBuf::from("/helper.js"),
            helper_executable: PathBuf::from("node"),
            data_dir: PathBuf::from("/tmp/warpi-test"),
            session_file: None,
            system_prompt: None,
            load_context_files: false,
            max_context_file_bytes: 4096,
            api_key: api_key.map(SecretString::new),
            subagents: None,
        }
    }

    #[test]
    fn a_changed_model_or_cwd_changes_the_session_fingerprint() {
        let base = test_request_config("model-a", "/work", None);
        let baseline = SessionFingerprint::from_request(&base);
        assert_eq!(baseline, SessionFingerprint::from_request(&base));

        let other_model = test_request_config("model-b", "/work", None);
        assert_ne!(baseline, SessionFingerprint::from_request(&other_model));

        let other_cwd = test_request_config("model-a", "/elsewhere", None);
        assert_ne!(baseline, SessionFingerprint::from_request(&other_cwd));

        let with_key = test_request_config("model-a", "/work", Some("sk-one"));
        assert_ne!(baseline, SessionFingerprint::from_request(&with_key));

        let rotated = test_request_config("model-a", "/work", Some("sk-two"));
        assert_ne!(
            SessionFingerprint::from_request(&with_key),
            SessionFingerprint::from_request(&rotated),
            "a rotated credential must reopen the session"
        );
    }

    #[test]
    fn concurrent_session_map_updates_keep_both_entries() {
        let dir =
            std::env::temp_dir().join(format!("warpi-session-map-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("session-map.json");

        std::thread::scope(|scope| {
            let barrier = Arc::new(std::sync::Barrier::new(2));
            for (conversation, session_file) in [("c1", "s1"), ("c2", "s2")] {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    update_session_map_at(&path, |map| {
                        map.conversations
                            .insert(conversation.to_string(), session_file.to_string());
                    });
                });
            }
        });

        let map = read_session_map_from(&path);
        assert_eq!(map.conversations.get("c1").map(String::as_str), Some("s1"));
        assert_eq!(map.conversations.get("c2").map(String::as_str), Some("s2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_map_write_is_atomic() {
        let dir =
            std::env::temp_dir().join(format!("warpi-session-map-atomic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("session-map.json");
        write_session_map_to(
            &path,
            &SessionMap {
                conversations: HashMap::from([("c1".to_string(), "s1".to_string())]),
                task_ids: HashMap::new(),
            },
        );
        write_session_map_to(
            &path,
            &SessionMap {
                conversations: HashMap::from([("c2".to_string(), "s2".to_string())]),
                task_ids: HashMap::new(),
            },
        );
        let map = read_session_map_from(&path);
        assert_eq!(map.conversations.len(), 1);
        assert_eq!(map.conversations.get("c2").map(String::as_str), Some("s2"));
        assert!(
            !path.with_extension("json.tmp").exists(),
            "the temporary file must not survive a successful rename"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn turn_death_events_are_mapped_for_the_ui() {
        let death = turn_death_from_event(
            "conv-1",
            &BridgeEvent::RunFailed {
                code: "tool_timeout".to_string(),
                message: "no result".to_string(),
                retryable: true,
            },
        )
        .expect("a failed turn is a death");
        assert_eq!(death.conversation_id, "conv-1");
        assert_eq!(death.code, "tool_timeout");
        assert_eq!(death.message, "no result");

        assert!(
            turn_death_from_event(
                "conv-1",
                &BridgeEvent::TextDelta {
                    message_id: "m".to_string(),
                    delta: "x".to_string(),
                }
            )
            .is_none(),
            "prose is not a turn death"
        );
    }

    #[test]
    fn publishing_a_turn_death_reaches_subscribers() {
        let receiver = subscribe_turn_deaths();
        let death = TurnDeath {
            conversation_id: "conv-publish-test".to_string(),
            code: "tool_timeout".to_string(),
            message: "expired".to_string(),
        };
        turn_deaths()
            .try_send(death)
            .expect("the channel has a receiver");
        let received = receiver.try_recv().expect("the death is delivered");
        assert_eq!(received.conversation_id, "conv-publish-test");
        assert_eq!(received.code, "tool_timeout");
    }

    fn priced_config(model_id: &str) -> StandaloneRequestConfig {
        let mut config = test_request_config(model_id, "/work", None);
        config.profile.pricing.insert(
            model_id.to_string(),
            ModelPricing {
                input: 1.0,
                output: 2.0,
                cache_read: Some(0.5),
                cache_write: None,
            },
        );
        config
    }

    fn sample_usage() -> AgentUsage {
        AgentUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_tokens: Some(1_000_000),
            cache_write_tokens: Some(1_000_000),
            reasoning_tokens: Some(500_000),
            total_tokens: Some(4_000_000),
            ..Default::default()
        }
    }

    #[test]
    fn live_usage_records_price_the_profile_table_and_keep_reasoning_unbilled() {
        let config = priced_config("model-a");
        let record = live_usage_record(
            &config,
            "exchange-1",
            MessageUsageFacts {
                message_id: "m1",
                model_id: "model-a",
                usage: &sample_usage(),
                duration_ms: 812,
                first_token_ms: Some(210),
                stop_reason: "stop",
            },
            Some(ContextUsage {
                tokens: Some(7_937),
                context_window: Some(131_072),
                percent: Some(6.06),
                source: ContextSource::Usage,
            }),
        );

        assert_eq!(record.category, RecordCategory::PrimaryAgent);
        assert_eq!(record.source, RecordSource::Live);
        assert_eq!(record.usage.reasoning, 500_000);
        assert_eq!(record.usage.total_tokens(), 4_000_000);
        assert_eq!(record.exchange_id.as_deref(), Some("exchange-1"));
        assert_eq!(record.profile_id, "p");
        assert_eq!(record.provider_id.as_deref(), Some("warpi-p"));
        let cost = record.cost.expect("the model has a pricing row");
        assert!(cost.estimated);
        // input $1 + output $2 + cache_read $0.5 + cache_write $1 (a missing
        // cache-write rate falls back to the input rate); reasoning is a
        // reported subset of output and is not billed again.
        assert!((cost.total_usd - 4.5).abs() < 1e-9, "{}", cost.total_usd);
        assert_eq!(record.timing.expect("timing").first_token_ms, Some(210));
        assert_eq!(record.context.expect("context").tokens, Some(7_937));
    }

    #[test]
    fn live_usage_records_without_a_pricing_row_stay_cost_unknown() {
        let config = test_request_config("unpriced-model", "/work", None);
        let record = live_usage_record(
            &config,
            "e",
            MessageUsageFacts {
                message_id: "m",
                model_id: "unpriced-model",
                usage: &sample_usage(),
                duration_ms: 1,
                first_token_ms: None,
                stop_reason: "stop",
            },
            None,
        );
        assert!(record.cost.is_none(), "no row means cost unknown, not $0");
        assert_eq!(record.model_id, "unpriced-model");
    }

    #[test]
    fn compaction_records_are_split_and_priced_from_the_summary_usage() {
        let mut config = priced_config("model-a");
        let usage = AgentUsage {
            input_tokens: 90_000,
            output_tokens: 1_000,
            ..Default::default()
        };
        let record = compaction_usage_record(
            &config,
            "e2",
            CompactionFacts {
                reason: "threshold",
                summarized: true,
                tokens_before: Some(90_000),
                tokens_after: Some(12_000),
                summary_usage: Some(&usage),
                duration_ms: Some(4_200),
            },
        );
        assert_eq!(record.category, RecordCategory::Compaction);
        assert_eq!(record.usage.total_tokens(), 91_000);
        let cost = record.cost.expect("summary usage with a priced model");
        assert!((cost.total_usd - 0.092).abs() < 1e-9, "{}", cost.total_usd);
        let compaction = record.compaction.expect("compaction facts");
        assert_eq!(compaction.tokens_before, Some(90_000));
        assert_eq!(compaction.tokens_after, Some(12_000));
        assert_eq!(compaction.duration_ms, Some(4_200));

        // No reported summary usage: the facts are kept and cost stays unknown.
        let no_usage = compaction_usage_record(
            &config,
            "e3",
            CompactionFacts {
                reason: "manual",
                summarized: false,
                tokens_before: None,
                tokens_after: None,
                summary_usage: None,
                duration_ms: None,
            },
        );
        assert!(no_usage.cost.is_none());
        assert!(no_usage.usage.is_zero());
        assert!(no_usage.compaction.is_some());

        // A priced model with no row keeps cost unknown instead of $0.
        config.profile.pricing.clear();
        let unknown_model = compaction_usage_record(
            &config,
            "e4",
            CompactionFacts {
                reason: "threshold",
                summarized: true,
                tokens_before: None,
                tokens_after: None,
                summary_usage: Some(&usage),
                duration_ms: Some(1),
            },
        );
        assert!(unknown_model.cost.is_none());
    }

    #[test]
    fn usage_events_append_one_ledger_record_per_call_and_track_context() {
        let dir = std::env::temp_dir().join(format!("warpi-usage-live-{}", uuid::Uuid::new_v4()));
        let ledger = UsageLedger::new(&dir);
        let config = priced_config("model-a");
        let usage = sample_usage();
        let mut last_context = None;

        record_usage_event(
            &ledger,
            &config,
            "r1",
            "e1",
            &BridgeEvent::MessageUsage {
                message_id: "m1".to_string(),
                model_id: "model-a".to_string(),
                usage: usage.clone(),
                duration_ms: 10,
                first_token_ms: Some(2),
                output_tokens_per_second: Some(3.0),
                stop_reason: "stop".to_string(),
            },
            &mut last_context,
        );
        record_usage_event(
            &ledger,
            &config,
            "r1",
            "e1",
            &BridgeEvent::ContextUpdated {
                tokens: Some(7_937),
                context_window: Some(131_072),
                percent: Some(6.06),
                source: "compaction_estimate".to_string(),
            },
            &mut last_context,
        );
        record_usage_event(
            &ledger,
            &config,
            "r1",
            "e1",
            &BridgeEvent::CompactionFinished {
                reason: "threshold".to_string(),
                summarized: true,
                tokens_before: Some(9_000),
                tokens_after: Some(2_000),
                summary_usage: Some(usage),
                duration_ms: Some(400),
            },
            &mut last_context,
        );
        // The context reading is carried onto the next call's record.
        record_usage_event(
            &ledger,
            &config,
            "r1",
            "e1",
            &BridgeEvent::MessageUsage {
                message_id: "m2".to_string(),
                model_id: "model-a".to_string(),
                usage: sample_usage(),
                duration_ms: 5,
                first_token_ms: None,
                output_tokens_per_second: None,
                stop_reason: "stop".to_string(),
            },
            &mut last_context,
        );

        let records = ledger.read_records().expect("ledger readable");
        assert_eq!(records.len(), 3, "two messages and one compaction");
        assert_eq!(records[0].category, RecordCategory::PrimaryAgent);
        assert_eq!(records[0].message_id.as_deref(), Some("m1"));
        assert!(
            records[0].context.is_none(),
            "no reading before the first call"
        );
        assert_eq!(records[1].category, RecordCategory::Compaction);
        assert_eq!(
            records[1].compaction.as_ref().map(|c| c.reason.as_str()),
            Some("threshold")
        );
        let context = records[2].context.expect("context carried forward");
        assert_eq!(context.tokens, Some(7_937));
        assert_eq!(context.source, ContextSource::CompactionEstimate);
        assert!((context.percent.unwrap_or_default() - 6.06).abs() < 1e-9);

        // The same events feed the live UI model: per-request usage accumulates
        // and the conversation keeps the latest context reading.
        let entry =
            usage_model::entry_for_request("r1").expect("the usage model receives the call");
        assert_eq!(entry.output_tokens, 2_000_000, "two messages accumulate");
        assert_eq!(entry.reasoning_tokens, Some(1_000_000));
        assert_eq!(entry.generation_ms, Some(15));
        let reading = usage_model::context_for_conversation("conv-1").expect("context recorded");
        assert_eq!(reading.percent_used(), Some(6.06));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn child_task_usage_is_a_separate_subagent_record() {
        let dir = std::env::temp_dir().join(format!("warpi-usage-child-{}", uuid::Uuid::new_v4()));
        let ledger = UsageLedger::new(&dir);
        let config = priced_config("model-a");
        let mut last_context = None;

        record_usage_event(
            &ledger,
            &config,
            "r-child",
            "e1",
            &BridgeEvent::MessageUsage {
                message_id: "m1".to_string(),
                model_id: "model-a".to_string(),
                usage: sample_usage(),
                duration_ms: 10,
                first_token_ms: None,
                output_tokens_per_second: None,
                stop_reason: "stop".to_string(),
            },
            &mut last_context,
        );
        record_usage_event(
            &ledger,
            &config,
            "r-child",
            "e1",
            &BridgeEvent::TaskCompleted {
                task_id: "task-1".to_string(),
                child_session_id: "conv-1:task-1".to_string(),
                status: "ok".to_string(),
                reason: None,
                subagent_type: "explore".to_string(),
                turns: 3,
                tool_calls: 4,
                usage: AgentUsage {
                    input_tokens: 400,
                    output_tokens: 100,
                    total_tokens: Some(500),
                    ..Default::default()
                },
                wall_ms: 2_000,
                summary_bytes: 512,
            },
            &mut last_context,
        );
        // A task that failed before any model call reports zero usage; it must
        // not add a false $0 ledger line.
        record_usage_event(
            &ledger,
            &config,
            "r-child",
            "e1",
            &BridgeEvent::TaskCompleted {
                task_id: "task-2".to_string(),
                child_session_id: "conv-1:task-2".to_string(),
                status: "error".to_string(),
                reason: Some("no model call".to_string()),
                subagent_type: "verify".to_string(),
                turns: 0,
                tool_calls: 0,
                usage: AgentUsage::default(),
                wall_ms: 5,
                summary_bytes: 10,
            },
            &mut last_context,
        );

        let records = ledger.read_records().expect("ledger readable");
        assert_eq!(records.len(), 2, "parent call plus one child task");
        assert_eq!(records[0].category, RecordCategory::PrimaryAgent);
        assert_eq!(records[1].category, RecordCategory::Subagent);
        assert_eq!(records[1].stop_reason.as_deref(), Some("ok"));
        assert_eq!(records[1].usage.input, 400);
        assert_eq!(
            records[1].timing.expect("child timing").wall_ms,
            Some(2_000)
        );
        assert!(
            (records[1].cost.expect("child is priced").total_usd - 0.0006).abs() < 1e-12,
            "400 in @ $1/M + 100 out @ $2/M"
        );

        let summary = standalone_agent::usage_ledger::summarize(records.iter());
        assert_eq!(summary.overall.primary.records, 1);
        assert_eq!(summary.overall.subagent.records, 1);
        assert_eq!(summary.overall.total().records, 2);
        assert_eq!(
            summary.overall.total().usage.input,
            records[0].usage.input + records[1].usage.input,
            "the split sums parent and child without double counting"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
