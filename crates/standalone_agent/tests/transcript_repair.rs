//! Startup transcript repair: a Pi session file left with an assistant tool
//! call that never received a result (crash / kill -9 while suspended on an
//! approval) must be repaired before the next provider request, or every later
//! prompt fails as a provider error.

mod support;

use std::time::Duration;

use standalone_agent::bridge::{
    BridgeConfig, BridgeEvent, BridgeTimeouts, RetryOptions, SessionSpec, StandaloneBridge,
};
use standalone_agent::helper::HelperLaunchConfig;
use support::*;

const TIMEOUT: Duration = Duration::from_secs(30);

async fn spawn_bridge_at(data_dir: &std::path::Path) -> StandaloneBridge {
    let mut bridge = StandaloneBridge::spawn(BridgeConfig {
        launch: HelperLaunchConfig::node(helper_entry(), data_dir),
        retry: RetryOptions {
            enabled: false,
            max_retries: 0,
            base_delay_ms: 0,
        },
        compaction_enabled: true,
        timeouts: BridgeTimeouts::default(),
    })
    .await
    .expect("bridge spawns");
    bridge.hello().await.expect("handshake");
    bridge
}

// Cross-process fixture test: the real helper talks to a separate Node fixture
// provider (tests/support/mod.rs).
fixture_test! {
async fn a_dangling_tool_call_is_repaired_before_the_next_provider_request() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let data_dir = root.path().join("data");
    let first_captures = root.path().join("first-captures.json");
    let second_captures = root.path().join("second-captures.json");

    // First session: the model asks for a command and the process stops while
    // the approval is still up, so the assistant tool call has no result.
    let mut first_fixture = FixtureProvider::start(
        &first_captures,
        &serde_json::json!([
            {
                "kind": "tool_call",
                "toolCallId": "call_dangle",
                "toolName": "bash",
                "argumentChunks": ["{\"command\":\"echo dangling\"}"]
            }
        ]),
    )
    .await;
    let mut first_profile = profile(&first_fixture.base_url, true);
    first_profile.id = "repair-first".into();
    let mut first_bridge = spawn_bridge_at(&data_dir).await;
    let opened = first_bridge
        .open_session(SessionSpec {
            conversation_id: "conv-1".into(),
            working_dir: data_dir.to_path_buf(),
            provider: first_profile,
            api_key: Some(standalone_agent::SecretString::new("sk-repair")),
            session_file: None,
            system_prompt: None,
            load_context_files: false,
            max_context_file_bytes: 4096,
            data_dir: data_dir.clone(),
            task_id: Some("conv-1".into()),
            create_task: true,
            subagents: None,
        })
        .await
        .expect("first session opens");
    let session_file = opened
        .session_file
        .expect("a fresh session persists to a file");

    let mut stream = first_bridge
        .start_turn("conv-1", "run the command".to_string())
        .await
        .expect("turn starts")
        .stream;
    collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::ExchangePaused { .. })
    })
    .await;
    // Give the SDK a moment to persist the assistant message before the
    // simulated crash.
    tokio::time::sleep(Duration::from_millis(250)).await;
    drop(stream);
    first_bridge.shutdown().await;
    first_fixture.stop().await;
    drop(first_fixture);

    // Restart: the same session file is opened, and the repair must answer the
    // dangling call before the provider sees the history.
    let mut second_fixture = FixtureProvider::start(
        &second_captures,
        &serde_json::json!([{ "kind": "text", "chunks": ["recovered"] }]),
    )
    .await;
    let mut second_profile = profile(&second_fixture.base_url, true);
    second_profile.id = "repair-second".into();
    let mut second_bridge = spawn_bridge_at(&data_dir).await;
    second_bridge
        .open_session(SessionSpec {
            conversation_id: "conv-1".into(),
            working_dir: data_dir.to_path_buf(),
            provider: second_profile,
            api_key: Some(standalone_agent::SecretString::new("sk-repair")),
            session_file: Some(std::path::PathBuf::from(&session_file)),
            system_prompt: None,
            load_context_files: false,
            max_context_file_bytes: 4096,
            data_dir: data_dir.clone(),
            task_id: Some("conv-1".into()),
            create_task: false,
            subagents: None,
        })
        .await
        .expect("the session resumes");

    let mut stream = second_bridge
        .start_turn("conv-1", "continue".to_string())
        .await
        .expect("turn starts")
        .stream;
    let settled = collect_until(&mut stream, TIMEOUT, |event| {
        matches!(
            event,
            BridgeEvent::RunSettled { .. } | BridgeEvent::RunFailed { .. }
        )
    })
    .await;
    assert!(
        matches!(settled.last(), Some(BridgeEvent::RunSettled { .. })),
        "the repaired session must reach the provider: {settled:?}"
    );
    assert_eq!(final_text(&settled).as_deref(), Some("recovered"));

    let captures = second_fixture.captures().await;
    let requests = captures.as_array().expect("capture array");
    assert_eq!(requests.len(), 1, "one provider request: {captures}");
    let body = serde_json::to_string(&requests[0]["body"]).expect("serialize body");
    assert!(
        body.contains("call_dangle"),
        "the provider history keeps the original call: {body}"
    );
    assert!(
        body.contains("never ran"),
        "the dangling call was answered with a synthetic result: {body}"
    );

    second_bridge.shutdown().await;
}
}
