//! Duplicate tool-call handling and live-continuation re-attachment.
//!
//! A helper that re-emits a call id must not fail the exchange (which would
//! leave the helper parked forever); the duplicate is answered in place with a
//! rejection. A resume that delivers only already-delivered results while the
//! Pi turn is still live is a re-attach: the retry stream must receive the
//! helper's continuation instead of a synthetic terminal event that strands it.

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
async fn a_duplicate_call_id_is_answered_in_place_and_the_turn_settles() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let capture = dir.path().join("stub.jsonl");
    let mut bridge =
        spawn_stub_bridge(dir.path(), "duplicate_call", &capture, test_timeouts()).await;

    let started = bridge
        .start_turn("conv-1", "run the duplicate".to_string())
        .await
        .expect("turn starts");
    let mut stream = started.stream;
    let paused = collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::ExchangePaused { .. })
    })
    .await;
    assert_eq!(tool_calls(&paused).len(), 1);

    // The first result makes the helper re-emit the same call id.
    let mut resumed = bridge
        .resume_turn(
            "conv-1",
            vec![tool_call(
                "call_dup",
                HelperToolResultStatus::Success,
                "dup output",
            )],
        )
        .await
        .expect("resume accepted")
        .stream;
    let settled = collect_until(&mut resumed, TIMEOUT, |event| {
        matches!(
            event,
            BridgeEvent::RunSettled { .. } | BridgeEvent::ProtocolError { .. }
        )
    })
    .await;
    assert!(
        matches!(settled.last(), Some(BridgeEvent::RunSettled { .. })),
        "a duplicate id must not fail the exchange: {settled:?}"
    );
    assert_eq!(final_text(&settled).as_deref(), Some("both done"));

    // The bridge answered the duplicate itself with a rejection, so the helper
    // never parked on it.
    let frames = stub_capture(&capture);
    let resumes: Vec<_> = frames
        .iter()
        .filter(|frame| frame["kind"] == "turn.resume")
        .collect();
    assert_eq!(
        resumes.len(),
        2,
        "one real resume and one duplicate answer: {frames:?}"
    );
    let duplicate_answer = resumes[1]["results"]
        .as_array()
        .expect("duplicate result array");
    assert_eq!(duplicate_answer.len(), 1);
    assert_eq!(duplicate_answer[0]["tool_call_id"], "call_dup");
    assert_eq!(duplicate_answer[0]["status"], "rejected");

    bridge.shutdown().await;
}

#[tokio::test]
async fn an_all_duplicate_resume_reattaches_to_the_live_continuation() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let capture = dir.path().join("stub.jsonl");
    let mut bridge = spawn_stub_bridge(dir.path(), "slow_resume", &capture, test_timeouts()).await;

    let started = bridge
        .start_turn("conv-1", "run two commands".to_string())
        .await
        .expect("turn starts");
    let mut stream = started.stream;
    collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::ExchangePaused { .. })
    })
    .await;

    // The app's first continuation exchange is lost before any event arrives
    // (the shape that makes the response-stream model retry the request).
    let first = bridge
        .resume_turn(
            "conv-1",
            vec![
                tool_call("call_a", HelperToolResultStatus::Success, "a output"),
                tool_call("call_b", HelperToolResultStatus::Success, "b output"),
            ],
        )
        .await
        .expect("resume accepted");
    let first_exchange_id = first.exchange_id.clone();
    drop(first.stream);

    // The retry re-sends the identical results while the Pi turn is still live.
    let retry = bridge
        .resume_turn(
            "conv-1",
            vec![
                tool_call("call_a", HelperToolResultStatus::Success, "a output"),
                tool_call("call_b", HelperToolResultStatus::Success, "b output"),
            ],
        )
        .await
        .expect("a duplicate-only retry re-attaches");
    assert_eq!(
        retry.exchange_id, first_exchange_id,
        "the live continuation keeps its exchange identity"
    );

    let mut retry_stream = retry.stream;
    let events = collect_until(&mut retry_stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::RunSettled { .. })
    })
    .await;

    assert!(
        !events.iter().any(|event| matches!(
            event,
            BridgeEvent::RunSettled {
                stop_reason,
                ..
            } if stop_reason == "duplicate_tool_results"
        )),
        "a live turn must not be settled as duplicates: {events:?}"
    );
    let texts: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            BridgeEvent::TextMessage { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        vec!["slow done"],
        "the retry must see exactly one copy of the continuation"
    );

    bridge.shutdown().await;
}
