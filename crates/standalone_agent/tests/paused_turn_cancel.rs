//! Stop / "send now" must cancel a turn that is paused on an approval card,
//! even though the app stopped reading the paused exchange (the W1 incident
//! shape). The app-side glue calls `cancel_turn(conversation, None)` from the
//! no-live-stream branch of `cancel_conversation_progress`; these tests pin the
//! bridge behavior that path depends on: the cancel reaches the helper with the
//! paused exchange identity, and the prompt queued behind the parked turn
//! starts as soon as the turn is released.

mod support;

use std::time::Duration;

use standalone_agent::bridge::{BridgeEvent, BridgeTimeouts};
use support::*;

const TIMEOUT: Duration = Duration::from_secs(30);

/// The documented "restore native's unbounded approval wait" configuration:
/// only the cancel deadline may release the parked turn.
fn unbounded_pending_timeouts() -> BridgeTimeouts {
    BridgeTimeouts {
        stall: Duration::from_secs(30),
        pending_tools: None,
        cancel: Duration::from_millis(250),
        compaction: Duration::from_secs(30),
        watchdog_tick: Duration::from_millis(25),
    }
}

#[tokio::test]
async fn a_paused_turn_is_cancelled_and_the_queued_prompt_starts() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let capture = dir.path().join("stub.jsonl");
    let mut bridge = spawn_stub_bridge(
        dir.path(),
        "pending_never",
        &capture,
        unbounded_pending_timeouts(),
    )
    .await;

    let started = bridge
        .start_turn("conv-1", "first prompt".to_string())
        .await
        .expect("turn starts");
    assert!(!started.queued, "the first prompt starts immediately");
    let mut stream = started.stream;
    let paused_exchange_id = started.exchange_id.clone();
    collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::ExchangePaused { .. })
    })
    .await;
    // The real app has no reader for a paused exchange; drop it here for the
    // same reason. Cancel must not depend on that stream.
    drop(stream);

    // A new prompt arrives while the approval is up: the bridge queues it.
    let queued = bridge
        .start_turn("conv-1", "second prompt".to_string())
        .await
        .expect("a prompt while paused is queued, not rejected");
    assert!(queued.queued, "the second prompt is queued behind the turn");
    let mut queued_stream = queued.stream;

    // This is the glue's no-live-stream cancel (stop / "send now"): it targets
    // the exchange the paused turn is attached to.
    bridge
        .cancel_turn("conv-1", Some(&paused_exchange_id))
        .await
        .expect("cancel accepted");

    let frames = stub_capture_until(&capture, Duration::from_secs(5), |frames| {
        frames.iter().any(|frame| frame["kind"] == "turn.cancel")
    })
    .await;
    assert!(
        frames.iter().any(|frame| frame["kind"] == "turn.cancel"),
        "turn.cancel must reach the helper: {frames:?}"
    );

    // The stub ignores cancels, so the bridge's cancel deadline releases the
    // turn and the queued prompt starts instead of waiting for the pending-tool
    // deadline (which is disabled here).
    let first_event = tokio::time::timeout(Duration::from_secs(5), queued_stream.recv())
        .await
        .expect("the queued prompt must start after the cancel")
        .expect("bridge events");
    assert!(matches!(first_event, BridgeEvent::Init { .. }));

    let frames = stub_capture_until(&capture, Duration::from_secs(5), |frames| {
        frames
            .iter()
            .filter(|frame| frame["kind"] == "turn.start")
            .count()
            >= 2
    })
    .await;
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["kind"] == "turn.start")
            .count(),
        2,
        "the queued prompt reaches the helper: {frames:?}"
    );

    bridge.shutdown().await;
}
