//! Test support: spawns the TypeScript fixture provider and the Rust bridge
//! around the real Pi helper.

// Every integration test binary includes this module but uses a different
// subset of it.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use standalone_agent::bridge::{
    BridgeConfig, BridgeEvent, BridgeTimeouts, SessionSpec, StandaloneBridge, TurnStream,
};
use standalone_agent::helper::HelperLaunchConfig;
use standalone_agent::protocol::{AgentUsage, HelperToolResultStatus};
use standalone_agent::provider::{CredentialRef, ProviderProfile, WireProtocol};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use warp_multi_agent_api as api;

pub const FIXTURE_STEPS_KEY: &str = "FIXTURE_STEPS";

/// Absolute path to the helper entry compiled by `npm run build`.
pub fn helper_entry() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .join("../../standalone/pi-helper/dist/main.js")
        .canonicalize()
        .expect("helper dist/main.js must exist; run `npm run build` in standalone/pi-helper")
}

pub fn pi_helper_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../standalone/pi-helper")
}

#[allow(clippy::disallowed_types)] // test-only probe: no console window exists under cargo test
pub fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Fixture provider running in a child Node process.
pub struct FixtureProvider {
    child: Child,
    captures_path: PathBuf,
    pub base_url: String,
}

impl FixtureProvider {
    /// `steps` is the JSON script understood by `FakeProvider`.
    pub async fn start(captures_path: &Path, steps: &serde_json::Value) -> Self {
        let dir = pi_helper_dir();
        let mut child = Command::new("node")
            .arg("--import")
            .arg("tsx")
            .arg("test/serve-fixture.ts")
            .arg("-")
            .arg(captures_path)
            .current_dir(&dir)
            .env("FIXTURE_STEPS", steps.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn fixture provider");
        let stdout = child.stdout.take().expect("fixture stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(30), reader.read_line(&mut line))
            .await
            .expect("fixture provider did not start in time")
            .expect("fixture provider stdout");
        let base_url = line
            .trim()
            .strip_prefix("LISTENING ")
            .expect("fixture provider should print LISTENING <url>")
            .to_string();
        Self {
            child,
            captures_path: captures_path.to_path_buf(),
            base_url,
        }
    }

    /// Captured request bodies and headers (written when the server stops).
    pub async fn captures(&mut self) -> serde_json::Value {
        self.stop().await;
        let raw =
            std::fs::read_to_string(&self.captures_path).expect("fixture captures written on stop");
        serde_json::from_str(&raw).expect("captures are valid JSON")
    }

    pub async fn stop(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        // Give the fixture a moment to flush the captures file.
        for _ in 0..100 {
            if self.captures_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for FixtureProvider {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

pub fn profile(base_url: &str, with_key: bool) -> ProviderProfile {
    ProviderProfile {
        id: "fixture".into(),
        display_name: "Fixture".into(),
        base_url: base_url.to_string(),
        wire: WireProtocol::OpenAiChatCompletions,
        model_id: "fixture-model".into(),
        models: Vec::new(),
        disabled_models: Vec::new(),
        credential: if with_key {
            CredentialRef::SecretStore {
                key: "warpi/fixture".into(),
            }
        } else {
            CredentialRef::None
        },
        context_limit: 32768,
        output_limit: 4096,
        compat: Default::default(),
        reasoning: false,
        supports_image_input: false,
        headers: Default::default(),
        pricing: Default::default(),
    }
}

pub async fn spawn_bridge(
    data_dir: &Path,
    provider: ProviderProfile,
    api_key: Option<&str>,
) -> StandaloneBridge {
    spawn_bridge_with_task(data_dir, provider, api_key, "conv-1", true).await
}

pub async fn spawn_bridge_with_task(
    data_dir: &Path,
    provider: ProviderProfile,
    api_key: Option<&str>,
    task_id: &str,
    create_task: bool,
) -> StandaloneBridge {
    let mut launch = HelperLaunchConfig::node(helper_entry(), data_dir);
    launch.shutdown_timeout = Duration::from_secs(3);
    let mut bridge = StandaloneBridge::spawn(BridgeConfig {
        launch,
        retry: standalone_agent::bridge::RetryOptions {
            enabled: false,
            max_retries: 0,
            base_delay_ms: 0,
        },
        compaction_enabled: true,
        timeouts: BridgeTimeouts::default(),
    })
    .await
    .expect("bridge spawns");
    let hello = bridge.hello().await.expect("hello handshake");
    assert_eq!(hello.capabilities.protocol, 1);
    let mut tools = hello.capabilities.brokered_tools.clone();
    tools.sort();
    assert_eq!(
        tools,
        vec![
            "bash",
            "bash_output",
            "edit",
            "glob",
            "grep",
            "read",
            "write"
        ]
    );
    bridge
        .open_session(SessionSpec {
            conversation_id: "conv-1".into(),
            working_dir: data_dir.to_path_buf(),
            provider,
            api_key: api_key.map(standalone_agent::SecretString::new),
            session_file: None,
            system_prompt: Some("You are a fixture agent.".into()),
            load_context_files: false,
            max_context_file_bytes: 4096,
            data_dir: data_dir.to_path_buf(),
            task_id: Some(task_id.to_string()),
            create_task,
            subagents: None,
        })
        .await
        .expect("session opens");
    bridge
}

/// Collect events until `done` returns true, or time out.
pub async fn collect_until(
    stream: &mut TurnStream,
    timeout: Duration,
    mut done: impl FnMut(&BridgeEvent) -> bool,
) -> Vec<BridgeEvent> {
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("timed out collecting bridge events; saw: {events:?}");
        }
        match tokio::time::timeout(remaining, stream.recv()).await {
            Ok(Some(event)) => {
                let finished = done(&event);
                events.push(event);
                if finished {
                    return events;
                }
            }
            Ok(None) => panic!("bridge stream closed early; saw: {events:?}"),
            Err(_) => panic!("timed out collecting bridge events; saw: {events:?}"),
        }
    }
}

pub fn tool_calls(events: &[BridgeEvent]) -> Vec<api::message::ToolCall> {
    events
        .iter()
        .find_map(|event| match event {
            BridgeEvent::ToolCalls { calls } => Some(calls.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Shell command of a translated `RunShellCommand` call, if that is what it is.
#[allow(deprecated)]
pub fn shell_command(call: &api::message::ToolCall) -> Option<&str> {
    match call.tool.as_ref()? {
        api::message::tool_call::Tool::RunShellCommand(shell) => Some(&shell.command),
        _ => None,
    }
}

/// Spawn a bridge around the scripted stub helper instead of the real Pi
/// helper. The stub implements just enough of the protocol to drive failure
/// paths the Pi SDK cannot be forced into: ignored cancels, untranslatable
/// calls, and tool results that never arrive.
pub async fn spawn_stub_bridge(
    data_dir: &Path,
    mode: &str,
    capture_path: &Path,
    timeouts: BridgeTimeouts,
) -> StandaloneBridge {
    spawn_stub_bridge_with(data_dir, mode, capture_path, timeouts, &[]).await
}

pub async fn spawn_stub_bridge_with(
    data_dir: &Path,
    mode: &str,
    capture_path: &Path,
    timeouts: BridgeTimeouts,
    extra_env: &[(&str, &str)],
) -> StandaloneBridge {
    spawn_stub_bridge_full(data_dir, mode, capture_path, timeouts, extra_env, None).await
}

/// Stub bridge whose session opens with an explicit subagents config. The stub
/// advertises the capability, so the payload is forwarded exactly as a capable
/// real helper would receive it.
pub async fn spawn_stub_bridge_with_subagents(
    data_dir: &Path,
    mode: &str,
    capture_path: &Path,
    timeouts: BridgeTimeouts,
    subagents: Option<standalone_agent::protocol::HelperSubagents>,
) -> StandaloneBridge {
    spawn_stub_bridge_full(data_dir, mode, capture_path, timeouts, &[], subagents).await
}

async fn spawn_stub_bridge_full(
    data_dir: &Path,
    mode: &str,
    capture_path: &Path,
    timeouts: BridgeTimeouts,
    extra_env: &[(&str, &str)],
    subagents: Option<standalone_agent::protocol::HelperSubagents>,
) -> StandaloneBridge {
    let mut launch = HelperLaunchConfig::node(stub_helper_entry(), data_dir);
    launch.extra_env = vec![
        ("WARPI_STUB_MODE".to_string(), mode.to_string()),
        (
            "WARPI_STUB_CAPTURE".to_string(),
            capture_path.to_string_lossy().into_owned(),
        ),
    ];
    launch.extra_env.extend(
        extra_env
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string())),
    );
    launch.shutdown_timeout = Duration::from_secs(3);
    let mut bridge = StandaloneBridge::spawn(BridgeConfig {
        launch,
        retry: standalone_agent::bridge::RetryOptions {
            enabled: false,
            max_retries: 0,
            base_delay_ms: 0,
        },
        compaction_enabled: true,
        timeouts,
    })
    .await
    .expect("stub bridge spawns");
    let hello = bridge.hello().await.expect("hello handshake");
    assert_eq!(hello.capabilities.protocol, 1);
    bridge
        .open_session(SessionSpec {
            conversation_id: "conv-1".into(),
            working_dir: data_dir.to_path_buf(),
            provider: profile("http://127.0.0.1:1/v1", false),
            api_key: None,
            session_file: None,
            system_prompt: None,
            load_context_files: false,
            max_context_file_bytes: 4096,
            data_dir: data_dir.to_path_buf(),
            task_id: Some("conv-1".into()),
            create_task: true,
            subagents,
        })
        .await
        .expect("session opens");
    bridge
}

pub fn stub_helper_entry() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/stub-helper.mjs")
}

/// Frames captured by the stub helper, one JSON value per line.
pub fn stub_capture(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Poll a stub capture file until `done` accepts its frames. The stub writes
/// from a child process, so a frame can land just after the bridge event that
/// triggered it.
pub async fn stub_capture_until(
    path: &Path,
    timeout: Duration,
    mut done: impl FnMut(&[serde_json::Value]) -> bool,
) -> Vec<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let frames = stub_capture(path);
        if done(&frames) || tokio::time::Instant::now() >= deadline {
            return frames;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub fn text_deltas(events: &[BridgeEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            BridgeEvent::TextDelta { delta, .. } => Some(delta.clone()),
            _ => None,
        })
        .collect()
}

pub fn final_text(events: &[BridgeEvent]) -> Option<String> {
    events.iter().rev().find_map(|event| match event {
        BridgeEvent::TextMessage { text, .. } => Some(text.clone()),
        _ => None,
    })
}

pub fn usage(events: &[BridgeEvent]) -> Option<AgentUsage> {
    events.iter().find_map(|event| match event {
        BridgeEvent::RunSettled { usage, .. } => usage.clone(),
        _ => None,
    })
}

pub fn tool_call(
    id: &str,
    status: HelperToolResultStatus,
    content: &str,
) -> (String, HelperToolResultStatus, String) {
    (id.to_string(), status, content.to_string())
}

pub fn steps(value: serde_json::Value) -> serde_json::Value {
    value
}
