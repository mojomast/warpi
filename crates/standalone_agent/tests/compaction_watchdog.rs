//! Compaction runs inside the turn and emits only its start/finish events, so
//! it must not trip the 120 s provider-stall watchdog. The bridge gives a
//! compacting turn its own, longer deadline.

mod support;

use std::time::Duration;

use standalone_agent::bridge::{BridgeEvent, BridgeTimeouts};
use support::*;

const TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test]
async fn a_slow_compaction_does_not_trip_the_stall_watchdog() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let capture = dir.path().join("stub.jsonl");
    let timeouts = BridgeTimeouts {
        // Much shorter than the stub's 600 ms compaction, so a stall-based
        // watchdog would kill the turn while it is compacting.
        stall: Duration::from_millis(150),
        pending_tools: Some(Duration::from_secs(30)),
        cancel: Duration::from_secs(30),
        compaction: Duration::from_secs(5),
        watchdog_tick: Duration::from_millis(25),
    };
    let mut bridge = spawn_stub_bridge(dir.path(), "slow_compaction", &capture, timeouts).await;

    let mut stream = bridge
        .start_turn("conv-1", "compact the transcript".to_string())
        .await
        .expect("turn starts")
        .stream;
    let events = collect_until(&mut stream, TIMEOUT, |event| {
        matches!(
            event,
            BridgeEvent::RunSettled { .. } | BridgeEvent::ProtocolError { .. }
        )
    })
    .await;

    assert!(
        !events
            .iter()
            .any(|event| matches!(event, BridgeEvent::ProtocolError { .. })),
        "a healthy compaction must not be reported as a stall: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(BridgeEvent::RunSettled { .. })),
        "the turn must settle after compaction: {events:?}"
    );
    assert_eq!(final_text(&events).as_deref(), Some("compacted"));

    bridge.shutdown().await;
}
