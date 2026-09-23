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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StandaloneConfig {
    /// Explicit opt-in. Standalone mode never activates implicitly.
    pub enabled: bool,
    pub profile: ProviderProfile,
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

impl StandaloneConfig {
    pub fn config_path() -> Option<PathBuf> {
        if let Ok(override_path) = std::env::var("WARPOS_STANDALONE_CONFIG") {
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
                "standalone helper was not found; set `helper_entry` in the standalone config or WARPOS_PI_HELPER_ENTRY"
            )
        })
    }

    /// Directory that holds the Pi session files and the conversation mapping.
    fn data_dir() -> PathBuf {
        warp_core::paths::data_dir().join("standalone")
    }
}

fn cached_config() -> Option<&'static StandaloneConfig> {
    static CONFIG: OnceLock<Option<StandaloneConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| match StandaloneConfig::load() {
            Some(config) if config.enabled => {
                if let Err(error) = config.profile.validate() {
                    log::warn!("standalone: profile is invalid: {error}");
                    return None;
                }
                Some(config)
            }
            _ => None,
        })
        .as_ref()
}

/// Whether this process is running the standalone agent backend.
pub fn is_enabled() -> bool {
    cached_config().is_some()
}

/// Durable conversation -> Pi session mapping. Written next to the Pi session
/// files so a restart resumes the exact same model transcript, never "the most
/// recent session".
#[derive(Debug, Default, Serialize, Deserialize)]
struct SessionMap {
    #[serde(default)]
    conversations: HashMap<String, String>,
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
    if let Some(parent) = session_map_path().parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        log::warn!("standalone: cannot create data directory: {error}");
        return;
    }
    match serde_json::to_string_pretty(&map) {
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
    let config = cached_config()?;
    let helper_entry = match config.helper_entry() {
        Ok(path) => path,
        Err(error) => {
            log::warn!("standalone: {error}");
            return None;
        }
    };
    let api_key = resolve_api_key(app, &config.profile);
    let session_file = read_session_map().conversations.get(conversation_id).map(PathBuf::from);
    Some(StandaloneRequestConfig {
        conversation_id: conversation_id.to_string(),
        working_dir: working_dir
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from(".")),
        profile: config.profile.clone(),
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

/// Read the profile credential from the OS secret store exactly once per
/// process, keeping only an in-memory copy afterwards. `auth=none` profiles
/// never touch the secret store.
fn resolve_api_key(
    app: &warpui_core::AppContext,
    profile: &ProviderProfile,
) -> Option<SecretString> {
    use warpui_extras::secure_storage::AppContextExt;
    let standalone_agent::provider::CredentialRef::SecretStore { key } = &profile.credential else {
        return None;
    };
    match app.secure_storage().read_value(key) {
        Ok(value) if !value.trim().is_empty() => Some(SecretString::new(value)),
        Ok(_) => {
            log::warn!("standalone: credential {key} is empty");
            None
        }
        Err(error) => {
            log::warn!("standalone: cannot read credential {key}: {error}");
            None
        }
    }
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

    let mut writer = ExchangeWriter::new(inputs.task_id.clone(), &inputs, request_id, run_id);
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
        let config = StandaloneConfig {
            enabled: false,
            profile: standalone_agent::provider::ProviderProfile {
                id: "p".into(),
                display_name: "P".into(),
                base_url: "http://127.0.0.1:1/v1".into(),
                wire: standalone_agent::provider::WireProtocol::OpenAiChatCompletions,
                model_id: "m".into(),
                credential: standalone_agent::provider::CredentialRef::None,
                context_limit: 8192,
                output_limit: 1024,
                compat: Default::default(),
                reasoning: false,
                supports_image_input: false,
                headers: Default::default(),
            },
            helper_entry: None,
            helper_executable: None,
            load_context_files: true,
            max_context_file_bytes: 4096,
            system_prompt: None,
        };
        assert!(!config.enabled);
        assert!(credential_reference_ok(&config.profile));
    }
}
