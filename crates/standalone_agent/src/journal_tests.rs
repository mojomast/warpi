use super::*;

fn admit(id: &str, text: &str) -> JournalDraft {
    JournalDraft::PromptAdmitted {
        enqueue_id: id.to_string(),
        text: text.to_string(),
        origin: "queued".to_string(),
        delivery: Delivery::Queue,
    }
}

fn start(id: &str, turn: &str) -> JournalDraft {
    JournalDraft::PromptStarted {
        enqueue_id: id.to_string(),
        turn_id: turn.to_string(),
        exchange_id: format!("exchange-{turn}"),
        generation: 1,
    }
}

fn terminal(turn: &str) -> JournalDraft {
    JournalDraft::TurnTerminal {
        turn_id: turn.to_string(),
        outcome: TurnOutcome::Completed,
        code: None,
    }
}

fn raw_records(path: &Path) -> Vec<JournalRecord> {
    let raw = fs::read_to_string(path).expect("journal readable");
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("record parses"))
        .collect()
}

#[test]
fn admission_is_durable_and_rehydrates_after_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let mut journal = Journal::open(root, "conv-1").expect("open");
    let seq = journal.append(admit("q1", "first prompt")).expect("append");
    assert_eq!(seq, 1);
    drop(journal);

    let queue = Journal::read_queue(root, "conv-1").expect("read queue");
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].enqueue_id, "q1");
    assert_eq!(queue[0].text, "first prompt");
    assert_eq!(queue[0].enqueued_seq, 1);

    // A second admission keeps its place after the rehydrated one.
    let mut journal = Journal::open(root, "conv-1").expect("reopen");
    journal.append(admit("q2", "second prompt")).expect("append");
    drop(journal);
    let queue = Journal::read_queue(root, "conv-1").expect("read queue");
    let ids: Vec<&str> = queue.iter().map(|row| row.enqueue_id.as_str()).collect();
    assert_eq!(ids, vec!["q1", "q2"]);
}

#[test]
fn duplicate_admission_with_the_same_text_is_a_no_op() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let mut journal = Journal::open(root, "conv-1").expect("open");
    let first = journal.append(admit("q1", "hello")).expect("append");
    let second = journal.append(admit("q1", "hello")).expect("duplicate append");
    assert_eq!(first, second);
    assert_eq!(raw_records(&journal.path).len(), 1);
    assert_eq!(journal.state().max_enqueued_seq, 1);
}

#[test]
fn conflicting_admission_is_a_lifecycle_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut journal = Journal::open(dir.path(), "conv-1").expect("open");
    journal.append(admit("q1", "hello")).expect("append");
    let error = journal.append(admit("q1", "different")).unwrap_err();
    assert!(matches!(error, JournalError::Lifecycle(_)), "{error}");
    assert_eq!(raw_records(&journal.path).len(), 1);
}

#[test]
fn started_prompt_settles_on_turn_terminal_and_leaves_the_queue() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut journal = Journal::open(dir.path(), "conv-1").expect("open");
    journal.append(admit("q1", "hello")).expect("admit");
    journal.append(start("q1", "t1")).expect("start");
    assert!(journal.state().queued_prompts().is_empty());
    assert_eq!(journal.state().orphan_turn_id(), Some("t1"));
    journal.append(terminal("t1")).expect("terminal");
    assert_eq!(journal.state().orphan_turn_id(), None);
    let state = journal.state();
    assert_eq!(state.prompts["q1"].status, PromptStatus::Settled);
    assert_eq!(state.turns["t1"].terminal_seq, Some(3));
}

#[test]
fn cancelled_prompt_is_idempotent_and_never_starts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut journal = Journal::open(dir.path(), "conv-1").expect("open");
    journal.append(admit("q1", "hello")).expect("admit");
    journal
        .append(JournalDraft::PromptCancelled {
            enqueue_id: "q1".to_string(),
            reason: "user".to_string(),
        })
        .expect("cancel");
    let duplicate = journal
        .append(JournalDraft::PromptCancelled {
            enqueue_id: "q1".to_string(),
            reason: "user".to_string(),
        })
        .expect("duplicate cancel");
    assert_eq!(duplicate, 2);
    assert!(journal.state().queued_prompts().is_empty());
    let error = journal.append(start("q1", "t1")).unwrap_err();
    assert!(matches!(error, JournalError::Lifecycle(_)), "{error}");
}

#[test]
fn requeue_returns_a_failed_start_to_the_queue() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut journal = Journal::open(dir.path(), "conv-1").expect("open");
    journal.append(admit("q1", "hello")).expect("admit");
    journal.append(start("q1", "t1")).expect("start");
    journal
        .append(JournalDraft::PromptRequeued {
            enqueue_id: "q1".to_string(),
            reason: "send failed".to_string(),
        })
        .expect("requeue");
    let queue = journal.state().queued_prompts();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].enqueue_id, "q1");
    assert_eq!(journal.state().orphan_turn_id(), None);
}

#[test]
fn promotions_reorder_the_drain() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut journal = Journal::open(dir.path(), "conv-1").expect("open");
    for id in ["q1", "q2", "q3"] {
        journal.append(admit(id, id)).expect("admit");
    }
    journal
        .append(JournalDraft::PromptPromoted {
            enqueue_id: "q3".to_string(),
            position: PromotionPosition::Front,
        })
        .expect("promote front");
    let ids = |journal: &Journal| -> Vec<String> {
        journal
            .state()
            .queued_prompts()
            .iter()
            .map(|row| row.enqueue_id.clone())
            .collect()
    };
    assert_eq!(ids(&journal), vec!["q3", "q1", "q2"]);

    journal
        .append(JournalDraft::PromptPromoted {
            enqueue_id: "q2".to_string(),
            position: PromotionPosition::before("q1"),
        })
        .expect("promote before");
    assert_eq!(ids(&journal), vec!["q3", "q2", "q1"]);
}

#[test]
fn enqueued_seq_is_stable_across_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let mut journal = Journal::open(root, "conv-1").expect("open");
    journal.append(admit("q1", "a")).expect("admit");
    journal.append(admit("q2", "b")).expect("admit");
    journal
        .append(JournalDraft::PromptCancelled {
            enqueue_id: "q1".to_string(),
            reason: "user".to_string(),
        })
        .expect("cancel");
    drop(journal);

    let mut journal = Journal::open(root, "conv-1").expect("reopen");
    journal.append(admit("q3", "c")).expect("admit");
    let queue = journal.state().queued_prompts();
    let seqs: Vec<u64> = queue.iter().map(|row| row.enqueued_seq).collect();
    assert_eq!(seqs, vec![2, 3]);
}

#[test]
fn a_torn_tail_is_ignored_on_replay() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let mut journal = Journal::open(root, "conv-1").expect("open");
    journal.append(admit("q1", "hello")).expect("admit");
    let path = journal.path.clone();
    drop(journal);

    let mut bytes = fs::read(&path).expect("read");
    bytes.extend_from_slice(br#"{"seq":2,"ts":1,"body":{"kind":"prompt.ad"#);
    fs::write(&path, bytes).expect("write torn tail");

    let (state, report) = Journal::read_state_with_report(root, "conv-1").expect("read");
    assert!(report.truncated_tail);
    assert!(!report.corrupt);
    assert_eq!(state.last_seq, 1);

    let mut journal = Journal::open(root, "conv-1").expect("reopen");
    assert!(journal.report().truncated_tail);
    let seq = journal.append(admit("q2", "world")).expect("append");
    assert_eq!(seq, 2, "the torn tail did not consume a sequence number");
}

#[test]
fn non_tail_corruption_is_quarantined_and_the_valid_prefix_is_kept() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let path = journal_path(root, "conv-1");
    let good_one = JournalRecord {
        seq: 1,
        ts: 1,
        body: JournalBody::PromptAdmitted(PromptAdmitted {
            enqueue_id: "q1".to_string(),
            text: "hello".to_string(),
            origin: "queued".to_string(),
            enqueued_seq: 1,
            delivery: Delivery::Queue,
        }),
    };
    let good_three = JournalRecord {
        seq: 3,
        ts: 3,
        body: JournalBody::PromptAdmitted(PromptAdmitted {
            enqueue_id: "q3".to_string(),
            text: "later".to_string(),
            origin: "queued".to_string(),
            enqueued_seq: 3,
            delivery: Delivery::Queue,
        }),
    };
    let contents = format!(
        "{}\nnot json\n{}\n",
        serde_json::to_string(&good_one).unwrap(),
        serde_json::to_string(&good_three).unwrap()
    );
    fs::create_dir_all(root).unwrap();
    fs::write(&path, contents).unwrap();

    let mut journal = Journal::open(root, "conv-1").expect("open");
    assert!(journal.report().corrupt);
    assert!(journal.report().corrupt_path.is_some());
    let corrupt_path = journal.report().corrupt_path.clone().unwrap();
    assert!(corrupt_path.exists(), "the corrupt journal was renamed away");
    assert_eq!(journal.state().last_seq, 1);
    assert!(journal.state().prompts.contains_key("q1"));
    assert!(!journal.state().prompts.contains_key("q3"));

    let seq = journal.append(admit("q2", "next")).expect("append");
    assert_eq!(seq, 2, "the fresh journal continues from the kept prefix");
}

#[test]
fn compaction_writes_a_snapshot_and_replays_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let limits = JournalLimits {
        snapshot_records: 3,
        snapshot_bytes: u64::MAX,
    };
    let mut journal = Journal::open_with(root, "conv-1", limits).expect("open");
    journal.append(admit("q1", "a")).expect("admit");
    journal.append(start("q1", "t1")).expect("start");
    journal.append(terminal("t1")).expect("terminal");
    journal.append(admit("q2", "b")).expect("admit");
    assert!(snapshot_path(root, "conv-1").exists(), "snapshot written");
    let before = journal.state().clone();

    drop(journal);
    let mut reopened = Journal::open_with(root, "conv-1", limits).expect("reopen");
    assert!(reopened.report().snapshot_loaded);
    assert_eq!(reopened.state(), &before);
    let queue = reopened.state().queued_prompts();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].enqueue_id, "q2");
    assert_eq!(queue[0].enqueued_seq, 2);

    let seq = reopened.append(admit("q3", "c")).expect("append");
    assert_eq!(seq, 5, "sequence continues across the snapshot boundary");
}

#[test]
fn duplicate_terminals_are_dropped_and_first_terminal_wins() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let mut journal = Journal::open(root, "conv-1").expect("open");
    journal.append(admit("q1", "a")).expect("admit");
    journal.append(start("q1", "t1")).expect("start");
    journal
        .append(JournalDraft::ToolCall {
            turn_id: "t1".to_string(),
            tool_call_id: "c1".to_string(),
            name: "bash".to_string(),
            args_bytes: 10,
        })
        .expect("call");
    let first = journal
        .append(JournalDraft::ToolTerminal {
            turn_id: "t1".to_string(),
            tool_call_id: "c1".to_string(),
            status: "success".to_string(),
            source: ToolTerminalSource::AppResult,
        })
        .expect("terminal");
    let duplicate = journal
        .append(JournalDraft::ToolTerminal {
            turn_id: "t1".to_string(),
            tool_call_id: "c1".to_string(),
            status: "failure".to_string(),
            source: ToolTerminalSource::SynthesizedOrphan,
        })
        .expect("duplicate terminal");
    assert_eq!(first, duplicate);
    let call = &journal.state().tools["t1"]["c1"];
    assert_eq!(call.terminal.as_ref().unwrap().status, "success");
    assert_eq!(raw_records(&journal.path).len(), 4);
}

#[test]
fn every_journal_prefix_replays_with_one_terminal_per_aggregate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let mut journal = Journal::open(root, "conv-1").expect("open");
    let drafts = vec![
        admit("q1", "a"),
        start("q1", "t1"),
        JournalDraft::ToolCall {
            turn_id: "t1".to_string(),
            tool_call_id: "c1".to_string(),
            name: "bash".to_string(),
            args_bytes: 3,
        },
        JournalDraft::ToolCall {
            turn_id: "t1".to_string(),
            tool_call_id: "c2".to_string(),
            name: "read".to_string(),
            args_bytes: 4,
        },
        JournalDraft::ToolTerminal {
            turn_id: "t1".to_string(),
            tool_call_id: "c1".to_string(),
            status: "success".to_string(),
            source: ToolTerminalSource::AppResult,
        },
        JournalDraft::ToolTerminal {
            turn_id: "t1".to_string(),
            tool_call_id: "c2".to_string(),
            status: "failure".to_string(),
            source: ToolTerminalSource::SynthesizedDenied,
        },
        terminal("t1"),
        admit("q2", "b"),
    ];
    for draft in drafts {
        journal.append(draft).expect("append");
        let state = Journal::read_state(root, "conv-1").expect("replay prefix");
        for (turn_id, calls) in &state.tools {
            for (call_id, call) in calls {
                assert_eq!(
                    call.terminal.is_some(),
                    call.terminal_seq.is_some(),
                    "one terminal per {turn_id}/{call_id}"
                );
            }
        }
        for turn in state.turns.values() {
            if let Some(seq) = turn.terminal_seq {
                assert!(turn.terminal.is_some(), "turn {} terminal at {seq}", turn.turn_id);
            }
        }
    }
    assert_eq!(journal.state().last_seq, 8);
}

#[test]
fn crafted_duplicate_terminals_on_disk_still_derive_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let path = journal_path(root, "conv-1");
    let call = JournalRecord {
        seq: 2,
        ts: 2,
        body: JournalBody::ToolCall(ToolCall {
            turn_id: "t1".to_string(),
            tool_call_id: "c1".to_string(),
            name: "bash".to_string(),
            args_bytes: 1,
        }),
    };
    let first = JournalRecord {
        seq: 3,
        ts: 3,
        body: JournalBody::ToolTerminal(ToolTerminal {
            turn_id: "t1".to_string(),
            tool_call_id: "c1".to_string(),
            status: "success".to_string(),
            source: ToolTerminalSource::AppResult,
        }),
    };
    let duplicate = JournalRecord {
        seq: 4,
        ts: 4,
        body: JournalBody::ToolTerminal(ToolTerminal {
            turn_id: "t1".to_string(),
            tool_call_id: "c1".to_string(),
            status: "failure".to_string(),
            source: ToolTerminalSource::SynthesizedOrphan,
        }),
    };
    fs::create_dir_all(root).unwrap();
    let contents = [call, first, duplicate]
        .iter()
        .map(|record| serde_json::to_string(record).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, format!("{contents}\n")).unwrap();

    let state = Journal::read_state(root, "conv-1").expect("read");
    assert_eq!(state.last_seq, 4);
    let call = &state.tools["t1"]["c1"];
    assert_eq!(call.terminal.as_ref().unwrap().status, "success");
    assert_eq!(call.terminal_seq, Some(3));
}

#[test]
fn orphan_recovery_writes_honest_terminals_and_is_idempotent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut journal = Journal::open(dir.path(), "conv-1").expect("open");
    journal.append(admit("q1", "a")).expect("admit");
    journal.append(start("q1", "t1")).expect("start");
    journal
        .append(JournalDraft::ToolCall {
            turn_id: "t1".to_string(),
            tool_call_id: "c1".to_string(),
            name: "bash".to_string(),
            args_bytes: 3,
        })
        .expect("call");
    journal
        .append(JournalDraft::ToolCall {
            turn_id: "t1".to_string(),
            tool_call_id: "c2".to_string(),
            name: "read".to_string(),
            args_bytes: 4,
        })
        .expect("call");

    let mut repaired = BTreeMap::new();
    repaired.insert("c1".to_string(), RepairStatus::Repaired);
    let plan = journal.plan_orphan_recovery(&repaired).expect("plan");
    assert_eq!(plan.turn_id, "t1");
    assert!(!plan.poisoned);
    assert_eq!(plan.tool_terminals.len(), 2);
    assert_eq!(plan.tool_terminals[0].1, ToolTerminalSource::HelperRepaired);
    assert_eq!(plan.tool_terminals[1].1, ToolTerminalSource::SynthesizedOrphan);

    journal
        .apply_orphan_recovery(&plan, "previous turn interrupted")
        .expect("apply");
    let state = journal.state();
    assert_eq!(state.orphan_turn_id(), None);
    assert_eq!(
        state.tools["t1"]["c1"].terminal.as_ref().unwrap().source,
        ToolTerminalSource::HelperRepaired
    );
    assert_eq!(
        state.tools["t1"]["c2"].terminal.as_ref().unwrap().source,
        ToolTerminalSource::SynthesizedOrphan
    );
    assert_eq!(state.turns["t1"].terminal.as_ref().unwrap().outcome, TurnOutcome::OrphanReconciled);
    assert_eq!(state.recoveries["t1"].calls_failed, 2);
    assert_eq!(state.queued_prompts().len(), 0, "no prompt is auto-replayed");
}

#[test]
fn an_unrepaired_call_poisons_recovery() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut journal = Journal::open(dir.path(), "conv-1").expect("open");
    journal.append(admit("q1", "a")).expect("admit");
    journal.append(start("q1", "t1")).expect("start");
    journal
        .append(JournalDraft::ToolCall {
            turn_id: "t1".to_string(),
            tool_call_id: "c1".to_string(),
            name: "bash".to_string(),
            args_bytes: 3,
        })
        .expect("call");
    let mut repaired = BTreeMap::new();
    repaired.insert("c1".to_string(), RepairStatus::Unrepaired);
    let plan = journal.plan_orphan_recovery(&repaired).expect("plan");
    assert!(plan.poisoned);
}

#[test]
fn session_opened_is_the_latest_mapping() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut journal = Journal::open(dir.path(), "conv-1").expect("open");
    for generation in [1_u64, 2] {
        journal
            .append(JournalDraft::SessionOpened(SessionOpened {
                session_file: format!("pi/sessions/{generation}.jsonl"),
                generation,
                provider_id: "local".to_string(),
                model_id: "model".to_string(),
                working_dir: "/tmp".to_string(),
                repaired_tool_calls: BTreeMap::new(),
            }))
            .expect("session opened");
    }
    let latest = journal.state().latest_session().expect("session");
    assert_eq!(latest.generation, 2);
    assert_eq!(latest.session_file, "pi/sessions/2.jsonl");
}

#[test]
fn sweep_removes_stale_queue_free_journals() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let mut live = Journal::open(root, "live").expect("open");
    live.append(admit("q1", "a")).expect("admit");
    drop(live);
    let mut stale = Journal::open(root, "stale").expect("open");
    stale.append(admit("q1", "a")).expect("admit");
    stale
        .append(JournalDraft::PromptCancelled {
            enqueue_id: "q1".to_string(),
            reason: "user".to_string(),
        })
        .expect("cancel");
    drop(stale);

    let now = 1_000_000 * MILLIS_PER_DAY;
    let report = Journal::sweep(root, now, DEFAULT_GLOBAL_MAX_BYTES).expect("sweep");
    assert_eq!(report.scanned, 2);
    assert_eq!(report.removed, 1);
    assert!(journal_path(root, "live").exists());
    assert!(!journal_path(root, "stale").exists());
}

#[test]
fn sweep_respects_the_global_cap_without_removing_live_queues() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    for id in ["a", "b"] {
        let mut journal = Journal::open(root, id).expect("open");
        journal.append(admit("q1", "a")).expect("admit");
    }
    let report = Journal::sweep(root, MILLIS_PER_DAY, 1).expect("sweep");
    assert_eq!(report.removed, 0, "live queues are never pruned");
}
