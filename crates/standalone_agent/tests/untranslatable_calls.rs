//! Tool calls the Warp executor cannot represent are answered by the bridge
//! instead of being forwarded as a `Tool::Server` call the client maps to
//! `NoClientRepresentation`. A forwarded server call has no action and no
//! result, so the Pi run would park on it forever.
//!
//! `grep` with a `glob` filter is valid for the helper's schema but has no
//! client-side representation, which is exactly the shape the parity plan
//! describes.

mod support;

use std::time::Duration;

use standalone_agent::bridge::{BridgeEvent, BridgeTimeouts};
use standalone_agent::protocol::HelperToolResultStatus;
use support::*;

const TIMEOUT: Duration = Duration::from_secs(30);

fn test_timeouts() -> BridgeTimeouts {
    BridgeTimeouts {
        stall: Duration::from_secs(30),
        pending_tools: Some(Duration::from_secs(30)),
        cancel: Duration::from_secs(30),
        compaction: Duration::from_secs(30),
        watchdog_tick: Duration::from_millis(25),
    }
}

#[tokio::test]
async fn an_untranslatable_call_is_answered_in_place() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let capture = dir.path().join("stub.jsonl");
    let mut bridge =
        spawn_stub_bridge(dir.path(), "untranslatable_call", &capture, test_timeouts()).await;

    let started = bridge
        .start_turn("conv-1", "search".to_string())
        .await
        .expect("turn starts");
    let mut stream = started.stream;
    let events = collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::RunSettled { .. })
    })
    .await;

    assert!(
        !events
            .iter()
            .any(|event| matches!(event, BridgeEvent::ToolCalls { .. })),
        "an untranslatable call must never become a client tool call: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, BridgeEvent::ExchangePaused { .. })),
        "the exchange continues in place, it does not pause: {events:?}"
    );
    assert_eq!(final_text(&events).as_deref(), Some("handled in place"));

    let frames = stub_capture(&capture);
    let resumes: Vec<_> = frames
        .iter()
        .filter(|frame| frame["kind"] == "turn.resume")
        .collect();
    assert_eq!(
        resumes.len(),
        1,
        "the bridge answers without an app resume: {frames:?}"
    );
    assert_eq!(
        resumes[0]["exchange_id"], started.exchange_id,
        "the answer keeps the exchange the calls arrived in"
    );
    let results = resumes[0]["results"].as_array().expect("results array");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["tool_call_id"], "call_bad");
    assert_eq!(results[0]["status"], "error");
    assert!(
        results[0]["content"]
            .as_str()
            .unwrap_or_default()
            .contains("glob"),
        "the model is told which argument was unsupported: {results:?}"
    );

    bridge.shutdown().await;
}

// Uses the cross-process fixture provider, which is unreliable on Windows;
// `fixture_test!` ignores it there with the tracked reason (tests/support/mod.rs).
fixture_test! {
async fn the_real_helper_recovers_from_an_untranslatable_call() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let captures = dir.path().join("captures.json");
    // `grep` with a `glob` filter is valid for the helper's schema but has no
    // client-side representation, so the bridge must answer it without ever
    // asking the app for a result.
    let steps = serde_json::json!([
        {
            "kind": "tool_call",
            "toolCallId": "call_bad",
            "toolName": "grep",
            "argumentChunks": ["{\"pattern\":\"needle\",\"glob\":\"*.rs\"}"]
        },
        { "kind": "text", "chunks": ["recovered"] }
    ]);
    let mut fixture = FixtureProvider::start(&captures, &steps).await;
    let mut bridge = spawn_bridge(
        dir.path(),
        profile(&fixture.base_url, true),
        Some("sk-fixture"),
    )
    .await;

    let started = bridge
        .start_turn("conv-1", "search for needle".to_string())
        .await
        .expect("turn starts");
    let mut stream = started.stream;
    let events = collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::RunSettled { .. })
    })
    .await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, BridgeEvent::ToolCalls { .. })),
        "the unsupported call is answered before it reaches the app: {events:?}"
    );
    assert_eq!(final_text(&events).as_deref(), Some("recovered"));

    // The model sees the argument error in the continuation request.
    let captures = fixture.captures().await;
    let requests = captures.as_array().expect("captures array");
    assert_eq!(
        requests.len(),
        2,
        "exactly two provider requests: {captures}"
    );
    let messages = requests[1]["body"]["messages"]
        .as_array()
        .expect("messages");
    let tool_messages: Vec<_> = messages
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect();
    assert_eq!(
        tool_messages.len(),
        1,
        "one tool result present: {messages:?}"
    );
    assert_eq!(tool_messages[0]["tool_call_id"], "call_bad");
    assert!(
        tool_messages[0]["content"]
            .as_str()
            .unwrap_or_default()
            .contains("glob"),
        "the model is told which argument failed: {messages:?}"
    );

    bridge.shutdown().await;
}
}

#[tokio::test]
async fn a_mixed_batch_defers_the_untranslatable_call_to_the_next_resume() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let capture = dir.path().join("stub.jsonl");
    let mut bridge = spawn_stub_bridge(dir.path(), "mixed_calls", &capture, test_timeouts()).await;

    let started = bridge
        .start_turn("conv-1", "run and search".to_string())
        .await
        .expect("turn starts");
    let mut stream = started.stream;
    let paused = collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::ExchangePaused { .. })
    })
    .await;
    let calls = tool_calls(&paused);
    assert_eq!(
        calls.len(),
        1,
        "only the representable call is forwarded: {calls:?}"
    );
    assert_eq!(calls[0].tool_call_id, "call_good");

    let mut stream = bridge
        .resume_turn(
            "conv-1",
            vec![tool_call(
                "call_good",
                HelperToolResultStatus::Success,
                "good output",
            )],
        )
        .await
        .expect("resume accepted")
        .stream;
    let settled = collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::RunSettled { .. })
    })
    .await;
    assert_eq!(final_text(&settled).as_deref(), Some("mixed done"));

    let frames = stub_capture(&capture);
    let resumes: Vec<_> = frames
        .iter()
        .filter(|frame| frame["kind"] == "turn.resume")
        .collect();
    assert_eq!(resumes.len(), 1, "{frames:?}");
    let results = resumes[0]["results"].as_array().expect("results array");
    assert_eq!(
        results.len(),
        2,
        "the deferred untranslatable call rides along: {results:?}"
    );
    let bad = results
        .iter()
        .find(|result| result["tool_call_id"] == "call_bad")
        .expect("call_bad result");
    assert_eq!(bad["status"], "error");
    assert!(
        bad["content"].as_str().unwrap_or_default().contains("glob"),
        "the argument error is model-visible: {results:?}"
    );

    bridge.shutdown().await;
}
