//! A user rejection on an approval card must reach the model as a rejection,
//! not as "cancelled by Warp ... run the command again".

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
async fn a_denied_call_is_delivered_as_rejected_on_the_next_resume() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let capture = dir.path().join("stub.jsonl");
    let mut bridge = spawn_stub_bridge(dir.path(), "two_calls", &capture, test_timeouts()).await;

    let mut stream = bridge
        .start_turn("conv-1", "run two commands".to_string())
        .await
        .expect("turn starts")
        .stream;
    collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::ExchangePaused { .. })
    })
    .await;

    // The user rejected call_b on its card; the app records the denial before
    // the follow-up request.
    bridge
        .deny_tool_call("conv-1", "call_b")
        .await
        .expect("the rejection is recorded");

    let mut resumed = bridge
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
    let settled = collect_until(&mut resumed, TIMEOUT, |event| {
        matches!(event, BridgeEvent::RunSettled { .. })
    })
    .await;
    assert_eq!(final_text(&settled).as_deref(), Some("both done"));

    let frames = stub_capture(&capture);
    let resumes: Vec<_> = frames
        .iter()
        .filter(|frame| frame["kind"] == "turn.resume")
        .collect();
    assert_eq!(resumes.len(), 1, "one resume frame: {frames:?}");
    let results = resumes[0]["results"].as_array().expect("results array");
    let rejected = results
        .iter()
        .find(|result| result["tool_call_id"] == "call_b")
        .expect("call_b is answered");
    assert_eq!(
        rejected["status"], "rejected",
        "a user denial must reach the model as a rejection: {results:?}"
    );
    assert!(
        rejected["content"]
            .as_str()
            .unwrap_or_default()
            .contains("The user rejected"),
        "the model is told why: {rejected:?}"
    );

    bridge.shutdown().await;
}
