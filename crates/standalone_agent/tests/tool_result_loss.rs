//! Every tool call the bridge forwards receives exactly one result.
//!
//! Warp drops the result of a cancelled command during request conversion
//! (`CancelledBeforeExecution` -> `Ignore`), so from the bridge's point of view
//! the app simply sends a resume that omits one of the calls it emitted. These
//! tests drive that shape with the scripted stub helper; the sanitized script
//! mirrors incident 1 (two shell calls in one batch, one answered, the sibling
//! never answered) with the commands reduced to `echo`.

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
async fn a_dropped_tool_result_is_synthesized_on_the_next_resume() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let capture = dir.path().join("stub.jsonl");
    let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/support/fixtures/incident-2026-09-23-two-shell-calls.json");
    let mut bridge = spawn_stub_bridge_with(
        dir.path(),
        "fixture",
        &capture,
        test_timeouts(),
        &[(
            "WARPI_STUB_FIXTURE",
            fixture.to_str().expect("fixture path is UTF-8"),
        )],
    )
    .await;

    let started = bridge
        .start_turn("conv-1", "run two commands".to_string())
        .await
        .expect("turn starts");
    let mut stream = started.stream;
    let paused = collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::ExchangePaused { .. })
    })
    .await;
    let ids: Vec<String> = tool_calls(&paused)
        .iter()
        .map(|call| call.tool_call_id.clone())
        .collect();
    assert_eq!(ids, vec!["call_a", "call_b"]);

    // The follow-up delivered only call_a: call_b was cancelled by Warp's
    // snapshot cascade and converted to `Ignore`, so its result never exists.
    let mut stream = bridge
        .resume_turn(
            "conv-1",
            vec![tool_call(
                "call_a",
                HelperToolResultStatus::Success,
                "a output",
            )],
        )
        .await
        .expect("resume accepted")
        .stream;
    let settled = collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::RunSettled { .. })
    })
    .await;
    assert_eq!(final_text(&settled).as_deref(), Some("both done"));

    let frames = stub_capture(&capture);
    let resumes: Vec<_> = frames
        .iter()
        .filter(|frame| frame["kind"] == "turn.resume")
        .collect();
    assert_eq!(resumes.len(), 1, "exactly one resume frame: {frames:?}");
    let results = resumes[0]["results"].as_array().expect("results array");
    assert_eq!(
        results.len(),
        2,
        "the dropped call is answered in the same frame: {results:?}"
    );
    let by_id = |id: &str| {
        results
            .iter()
            .find(|result| result["tool_call_id"] == id)
            .unwrap_or_else(|| panic!("missing {id}: {results:?}"))
    };
    assert_eq!(by_id("call_a")["status"], "success");
    assert_eq!(by_id("call_a")["content"], "a output");
    assert_eq!(by_id("call_b")["status"], "error");
    assert!(
        by_id("call_b")["content"]
            .as_str()
            .unwrap_or_default()
            .contains("cancelled in Warp"),
        "synthesized content spells out the drop: {results:?}"
    );

    bridge.shutdown().await;
}

#[tokio::test]
async fn a_pending_tool_call_that_is_never_answered_is_expired() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let capture = dir.path().join("stub.jsonl");
    let timeouts = BridgeTimeouts {
        pending_tools: Some(Duration::from_millis(200)),
        watchdog_tick: Duration::from_millis(25),
        ..test_timeouts()
    };
    let mut bridge = spawn_stub_bridge(dir.path(), "pending_never", &capture, timeouts).await;

    let started = bridge
        .start_turn("conv-1", "hang on a command".to_string())
        .await
        .expect("turn starts");
    let mut stream = started.stream;
    collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::ExchangePaused { .. })
    })
    .await;

    let failed = collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::RunFailed { .. })
    })
    .await;
    match failed.last() {
        Some(BridgeEvent::RunFailed {
            code,
            retryable,
            message,
        }) => {
            assert_eq!(code, "tool_timeout");
            assert!(retryable, "the app may retry the prompt");
            assert!(message.contains("dropped"), "message: {message}");
        }
        other => panic!("expected a tool timeout, got {other:?}"),
    }

    // The helper was told to cancel the turn it will never finish...
    let frames = stub_capture_until(&capture, Duration::from_secs(2), |frames| {
        frames.iter().any(|frame| frame["kind"] == "turn.cancel")
    })
    .await;
    assert!(
        frames.iter().any(|frame| frame["kind"] == "turn.cancel"),
        "the watchdog must cancel the parked turn: {frames:?}"
    );

    // ...and a fresh prompt starts instead of hitting a busy session.
    let mut stream = bridge
        .start_turn("conv-1", "try again".to_string())
        .await
        .expect("a new prompt is accepted")
        .stream;
    let first = tokio::time::timeout(Duration::from_secs(5), stream.recv())
        .await
        .expect("a fresh exchange starts")
        .expect("bridge events");
    assert!(matches!(first, BridgeEvent::Init { .. }));

    bridge.shutdown().await;
}
