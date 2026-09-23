//! Consumer-facing delivery over the durable event log (`event-log-delivery-spec.md` §1/§3).
//!
//! Exercises the writer/reader split, the generation fence, and a replay-to-consumer
//! loop with an ack watermark, all without the bridge or GUI.

use serde_json::json;
use standalone_agent::event_log::{
    Actor, DeliveryMode, EventLog, EventLogError, LogRecordDraft, kinds, read_conversation,
};

fn draft(kind: &str, idem: &str, data: serde_json::Value) -> LogRecordDraft {
    LogRecordDraft::new(kind, idem, Actor::Bridge, data)
}

#[test]
fn a_consumer_replays_only_records_after_its_ack_watermark() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let mut log = EventLog::open(root, "conv-1", "writer-1").expect("open");
    log.append(draft(kinds::SESSION_OPENED, "session.opened:1", json!({"provider_id": "local"})))
        .expect("append");
    log.append(draft(kinds::QUEUE_ENQUEUED, "queue:exchange-1", json!({"position": 1})))
        .expect("append");

    // First delivery: everything.
    let first = log.replay(0, 100).expect("replay");
    assert_eq!(first.len(), 3);
    let ack = first.last().unwrap().seq;
    assert_eq!(ack, 3);

    // New events after the ack are delivered exactly once.
    log.append(draft(kinds::TURN_STARTED, "turn:exchange-1", json!({"source": "queued"})))
        .expect("append");
    let second = log.replay(ack, 100).expect("replay");
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].kind, kinds::TURN_STARTED);

    // Re-running the tail is idempotent for the consumer: no new records past the ack.
    let third = log.replay(second[0].seq, 100).expect("replay");
    assert!(third.is_empty());
}

#[test]
fn the_next_writer_fences_the_previous_generation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    let mut first = EventLog::open(root, "conv-1", "writer-1").expect("open");
    first
        .append(draft(kinds::TURN_STARTED, "turn:exchange-1", json!({})))
        .expect("append");
    let first_generation = first.generation();
    drop(first);

    let second = EventLog::open(root, "conv-1", "writer-2").expect("reopen");
    assert_eq!(second.generation(), first_generation + 1);
    assert!(matches!(
        second.check_generation(first_generation),
        Err(EventLogError::StaleGeneration { .. })
    ));
    assert!(second.check_generation(second.generation()).is_ok());
    drop(second);

    let view = read_conversation(root, "conv-1").expect("read");
    let log_opened: Vec<u64> = view
        .records
        .iter()
        .filter(|record| record.kind == kinds::LOG_OPENED)
        .map(|record| record.generation)
        .collect();
    assert_eq!(log_opened, vec![1, 2], "each restart records its own generation");
}

#[test]
fn delivery_intent_is_recorded_on_admission() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let mut log = EventLog::open(root, "conv-1", "writer-1").expect("open");
    for mode in [
        DeliveryMode::Queue,
        DeliveryMode::Steer,
        DeliveryMode::CancelAndSendNow,
    ] {
        log.append(draft(
            kinds::QUEUE_ENQUEUED,
            &format!("queue:{}", mode.queue_source()),
            json!({"source": mode.queue_source(), "destructive": mode.is_destructive()}),
        ))
        .expect("append");
    }
    drop(log);

    let view = read_conversation(root, "conv-1").expect("read");
    let sources: Vec<&str> = view
        .records
        .iter()
        .filter(|record| record.kind == kinds::QUEUE_ENQUEUED)
        .map(|record| record.data["source"].as_str().unwrap())
        .collect();
    assert_eq!(sources, vec!["queued", "steer", "immediate"]);
    let destructive: Vec<bool> = view
        .records
        .iter()
        .filter(|record| record.kind == kinds::QUEUE_ENQUEUED)
        .map(|record| record.data["destructive"].as_bool().unwrap())
        .collect();
    assert_eq!(destructive, vec![false, false, true]);
}

#[test]
fn a_retried_helper_frame_is_idempotent_by_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let mut log = EventLog::open(root, "conv-1", "writer-1").expect("open");
    let payload = json!({"tool_call_id": "call_00", "status": "success", "source": "app"});
    let first = log
        .append(draft(kinds::TOOL_RESULT, "tool.result:t1:call_00:app", payload.clone()))
        .expect("append");
    let retry = log
        .append(draft(kinds::TOOL_RESULT, "tool.result:t1:call_00:app", payload))
        .expect("retry is a no-op");
    assert_eq!(first, retry);

    let diverging = log.append(draft(
        kinds::TOOL_RESULT,
        "tool.result:t1:call_00:app",
        json!({"tool_call_id": "call_00", "status": "failure", "source": "app"}),
    ));
    assert!(matches!(diverging, Err(EventLogError::ReplayDiverged { .. })));
}
