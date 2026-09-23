//! Subagent (`task.*`) frames are decoded, forwarded through the identity
//! matched exchange, and gated on the helper's advertised capability. The stub
//! helper stands in for the real Pi helper so the wire shape is deterministic.

mod support;

use std::time::Duration;

use standalone_agent::bridge::{BridgeEvent, BridgeTimeouts};
use standalone_agent::protocol::HelperSubagents;
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

fn enabled_subagents() -> HelperSubagents {
    HelperSubagents {
        enabled: true,
        max_children: Some(2),
        max_depth: Some(1),
        budget: Some(standalone_agent::protocol::HelperSubagentBudget {
            max_turns: Some(8),
            deadline_seconds: Some(120),
            ..Default::default()
        }),
    }
}

#[tokio::test]
async fn task_events_forward_through_the_parent_exchange() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let capture = dir.path().join("stub.jsonl");
    let mut bridge = spawn_stub_bridge_with_subagents(
        dir.path(),
        "subagents",
        &capture,
        test_timeouts(),
        Some(enabled_subagents()),
    )
    .await;

    let mut stream = bridge
        .start_turn("conv-1", "explore".to_string())
        .await
        .expect("turn starts")
        .stream;
    let events = collect_until(&mut stream, TIMEOUT, |event| {
        matches!(event, BridgeEvent::RunSettled { .. })
    })
    .await;

    let started = events
        .iter()
        .find_map(|event| match event {
            BridgeEvent::TaskStarted {
                task_id,
                child_session_id,
                subagent_type,
                max_turns,
                ..
            } => Some((
                task_id.clone(),
                child_session_id.clone(),
                subagent_type.clone(),
                *max_turns,
            )),
            _ => None,
        })
        .expect("task.started is forwarded");
    assert_eq!(started.0, "task-1");
    assert_eq!(started.1, "conv-1:task-1");
    assert_eq!(started.2, "explore");
    assert_eq!(started.3, 5);

    let progress = events
        .iter()
        .find_map(|event| match event {
            BridgeEvent::TaskProgress {
                task_id,
                tokens,
                pending_tools,
                ..
            } => Some((task_id.clone(), *tokens, *pending_tools)),
            _ => None,
        })
        .expect("task.progress is forwarded");
    assert_eq!(progress.0, "task-1");
    assert_eq!(progress.1.total_tokens, 15);
    assert_eq!(progress.2, 0);

    let completed = events
        .iter()
        .find_map(|event| match event {
            BridgeEvent::TaskCompleted {
                task_id,
                status,
                usage,
                turns,
                wall_ms,
                ..
            } => Some((
                task_id.clone(),
                status.clone(),
                usage.clone(),
                *turns,
                *wall_ms,
            )),
            _ => None,
        })
        .expect("task.completed is forwarded");
    assert_eq!(completed.0, "task-1");
    assert_eq!(completed.1, "ok");
    assert_eq!(completed.2.input_tokens, 1_000);
    assert_eq!(completed.2.total_tokens, Some(1_200));
    assert_eq!(completed.3, 2);
    assert_eq!(completed.4, 2_000);

    // Task events are not terminal: the exchange settles normally afterwards.
    let settled = events
        .iter()
        .filter(|event| matches!(event, BridgeEvent::RunSettled { .. }))
        .count();
    assert_eq!(settled, 1, "exactly one terminal event: {events:?}");

    bridge.shutdown().await;
}

#[tokio::test]
async fn the_session_open_payload_carries_the_enabled_config() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let capture = dir.path().join("stub.jsonl");
    let mut bridge = spawn_stub_bridge_with_subagents(
        dir.path(),
        "two_calls",
        &capture,
        test_timeouts(),
        Some(enabled_subagents()),
    )
    .await;

    let frames = stub_capture_until(&capture, TIMEOUT, |frames| {
        frames.iter().any(|frame| frame["kind"] == "session.open")
    })
    .await;
    let open = frames
        .iter()
        .find(|frame| frame["kind"] == "session.open")
        .expect("session.open captured");
    assert_eq!(open["subagents"]["enabled"], serde_json::json!(true));
    assert_eq!(open["subagents"]["max_children"], serde_json::json!(2));
    assert_eq!(
        open["subagents"]["budget"]["max_turns"],
        serde_json::json!(8)
    );

    bridge.shutdown().await;
}

#[tokio::test]
async fn a_disabled_config_never_reaches_session_open() {
    if !node_available() {
        eprintln!("NOT RUN: node is unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let capture = dir.path().join("stub.jsonl");
    let mut bridge = spawn_stub_bridge_with_subagents(
        dir.path(),
        "two_calls",
        &capture,
        test_timeouts(),
        Some(HelperSubagents {
            enabled: false,
            ..Default::default()
        }),
    )
    .await;

    let frames = stub_capture_until(&capture, TIMEOUT, |frames| {
        frames.iter().any(|frame| frame["kind"] == "session.open")
    })
    .await;
    let open = frames
        .iter()
        .find(|frame| frame["kind"] == "session.open")
        .expect("session.open captured");
    assert_eq!(
        open["subagents"],
        serde_json::Value::Null,
        "a disabled config must not add a subagents key: {open}"
    );

    bridge.shutdown().await;
}
