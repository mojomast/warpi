//! Reopening a session when the request inputs change (profile, model, cwd,
//! credential) and refusing to reopen while a turn is live.

mod support;

use std::time::Duration;

use standalone_agent::bridge::{BridgeError, BridgeEvent, BridgeTimeouts, SessionSpec};
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

fn reopen_spec(data_dir: &std::path::Path, model_id: &str) -> SessionSpec {
    let mut provider = profile("http://127.0.0.1:1/v1", false);
    provider.model_id = model_id.to_string();
    SessionSpec {
        conversation_id: "conv-1".into(),
        working_dir: data_dir.to_path_buf(),
        provider,
        api_key: None,
        session_file: None,
        system_prompt: None,
        load_context_files: false,
        max_context_file_bytes: 4096,
        data_dir: data_dir.to_path_buf(),
        task_id: Some("conv-1".into()),
        create_task: false,
        subagents: None,
    }
}

#[tokio::test]
async fn reopening_applies_the_new_profile_with_a_new_generation() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let capture = dir.path().join("stub.jsonl");
    let mut bridge = spawn_stub_bridge(dir.path(), "two_calls", &capture, test_timeouts()).await;

    bridge
        .open_session(reopen_spec(dir.path(), "model-b"))
        .await
        .expect("a reopen while idle is accepted");

    let mut stream = bridge
        .start_turn("conv-1", "after the switch".to_string())
        .await
        .expect("turn starts")
        .stream;
    let first = tokio::time::timeout(Duration::from_secs(5), stream.recv())
        .await
        .expect("a fresh exchange starts")
        .expect("bridge events");
    assert!(matches!(first, BridgeEvent::Init { .. }));

    // The child writes the capture asynchronously; poll until the turn lands.
    let frames = stub_capture_until(&capture, Duration::from_secs(5), |frames| {
        frames.iter().any(|frame| frame["kind"] == "turn.start")
    })
    .await;
    let opens: Vec<_> = frames
        .iter()
        .filter(|frame| frame["kind"] == "session.open")
        .collect();
    assert_eq!(opens.len(), 2, "the reopen reaches the helper: {frames:?}");
    assert_eq!(opens[0]["model"], "fixture-model");
    assert_eq!(
        opens[1]["model"], "model-b",
        "the second open carries the new provider model"
    );
    assert_eq!(opens[0]["generation"], 0);
    assert_eq!(opens[1]["generation"], 1);

    let starts: Vec<_> = frames
        .iter()
        .filter(|frame| frame["kind"] == "turn.start")
        .collect();
    assert_eq!(
        starts.last().map(|frame| frame["generation"].clone()),
        Some(serde_json::json!(1)),
        "the new turn runs in the reopened generation: {frames:?}"
    );

    bridge.shutdown().await;
}

#[tokio::test]
async fn reopening_while_a_turn_is_live_is_refused() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let capture = dir.path().join("stub.jsonl");
    let mut bridge =
        spawn_stub_bridge(dir.path(), "pending_never", &capture, test_timeouts()).await;

    let mut stream = bridge
        .start_turn("conv-1", "park on a call".to_string())
        .await
        .expect("turn starts")
        .stream;
    collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::ExchangePaused { .. })
    })
    .await;

    let error = bridge
        .open_session(reopen_spec(dir.path(), "model-b"))
        .await
        .expect_err("a reopen must not orphan a live turn");
    assert!(
        matches!(error, BridgeError::SessionBusy(ref id) if id == "conv-1"),
        "unexpected error: {error}"
    );

    bridge.shutdown().await;
}
