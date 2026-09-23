//! Two independent conversations with different provider profiles and working
//! directories must not share clients, sessions, or tool-correlation state.

mod support;

use std::time::Duration;

use standalone_agent::bridge::{BridgeEvent, SessionSpec, StandaloneBridge};
use standalone_agent::helper::HelperLaunchConfig;
use standalone_agent::protocol::HelperToolResultStatus;
use support::*;

const TIMEOUT: Duration = Duration::from_secs(30);

// Uses the cross-process fixture provider, which is unreliable on Windows;
// `fixture_test!` ignores it there with the tracked reason (tests/support/mod.rs).
fixture_test! {
async fn two_sessions_with_different_profiles_stay_isolated() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let first_dir = root.path().join("first");
    let second_dir = root.path().join("second");
    std::fs::create_dir_all(&first_dir).expect("first dir");
    std::fs::create_dir_all(&second_dir).expect("second dir");

    // Two independent fixture endpoints, each with its own script.
    let first_captures = root.path().join("first-captures.json");
    let second_captures = root.path().join("second-captures.json");
    let mut first_fixture = FixtureProvider::start(
        &first_captures,
        &serde_json::json!([
            {
                "kind": "tool_call",
                "toolCallId": "first-call",
                "toolName": "bash",
                "argumentChunks": ["{\"command\":\"echo first\"}"]
            },
            { "kind": "text", "chunks": ["first done"] }
        ]),
    )
    .await;
    let mut second_fixture = FixtureProvider::start(
        &second_captures,
        &serde_json::json!([
            {
                "kind": "tool_call",
                "toolCallId": "second-call",
                "toolName": "read",
                "argumentChunks": ["{\"path\":\"only-in-second.txt\"}"]
            },
            { "kind": "text", "chunks": ["second done"] }
        ]),
    )
    .await;

    let data_dir = root.path().join("data");
    let mut bridge: StandaloneBridge =
        StandaloneBridge::spawn(standalone_agent::bridge::BridgeConfig {
            launch: HelperLaunchConfig::node(helper_entry(), &data_dir),
            retry: standalone_agent::bridge::RetryOptions {
                enabled: false,
                max_retries: 0,
                base_delay_ms: 0,
            },
            compaction_enabled: true,
            timeouts: standalone_agent::bridge::BridgeTimeouts::default(),
        })
        .await
        .expect("bridge spawns");
    bridge.hello().await.expect("handshake");

    let mut first_profile = profile(&first_fixture.base_url, true);
    first_profile.id = "first-profile".into();
    let mut second_profile = profile(&second_fixture.base_url, true);
    second_profile.id = "second-profile".into();
    second_profile.model_id = "second-model".into();

    for (conversation, working_dir, provider) in [
        ("conv-first", &first_dir, first_profile),
        ("conv-second", &second_dir, second_profile),
    ] {
        bridge
            .open_session(SessionSpec {
                conversation_id: conversation.to_string(),
                working_dir: working_dir.to_path_buf(),
                provider,
                api_key: Some(standalone_agent::SecretString::new("sk-isolation")),
                session_file: None,
                system_prompt: None,
                load_context_files: false,
                max_context_file_bytes: 4096,
                data_dir: data_dir.clone(),
                task_id: Some(conversation.to_string()),
                create_task: true,
                subagents: None,
            })
            .await
            .expect("session opens");
    }

    let mut first_stream = bridge
        .start_turn("conv-first", "first query".to_string())
        .await
        .expect("first turn")
        .stream;
    let mut second_stream = bridge
        .start_turn("conv-second", "second query".to_string())
        .await
        .expect("second turn")
        .stream;

    let first_paused = collect_until(&mut first_stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::ExchangePaused { .. })
    })
    .await;
    let second_paused = collect_until(&mut second_stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::ExchangePaused { .. })
    })
    .await;
    assert_eq!(tool_calls(&first_paused)[0].tool_call_id, "first-call");
    assert_eq!(tool_calls(&second_paused)[0].tool_call_id, "second-call");

    // A result belonging to the second conversation must not resume the first.
    let mut foreign = bridge
        .resume_turn(
            "conv-first",
            vec![tool_call(
                "second-call",
                HelperToolResultStatus::Success,
                "x",
            )],
        )
        .await
        .expect("resume call accepted")
        .stream;
    let rejected = collect_until(&mut foreign, TIMEOUT, |event| {
        matches!(event, BridgeEvent::ProtocolError { .. })
    })
    .await;
    assert!(matches!(
        rejected.last(),
        Some(BridgeEvent::ProtocolError { .. })
    ));

    // The genuine result resumes the first turn without touching the second.
    let mut first_resumed = bridge
        .resume_turn(
            "conv-first",
            vec![tool_call(
                "first-call",
                HelperToolResultStatus::Success,
                "first output",
            )],
        )
        .await
        .expect("resume accepted")
        .stream;
    let first_settled = collect_until(&mut first_resumed, TIMEOUT, |event| {
        matches!(event, BridgeEvent::RunSettled { .. })
    })
    .await;
    assert_eq!(final_text(&first_settled).as_deref(), Some("first done"));

    let mut second_resumed = bridge
        .resume_turn(
            "conv-second",
            vec![tool_call(
                "second-call",
                HelperToolResultStatus::Success,
                "second output",
            )],
        )
        .await
        .expect("resume accepted")
        .stream;
    let second_settled = collect_until(&mut second_resumed, TIMEOUT, |event| {
        matches!(event, BridgeEvent::RunSettled { .. })
    })
    .await;
    assert_eq!(final_text(&second_settled).as_deref(), Some("second done"));

    // Each conversation used exactly its own endpoint.
    let first_captures = first_fixture.captures().await;
    let second_captures = second_fixture.captures().await;
    assert_eq!(first_captures.as_array().map(Vec::len), Some(2));
    assert_eq!(second_captures.as_array().map(Vec::len), Some(2));
    assert_eq!(first_captures[0]["body"]["model"], "fixture-model");
    assert_eq!(second_captures[0]["body"]["model"], "second-model");
    // Credentials are per profile/session and never cross endpoints.
    assert_eq!(
        first_captures[0]["headers"]["authorization"],
        "Bearer sk-isolation"
    );
    assert_eq!(
        second_captures[0]["headers"]["authorization"],
        "Bearer sk-isolation"
    );

    bridge.shutdown().await;
}
}
