//! A cancelled turn always settles, even when the helper never acknowledges
//! `turn.cancel`. The bridge synthesizes the terminal event after the cancel
//! deadline so the session accepts the next prompt instead of parking.

mod support;

use std::time::Duration;

use standalone_agent::bridge::{BridgeEvent, BridgeTimeouts};
use support::*;

const TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test]
async fn a_helper_that_ignores_cancel_is_expired_and_the_session_keeps_working() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let capture = dir.path().join("stub.jsonl");
    let timeouts = BridgeTimeouts {
        stall: Duration::from_secs(30),
        pending_tools: Some(Duration::from_secs(30)),
        cancel: Duration::from_millis(250),
        compaction: Duration::from_secs(30),
        watchdog_tick: Duration::from_millis(25),
    };
    let mut bridge = spawn_stub_bridge(dir.path(), "ignore_cancel", &capture, timeouts).await;

    let started = bridge
        .start_turn("conv-1", "start something".to_string())
        .await
        .expect("turn starts");
    let mut stream = started.stream;
    let first = tokio::time::timeout(Duration::from_secs(5), stream.recv())
        .await
        .expect("exchange attached")
        .expect("bridge events");
    assert!(matches!(first, BridgeEvent::Init { .. }));

    bridge
        .cancel_turn("conv-1", Some(&started.exchange_id))
        .await
        .expect("cancel accepted");

    let events = collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::RunCancelled { .. })
    })
    .await;
    assert!(
        matches!(events.last(), Some(BridgeEvent::RunCancelled { .. })),
        "the bridge synthesizes the terminal event: {events:?}"
    );

    let frames = stub_capture_until(&capture, Duration::from_secs(2), |frames| {
        frames.iter().any(|frame| frame["kind"] == "turn.cancel")
    })
    .await;
    assert!(
        frames.iter().any(|frame| frame["kind"] == "turn.cancel"),
        "the helper did receive turn.cancel before the deadline: {frames:?}"
    );

    // The session is free again: a fresh prompt starts its own exchange
    // instead of being queued behind the turn that never settled.
    let mut stream = bridge
        .start_turn("conv-1", "second prompt".to_string())
        .await
        .expect("a fresh turn is accepted")
        .stream;
    let first = tokio::time::timeout(Duration::from_secs(5), stream.recv())
        .await
        .expect("a fresh exchange starts")
        .expect("bridge events");
    assert!(matches!(first, BridgeEvent::Init { .. }));

    bridge.shutdown().await;
}
