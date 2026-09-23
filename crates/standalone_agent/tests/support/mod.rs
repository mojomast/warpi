//! Test support: spawns the TypeScript fixture provider and the Rust bridge
//! around the real Pi helper.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use standalone_agent::bridge::{BridgeConfig, BridgeEvent, SessionSpec, StandaloneBridge, TurnStream};
use standalone_agent::helper::HelperLaunchConfig;
use standalone_agent::protocol::{AgentUsage, HelperToolResultStatus, ToolCallSpec};
use standalone_agent::provider::{CredentialRef, ProviderProfile, WireProtocol};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

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
        Self { child, captures_path: captures_path.to_path_buf(), base_url }
    }

    /// Captured request bodies and headers (written when the server stops).
    pub async fn captures(&mut self) -> serde_json::Value {
        self.stop().await;
        let raw = std::fs::read_to_string(&self.captures_path).expect("fixture captures written on stop");
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
        credential: if with_key {
            CredentialRef::SecretStore { key: "warpi/fixture".into() }
        } else {
            CredentialRef::None
        },
        context_limit: 32768,
        output_limit: 4096,
        compat: Default::default(),
        reasoning: false,
        supports_image_input: false,
        headers: Default::default(),
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
    })
    .await
    .expect("bridge spawns");
    let hello = bridge.hello().await.expect("hello handshake");
    assert_eq!(hello.capabilities.protocol, 1);
    let mut tools = hello.capabilities.brokered_tools.clone();
    tools.sort();
    assert_eq!(tools, vec!["bash", "edit", "glob", "grep", "read", "write"]);
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

pub fn tool_calls(events: &[BridgeEvent]) -> Vec<ToolCallSpec> {
    events
        .iter()
        .find_map(|event| match event {
            BridgeEvent::ToolCalls { calls } => Some(calls.clone()),
            _ => None,
        })
        .unwrap_or_default()
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

pub fn tool_call(id: &str, status: HelperToolResultStatus, content: &str) -> (String, HelperToolResultStatus, String) {
    (id.to_string(), status, content.to_string())
}

pub fn steps(value: serde_json::Value) -> serde_json::Value {
    value
}
