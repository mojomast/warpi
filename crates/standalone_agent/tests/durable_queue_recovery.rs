//! End-to-end recovery of the durable queue (`durable-queue-recovery-spec.md` §3/§4).
//!
//! These exercise the store the way the bridge driver and the app's rehydration path
//! use it: admit prompts under one writer, drop it to simulate a crash, then rebuild
//! the queue from the journal on the next run. No GUI or helper is involved.

use std::collections::BTreeMap;

use standalone_agent::journal::{
    Delivery, DerivedState, Journal, JournalDraft, JournalError, PromptStatus, RepairStatus,
    TurnOutcome,
};

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

#[test]
fn accepted_prompts_survive_a_restart_and_rehydrate_in_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    {
        let mut journal = Journal::open(root, "conv-1").expect("open");
        journal.append(admit("q1", "first")).expect("admit");
        journal.append(admit("q2", "second")).expect("admit");
        journal.append(start("q1", "t1")).expect("start");
        // Process dies mid-turn: q1 is an orphan, q2 is still queued.
    }

    let state = Journal::read_state(root, "conv-1").expect("rehydrate");
    assert_eq!(state.orphan_turn_id(), Some("t1"));
    let queued_rows = state.queued_prompts();
    let queued: Vec<&str> = queued_rows
        .iter()
        .map(|row| row.enqueue_id.as_str())
        .collect();
    assert_eq!(queued, vec!["q2"]);
    assert_eq!(state.prompts["q1"].status, PromptStatus::Running);

    // Recovery closes the orphan without replaying the prompt.
    let mut journal = Journal::open(root, "conv-1").expect("reopen");
    let plan = journal
        .plan_orphan_recovery(&BTreeMap::new())
        .expect("orphan plan");
    journal
        .apply_orphan_recovery(&plan, "interrupted while starting")
        .expect("apply recovery");
    assert_eq!(journal.state().orphan_turn_id(), None);
    assert_eq!(
        journal.state().turns["t1"].terminal.as_ref().unwrap().outcome,
        TurnOutcome::OrphanReconciled
    );

    // A newly typed prompt drains after the rehydrated one.
    journal.append(admit("q3", "third")).expect("admit");
    let order_rows = journal.state().queued_prompts();
    let order: Vec<&str> = order_rows
        .iter()
        .map(|row| row.enqueue_id.as_str())
        .collect();
    assert_eq!(order, vec!["q2", "q3"]);
}

#[test]
fn a_crash_between_admission_and_start_does_not_lose_the_prompt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    let mut journal = Journal::open(root, "conv-1").expect("open");
    journal.append(admit("q1", "not yet started")).expect("admit");
    drop(journal);

    // §7.2 kill point 2: admitted with no started; the prompt rehydrates as queued.
    let queue = Journal::read_queue(root, "conv-1").expect("rehydrate");
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].enqueue_id, "q1");

    // The app re-dispatches the same id; first admission wins, no duplicate.
    let mut journal = Journal::open(root, "conv-1").expect("reopen");
    let before = journal.last_seq();
    let seq = journal.append(admit("q1", "not yet started")).expect("retry");
    assert_eq!(seq, before, "re-sent admission returns the original seq");
    assert_eq!(journal.state().prompts.len(), 1);
}

#[test]
fn recovery_is_idempotent_when_reentered() {
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
            args_bytes: 4,
        })
        .expect("call");
    let plan = journal
        .plan_orphan_recovery(&BTreeMap::new())
        .expect("plan");
    journal.apply_orphan_recovery(&plan, "note").expect("apply");
    let settled = journal.state().clone();
    drop(journal);

    let journal = Journal::open(root, "conv-1").expect("reopen");
    assert!(
        journal.plan_orphan_recovery(&BTreeMap::new()).is_none(),
        "a settled turn is not re-reconciled"
    );
    assert_eq!(journal.state(), &settled);
    assert_eq!(journal.state().recoveries.len(), 1);
}

#[test]
fn a_cancelled_queued_prompt_never_starts_after_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    let mut journal = Journal::open(root, "conv-1").expect("open");
    journal.append(admit("q1", "cancel me")).expect("admit");
    journal
        .append(JournalDraft::PromptCancelled {
            enqueue_id: "q1".to_string(),
            reason: "user".to_string(),
        })
        .expect("cancel");
    drop(journal);

    let state: DerivedState = Journal::read_state(root, "conv-1").expect("rehydrate");
    assert!(state.queued_prompts().is_empty());
    assert_eq!(state.prompts["q1"].status, PromptStatus::Cancelled);

    let mut journal = Journal::open(root, "conv-1").expect("reopen");
    let error = journal.append(start("q1", "t1")).unwrap_err();
    assert!(matches!(error, JournalError::Lifecycle(_)));
}

#[test]
fn repaired_orphan_calls_are_recorded_as_helper_repaired() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    let mut journal = Journal::open(root, "conv-1").expect("open");
    journal.append(admit("q1", "a")).expect("admit");
    journal.append(start("q1", "t1")).expect("start");
    journal
        .append(JournalDraft::ToolCall {
            turn_id: "t1".to_string(),
            tool_call_id: "c1".to_string(),
            name: "read".to_string(),
            args_bytes: 1,
        })
        .expect("call");
    let mut repaired = BTreeMap::new();
    repaired.insert("c1".to_string(), RepairStatus::Repaired);
    let plan = journal.plan_orphan_recovery(&repaired).expect("plan");
    assert!(!plan.poisoned);
    journal.apply_orphan_recovery(&plan, "note").expect("apply");
    assert_eq!(
        journal.state().tools["t1"]["c1"]
            .terminal
            .as_ref()
            .unwrap()
            .source,
        standalone_agent::journal::ToolTerminalSource::HelperRepaired
    );
}
