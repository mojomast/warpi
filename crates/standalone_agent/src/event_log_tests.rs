use super::*;
use serde_json::json;

fn record(seq: u64, kind: &str, idem: &str, data: Value) -> LogRecord {
    LogRecord {
        v: EVENT_LOG_VERSION,
        seq,
        ts: "2026-09-23T16:20:11.123Z".to_string(),
        conversation_id: "conv-1".to_string(),
        generation: 1,
        writer_id: "writer-1".to_string(),
        kind: kind.to_string(),
        idem: idem.to_string(),
        turn_id: None,
        exchange_id: None,
        run_id: None,
        actor: Actor::Bridge,
        data,
    }
}

fn write_segment(dir: &Path, index: u32, records: &[LogRecord]) {
    fs::create_dir_all(dir).unwrap();
    let path = segment_path(dir, index);
    let mut contents = String::new();
    for record in records {
        contents.push_str(&serde_json::to_string(record).unwrap());
        contents.push('\n');
    }
    fs::write(path, contents).unwrap();
}

fn append_record(log: &mut EventLog, kind: &str, idem: &str, data: Value) -> u64 {
    log.append(LogRecordDraft::new(kind, idem, Actor::Bridge, data))
        .expect("append")
}

#[test]
fn open_allocates_the_first_generation_and_writes_log_opened() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = EventLog::open(dir.path(), "conv-1", "writer-1").expect("open");
    assert_eq!(log.generation(), 1);
    assert_eq!(log.last_seq(), 1);
    drop(log);

    let view = read_conversation(dir.path(), "conv-1").expect("read");
    assert_eq!(view.generation, 1);
    assert_eq!(view.last_seq, 1);
    assert_eq!(view.records.len(), 1);
    assert_eq!(view.records[0].kind, kinds::LOG_OPENED);
    assert_eq!(view.records[0].generation, 1);
    assert_eq!(view.records[0].data["resumed_from_seq"], json!(0));
}

#[test]
fn append_is_idempotent_by_idempotency_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut log = EventLog::open(dir.path(), "conv-1", "writer-1").expect("open");
    let first = append_record(&mut log, kinds::SESSION_OPENED, "session.opened:1", json!({"a": 1}));
    assert_eq!(first, 2);
    let duplicate = append_record(
        &mut log,
        kinds::SESSION_OPENED,
        "session.opened:1",
        json!({"a": 1}),
    );
    assert_eq!(duplicate, 2);
    drop(log);

    let view = read_conversation(dir.path(), "conv-1").expect("read");
    assert_eq!(view.records.len(), 2, "the duplicate was not written");
}

#[test]
fn diverging_content_for_an_existing_key_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut log = EventLog::open(dir.path(), "conv-1", "writer-1").expect("open");
    append_record(&mut log, kinds::SESSION_OPENED, "session.opened:1", json!({"a": 1}));
    let error = log
        .append(LogRecordDraft::new(
            kinds::SESSION_OPENED,
            "session.opened:1",
            Actor::Bridge,
            json!({"a": 2}),
        ))
        .unwrap_err();
    assert!(matches!(error, EventLogError::ReplayDiverged { .. }), "{error}");
    assert_eq!(log.last_seq(), 2);
}

#[test]
fn reopen_fences_the_previous_generation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = EventLog::open(dir.path(), "conv-1", "writer-1").expect("open");
    assert_eq!(log.generation(), 1);
    drop(log);

    let log = EventLog::open(dir.path(), "conv-1", "writer-2").expect("reopen");
    assert_eq!(log.generation(), 2, "generation is persisted + 1");
    assert!(matches!(
        log.check_generation(1),
        Err(EventLogError::StaleGeneration { .. })
    ));
    assert!(log.check_generation(2).is_ok());
    assert_eq!(log.last_seq(), 2, "the reopen record follows the first log.opened");
}

#[test]
fn replay_respects_after_seq_and_limit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut log = EventLog::open(dir.path(), "conv-1", "writer-1").expect("open");
    for seq in 0..3 {
        append_record(
            &mut log,
            kinds::USAGE_RECORDED,
            &format!("usage:{seq}"),
            json!({"seq": seq}),
        );
    }
    let after_one = log.replay(1, 10).expect("replay");
    assert_eq!(after_one.len(), 3);
    assert_eq!(after_one[0].seq, 2);
    let limited = log.replay(0, 2).expect("replay");
    assert_eq!(limited.len(), 2);
    assert_eq!(limited[1].seq, 2);
}

#[test]
fn a_torn_tail_is_reported_and_not_treated_as_a_record() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log_dir = dir.path().join("conv-1");
    write_segment(
        &log_dir,
        1,
        &[record(1, kinds::SESSION_OPENED, "session.opened:1", json!({}))],
    );
    let path = segment_path(&log_dir, 1);
    let mut bytes = fs::read(&path).unwrap();
    bytes.extend_from_slice(b"{\"v\":1,\"seq\":2,\"idem\":");
    fs::write(&path, bytes).unwrap();

    let report = verify_conversation(dir.path(), "conv-1").expect("verify");
    assert!(report.truncated_tail);
    assert!(report.gaps.is_empty());
    assert_eq!(report.last_seq, 1);
    assert_eq!(report.records, 1);
}

#[test]
fn a_sequence_gap_is_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log_dir = dir.path().join("conv-1");
    write_segment(
        &log_dir,
        1,
        &[
            record(1, kinds::SESSION_OPENED, "session.opened:1", json!({})),
            record(3, kinds::SESSION_OPENED, "session.opened:3", json!({})),
        ],
    );
    let report = verify_conversation(dir.path(), "conv-1").expect("verify");
    assert_eq!(report.gaps, vec![SequenceGap { expected: 2, found: 3 }]);
    assert_eq!(report.last_seq, 1);
}

#[test]
fn a_divergent_duplicate_is_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log_dir = dir.path().join("conv-1");
    write_segment(
        &log_dir,
        1,
        &[
            record(1, kinds::SESSION_OPENED, "session.opened:1", json!({"a": 1})),
            record(1, kinds::SESSION_OPENED, "session.opened:1", json!({"a": 2})),
        ],
    );
    let report = verify_conversation(dir.path(), "conv-1").expect("verify");
    assert_eq!(report.divergences.len(), 1);
    assert_eq!(report.divergences[0].seq, 1);
    assert_eq!(report.last_seq, 1);
}

#[test]
fn a_corrupt_non_tail_segment_is_quarantined() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log_dir = dir.path().join("conv-1");
    let good = record(1, kinds::SESSION_OPENED, "session.opened:1", json!({}));
    let contents = format!(
        "{}\nnot json\n{}\n",
        serde_json::to_string(&good).unwrap(),
        serde_json::to_string(&record(3, kinds::SESSION_OPENED, "session.opened:3", json!({})))
            .unwrap()
    );
    fs::create_dir_all(&log_dir).unwrap();
    fs::write(segment_path(&log_dir, 1), contents).unwrap();

    let log = EventLog::open(dir.path(), "conv-1", "writer-1").expect("open");
    assert!(log.is_degraded());
    assert!(log_dir.join("seg-000001.jsonl.corrupt").exists());
    drop(log);
    assert!(!segment_path(&log_dir, 1).exists());
}

#[test]
fn a_second_writer_fails_closed_until_the_first_releases() {
    let dir = tempfile::tempdir().expect("tempdir");
    let first = EventLog::open(dir.path(), "conv-1", "writer-1").expect("open");
    let second = EventLog::open(dir.path(), "conv-1", "writer-2");
    assert!(matches!(second, Err(EventLogError::Locked)));
    drop(first);
    let third = EventLog::open(dir.path(), "conv-1", "writer-3").expect("reopen after release");
    assert_eq!(third.generation(), 2);
}

#[test]
fn rotation_spans_segments_and_pruning_keeps_the_active_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EventLogConfig::new(dir.path())
        .with_segment_bytes(1)
        .with_limits(1, 0);
    let mut log = EventLog::open_with_config(&config, "conv-1", "writer-1").expect("open");
    for index in 0..5 {
        append_record(
            &mut log,
            kinds::USAGE_RECORDED,
            &format!("usage:{index}"),
            json!({"index": index}),
        );
    }
    let log_dir = log.dir.clone();
    assert_eq!(log.last_seq(), 6);
    drop(log);

    let remaining = segment_files(&log_dir).expect("segments");
    assert!(
        remaining.len() < 6,
        "old segments were pruned: {remaining:?}"
    );
    assert!(remaining.iter().any(|(index, _)| *index == 6));
    let view = read_conversation(dir.path(), "conv-1").expect("read");
    assert_eq!(view.last_seq, 6);
    assert!(view.records.iter().all(|record| record.seq >= 5));
}

#[test]
fn pruning_is_skipped_while_a_turn_is_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EventLogConfig::new(dir.path())
        .with_segment_bytes(1)
        .with_limits(1, 0);
    let mut log = EventLog::open_with_config(&config, "conv-1", "writer-1").expect("open");
    let log_dir = log.dir.clone();
    log.append(LogRecordDraft::new(
        kinds::TURN_STARTED,
        "turn:exchange-1",
        Actor::Bridge,
        json!({"source": "user"}),
    ).with_turn("turn-1").with_exchange("exchange-1"))
        .expect("turn started");
    let after_start = segment_files(&log_dir).expect("segments").len();
    assert!(after_start > 1, "an open turn keeps its history");
    log.append(LogRecordDraft::new(
        kinds::TURN_SETTLED,
        "turn.terminal:turn-1",
        Actor::Bridge,
        json!({"stop_reason": "end_turn"}),
    ).with_turn("turn-1"))
        .expect("turn settled");
    let after_settle = segment_files(&log_dir).expect("segments").len();
    assert!(
        after_settle < after_start,
        "once the turn settles the guard lifts"
    );
}

#[test]
fn read_conversation_works_while_a_writer_is_live() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut log = EventLog::open(dir.path(), "conv-1", "writer-1").expect("open");
    append_record(&mut log, kinds::TOOL_CALLS, "tool.calls:t1:e1:c1", json!({"name": "bash"}));
    let view = read_conversation(dir.path(), "conv-1").expect("read");
    assert_eq!(view.last_seq, 2);
    assert_eq!(view.records.len(), 2);
    assert_eq!(view.records[1].kind, kinds::TOOL_CALLS);
}

#[test]
fn verify_on_a_live_log_matches_its_own_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut log = EventLog::open(dir.path(), "conv-1", "writer-1").expect("open");
    append_record(&mut log, kinds::SESSION_OPENED, "session.opened:1", json!({}));
    let report = log.verify().expect("verify");
    assert_eq!(report.last_seq, 2);
    assert_eq!(report.generation, 1);
    assert!(report.gaps.is_empty());
    assert!(report.divergences.is_empty());
    assert!(!report.truncated_tail);
}

#[test]
fn delivery_modes_map_to_queue_sources() {
    assert_eq!(DeliveryMode::default(), DeliveryMode::Queue);
    assert!(DeliveryMode::CancelAndSendNow.is_destructive());
    assert!(!DeliveryMode::Queue.is_destructive());
    assert!(!DeliveryMode::Steer.is_destructive());
    assert_eq!(DeliveryMode::Queue.queue_source(), "queued");
    assert_eq!(DeliveryMode::Steer.queue_source(), "steer");
    assert_eq!(DeliveryMode::CancelAndSendNow.queue_source(), "immediate");
}
