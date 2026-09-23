//! Durable per-conversation prompt journal and startup recovery.
//!
//! This is the pure-Rust store half of `standalone/research/durable-queue-recovery-spec.md`
//! (§2 durable store, §2.4 write/fsync policy, §2.5 rotation and compaction, §3 placement
//! rules, §4.3–§4.4 reconciliation and invariants). It has no GUI or bridge dependency: the
//! caller injects the queue root (the app passes `StandaloneConfig::data_dir()/queue`) and the
//! driver owns the single writer for a conversation.
//!
//! Layout (`§2.1`):
//!
//! - `<root>/<conversation_id>.journal.jsonl` — one JSON record per line, append-only.
//! - `<root>/<conversation_id>.snapshot.json` — optional rolled-up state at a sequence.
//! - `<root>/<conversation_id>.journal.jsonl.corrupt` — a journal that failed a non-tail parse.
//!
//! Every record carries `seq` (per-conversation, strictly increasing) and `ts` (Unix millis).
//! The record payload is nested under `body` keyed by `kind`; see [`JournalBody`]. Writes
//! `sync_data` before the in-memory state advances, so no acked state change is ever lost
//! (`§2.4`).

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Schema version written into each snapshot.
pub const JOURNAL_VERSION: u32 = 1;
/// Compact once this many records have been appended since the last snapshot.
pub const DEFAULT_SNAPSHOT_RECORDS: usize = 500;
/// Compact once the active journal passes this size.
pub const DEFAULT_SNAPSHOT_BYTES: u64 = 512 * 1024;
/// Conversations untouched for this many days are swept (`§2.5`).
pub const DEFAULT_RETENTION_DAYS: u64 = 90;
/// Global byte cap across all conversation journals (`§2.5`).
pub const DEFAULT_GLOBAL_MAX_BYTES: u64 = 50 * 1024 * 1024;

const MILLIS_PER_DAY: i64 = 86_400_000;
const JOURNAL_SUFFIX: &str = ".journal.jsonl";
const SNAPSHOT_SUFFIX: &str = ".snapshot.json";

/// Root directory holding every conversation journal for a standalone data dir.
pub fn queue_dir(standalone_data_dir: &Path) -> PathBuf {
    standalone_data_dir.join("queue")
}

/// The journal path for one conversation.
pub fn journal_path(root: &Path, conversation_id: &str) -> PathBuf {
    root.join(format!("{conversation_id}{JOURNAL_SUFFIX}"))
}

/// The snapshot path for one conversation.
pub fn snapshot_path(root: &Path, conversation_id: &str) -> PathBuf {
    root.join(format!("{conversation_id}{SNAPSHOT_SUFFIX}"))
}

/// Which queue a prompt uses when it is admitted (`§2.2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Start when the current turn is idle (the default).
    Queue,
    /// Send-now intent: promote to the front when the next turn is chosen.
    Immediate,
}

/// Outcome of a helper transcript repair for one dangling tool call (`§4.2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairStatus {
    Repaired,
    Unrepaired,
}

/// Where a durable tool terminal came from (`§2.2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolTerminalSource {
    AppResult,
    SynthesizedUntranslatable,
    SynthesizedDenied,
    SynthesizedOrphan,
    HelperRepaired,
}

/// A durable turn terminal outcome (`§2.2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    Failed,
    Cancelled,
    Expired,
    HelperExited,
    ProtocolError,
    OrphanReconciled,
}

/// Where a promoted prompt moves to (`§2.2`, `§2.3`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromotionPosition {
    Front,
    Before(String),
}

impl PromotionPosition {
    pub fn front() -> Self {
        Self::Front
    }

    pub fn before(id: impl Into<String>) -> Self {
        Self::Before(id.into())
    }

    /// The wire form stored in the journal: `front` or `before:<id>`.
    pub fn wire(&self) -> String {
        match self {
            Self::Front => "front".to_string(),
            Self::Before(id) => format!("before:{id}"),
        }
    }

    pub fn parse_wire(value: &str) -> Option<Self> {
        match value {
            "front" => Some(Self::Front),
            other => other.strip_prefix("before:").map(|id| Self::Before(id.to_string())),
        }
    }
}

impl Serialize for PromotionPosition {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.wire())
    }
}

impl<'de> Deserialize<'de> for PromotionPosition {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse_wire(&value).ok_or_else(|| serde::de::Error::custom("invalid promotion position"))
    }
}

/// `prompt.admitted` payload (`§2.2`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptAdmitted {
    pub enqueue_id: String,
    pub text: String,
    pub origin: String,
    pub enqueued_seq: u64,
    pub delivery: Delivery,
}

/// `prompt.started` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptStarted {
    pub enqueue_id: String,
    pub turn_id: String,
    pub exchange_id: String,
    pub generation: u64,
}

/// `prompt.requeued` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptRequeued {
    pub enqueue_id: String,
    pub reason: String,
}

/// `prompt.cancelled` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCancelled {
    pub enqueue_id: String,
    pub reason: String,
}

/// `prompt.promoted` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptPromoted {
    pub enqueue_id: String,
    pub position: PromotionPosition,
}

/// `tool.call` payload. Arguments are never stored, only their size (`§2.5`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub turn_id: String,
    pub tool_call_id: String,
    pub name: String,
    pub args_bytes: u64,
}

/// `tool.terminal` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolTerminal {
    pub turn_id: String,
    pub tool_call_id: String,
    pub status: String,
    pub source: ToolTerminalSource,
}

/// `turn.terminal` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnTerminal {
    pub turn_id: String,
    pub outcome: TurnOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// `session.opened` payload; the read replacement for `session-map.json` (`§4.3`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOpened {
    pub session_file: String,
    pub generation: u64,
    pub provider_id: String,
    pub model_id: String,
    pub working_dir: String,
    #[serde(default)]
    pub repaired_tool_calls: BTreeMap<String, RepairStatus>,
}

/// `recovery.applied` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryApplied {
    pub turn_id: String,
    pub calls_failed: u64,
    pub note: String,
}

/// The kind-tagged payload of one journal record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum JournalBody {
    #[serde(rename = "prompt.admitted")]
    PromptAdmitted(PromptAdmitted),
    #[serde(rename = "prompt.started")]
    PromptStarted(PromptStarted),
    #[serde(rename = "prompt.requeued")]
    PromptRequeued(PromptRequeued),
    #[serde(rename = "prompt.cancelled")]
    PromptCancelled(PromptCancelled),
    #[serde(rename = "prompt.promoted")]
    PromptPromoted(PromptPromoted),
    #[serde(rename = "tool.call")]
    ToolCall(ToolCall),
    #[serde(rename = "tool.terminal")]
    ToolTerminal(ToolTerminal),
    #[serde(rename = "turn.terminal")]
    TurnTerminal(TurnTerminal),
    #[serde(rename = "session.opened")]
    SessionOpened(SessionOpened),
    #[serde(rename = "recovery.applied")]
    RecoveryApplied(RecoveryApplied),
}

/// One durable line: envelope fields plus a [`JournalBody`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalRecord {
    pub seq: u64,
    pub ts: i64,
    pub body: JournalBody,
}

/// A caller-supplied state change; the journal assigns `seq`, `ts`, and `enqueued_seq`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalDraft {
    PromptAdmitted {
        enqueue_id: String,
        text: String,
        origin: String,
        delivery: Delivery,
    },
    PromptStarted {
        enqueue_id: String,
        turn_id: String,
        exchange_id: String,
        generation: u64,
    },
    PromptRequeued {
        enqueue_id: String,
        reason: String,
    },
    PromptCancelled {
        enqueue_id: String,
        reason: String,
    },
    PromptPromoted {
        enqueue_id: String,
        position: PromotionPosition,
    },
    ToolCall {
        turn_id: String,
        tool_call_id: String,
        name: String,
        args_bytes: u64,
    },
    ToolTerminal {
        turn_id: String,
        tool_call_id: String,
        status: String,
        source: ToolTerminalSource,
    },
    TurnTerminal {
        turn_id: String,
        outcome: TurnOutcome,
        code: Option<String>,
    },
    SessionOpened(SessionOpened),
    RecoveryApplied {
        turn_id: String,
        calls_failed: u64,
        note: String,
    },
}

/// Derived lifecycle state of one prompt (`§2.2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PromptStatus {
    #[default]
    Admitted,
    Running,
    Settled,
    Cancelled,
}

/// The journal-derived state of one admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptState {
    pub enqueue_id: String,
    pub text: String,
    pub origin: String,
    pub enqueued_seq: u64,
    pub delivery: Delivery,
    pub status: PromptStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exchange_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promotion: Option<PromotionPosition>,
    pub admitted_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promotion_seq: Option<u64>,
}

/// The journal-derived state of one turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnState {
    pub turn_id: String,
    pub started_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enqueue_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TurnTerminal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_seq: Option<u64>,
}

/// The journal-derived state of one tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolState {
    pub turn_id: String,
    pub tool_call_id: String,
    pub name: String,
    pub args_bytes: u64,
    pub call_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<ToolTerminal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_seq: Option<u64>,
}

/// A queue row rebuilt from the journal (`§3.2` rehydration).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedPrompt {
    pub enqueue_id: String,
    pub text: String,
    pub origin: String,
    pub enqueued_seq: u64,
    pub delivery: Delivery,
}

/// Full derived state; also the snapshot payload (`§2.5`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedState {
    pub last_seq: u64,
    pub max_enqueued_seq: u64,
    #[serde(default)]
    pub prompts: BTreeMap<String, PromptState>,
    #[serde(default)]
    pub turns: BTreeMap<String, TurnState>,
    #[serde(default)]
    pub tools: BTreeMap<String, BTreeMap<String, ToolState>>,
    #[serde(default)]
    pub sessions: Vec<SessionOpened>,
    #[serde(default)]
    pub recoveries: BTreeMap<String, RecoveryApplied>,
}

impl DerivedState {
    /// The latest `session.opened` (last record wins, `§2.2`).
    pub fn latest_session(&self) -> Option<&SessionOpened> {
        self.sessions.last()
    }

    /// Queued rows in effective drain order (`§2.3`): `enqueued_seq` order with
    /// `prompt.promoted` applied in record order, last writer wins.
    pub fn queued_prompts(&self) -> Vec<QueuedPrompt> {
        let mut admitted: Vec<&PromptState> = self
            .prompts
            .values()
            .filter(|prompt| prompt.status == PromptStatus::Admitted)
            .collect();
        admitted.sort_by_key(|prompt| prompt.enqueued_seq);
        let mut order: Vec<String> = admitted
            .iter()
            .map(|prompt| prompt.enqueue_id.clone())
            .collect();

        let mut promoted: Vec<&PromptState> = self
            .prompts
            .values()
            .filter(|prompt| prompt.status == PromptStatus::Admitted && prompt.promotion.is_some())
            .collect();
        promoted.sort_by_key(|prompt| prompt.promotion_seq);
        for prompt in promoted {
            let Some(position) = prompt.promotion.as_ref() else {
                continue;
            };
            order.retain(|id| id != &prompt.enqueue_id);
            match position {
                PromotionPosition::Front => order.insert(0, prompt.enqueue_id.clone()),
                PromotionPosition::Before(target) => {
                    if let Some(index) = order.iter().position(|id| id == target) {
                        order.insert(index, prompt.enqueue_id.clone());
                    } else {
                        order.push(prompt.enqueue_id.clone());
                    }
                }
            }
        }

        order
            .iter()
            .filter_map(|id| self.prompts.get(id))
            .map(|prompt| QueuedPrompt {
                enqueue_id: prompt.enqueue_id.clone(),
                text: prompt.text.clone(),
                origin: prompt.origin.clone(),
                enqueued_seq: prompt.enqueued_seq,
                delivery: prompt.delivery,
            })
            .collect()
    }

    /// The at-most-one turn that has started without a durable terminal (`§4.3`).
    pub fn orphan_turn_id(&self) -> Option<&str> {
        self.turns
            .values()
            .filter(|turn| turn.terminal_seq.is_none() && turn.started_seq != 0)
            .max_by_key(|turn| turn.started_seq)
            .map(|turn| turn.turn_id.as_str())
    }

    /// Tool calls of the orphan turn with no durable terminal (`§4.3`).
    pub fn orphan_calls(&self) -> Vec<(&str, &ToolState)> {
        let Some(turn_id) = self.orphan_turn_id() else {
            return Vec::new();
        };
        self.tools
            .get(turn_id)
            .map(|calls| {
                calls
                    .values()
                    .filter(|call| call.terminal_seq.is_none())
                    .map(|call| (turn_id, call))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn apply(&mut self, record: &JournalRecord) {
        if record.seq > self.last_seq {
            self.last_seq = record.seq;
        }
        match &record.body {
            JournalBody::PromptAdmitted(payload) => {
                self.max_enqueued_seq = self.max_enqueued_seq.max(payload.enqueued_seq);
                self.prompts
                    .entry(payload.enqueue_id.clone())
                    .or_insert_with(|| PromptState {
                        enqueue_id: payload.enqueue_id.clone(),
                        text: payload.text.clone(),
                        origin: payload.origin.clone(),
                        enqueued_seq: payload.enqueued_seq,
                        delivery: payload.delivery,
                        status: PromptStatus::Admitted,
                        turn_id: None,
                        exchange_id: None,
                        generation: None,
                        cancelled_reason: None,
                        promotion: None,
                        admitted_seq: record.seq,
                        started_seq: None,
                        cancelled_seq: None,
                        settled_seq: None,
                        promotion_seq: None,
                    });
            }
            JournalBody::PromptStarted(payload) => {
                if let Some(prompt) = self.prompts.get_mut(&payload.enqueue_id)
                    && prompt.status == PromptStatus::Admitted
                {
                    prompt.status = PromptStatus::Running;
                    prompt.turn_id = Some(payload.turn_id.clone());
                    prompt.exchange_id = Some(payload.exchange_id.clone());
                    prompt.generation = Some(payload.generation);
                    prompt.started_seq = Some(record.seq);
                }
                self.turns
                    .entry(payload.turn_id.clone())
                    .or_insert_with(|| TurnState {
                        turn_id: payload.turn_id.clone(),
                        started_seq: record.seq,
                        enqueue_id: Some(payload.enqueue_id.clone()),
                        terminal: None,
                        terminal_seq: None,
                    });
            }
            JournalBody::PromptRequeued(payload) => {
                let running_turn = self
                    .prompts
                    .get(&payload.enqueue_id)
                    .filter(|prompt| prompt.status == PromptStatus::Running)
                    .and_then(|prompt| prompt.turn_id.clone());
                if let Some(prompt) = self.prompts.get_mut(&payload.enqueue_id)
                    && prompt.status == PromptStatus::Running
                {
                    prompt.status = PromptStatus::Admitted;
                    prompt.turn_id = None;
                    prompt.exchange_id = None;
                    prompt.generation = None;
                    prompt.started_seq = None;
                }
                if let Some(turn_id) = running_turn
                    && self
                        .turns
                        .get(&turn_id)
                        .is_some_and(|turn| turn.terminal_seq.is_none())
                {
                    self.turns.remove(&turn_id);
                }
            }
            JournalBody::PromptCancelled(payload) => {
                if let Some(prompt) = self.prompts.get_mut(&payload.enqueue_id)
                    && prompt.status == PromptStatus::Admitted
                {
                    prompt.status = PromptStatus::Cancelled;
                    prompt.cancelled_reason = Some(payload.reason.clone());
                    prompt.cancelled_seq = Some(record.seq);
                }
            }
            JournalBody::PromptPromoted(payload) => {
                if let Some(prompt) = self.prompts.get_mut(&payload.enqueue_id) {
                    prompt.promotion = Some(payload.position.clone());
                    prompt.promotion_seq = Some(record.seq);
                }
            }
            JournalBody::ToolCall(payload) => {
                let calls = self.tools.entry(payload.turn_id.clone()).or_default();
                calls
                    .entry(payload.tool_call_id.clone())
                    .or_insert_with(|| ToolState {
                        turn_id: payload.turn_id.clone(),
                        tool_call_id: payload.tool_call_id.clone(),
                        name: payload.name.clone(),
                        args_bytes: payload.args_bytes,
                        call_seq: record.seq,
                        terminal: None,
                        terminal_seq: None,
                    });
            }
            JournalBody::ToolTerminal(payload) => {
                if let Some(calls) = self.tools.get_mut(&payload.turn_id)
                    && let Some(call) = calls.get_mut(&payload.tool_call_id)
                    && call.terminal_seq.is_none()
                {
                    call.terminal = Some(payload.clone());
                    call.terminal_seq = Some(record.seq);
                }
            }
            JournalBody::TurnTerminal(payload) => {
                let turn = self
                    .turns
                    .entry(payload.turn_id.clone())
                    .or_insert_with(|| TurnState {
                        turn_id: payload.turn_id.clone(),
                        started_seq: 0,
                        enqueue_id: None,
                        terminal: None,
                        terminal_seq: None,
                    });
                if turn.terminal_seq.is_none() {
                    turn.terminal = Some(payload.clone());
                    turn.terminal_seq = Some(record.seq);
                }
                for prompt in self.prompts.values_mut() {
                    if prompt.status == PromptStatus::Running
                        && prompt.turn_id.as_deref() == Some(payload.turn_id.as_str())
                    {
                        prompt.status = PromptStatus::Settled;
                        prompt.settled_seq = Some(record.seq);
                    }
                }
            }
            JournalBody::SessionOpened(payload) => {
                self.sessions.push(payload.clone());
            }
            JournalBody::RecoveryApplied(payload) => {
                self.recoveries
                    .entry(payload.turn_id.clone())
                    .or_insert_with(|| payload.clone());
            }
        }
    }
}

/// A plan to close an interrupted turn; produced from derived state and the
/// helper's repair report (`§4.3`), applied as durable records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryPlan {
    pub turn_id: String,
    pub tool_terminals: Vec<(String, ToolTerminalSource)>,
    pub poisoned: bool,
}

/// Report returned when a [`Journal`] is opened (`§2.4`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JournalOpenReport {
    pub snapshot_loaded: bool,
    pub snapshot_corrupt: bool,
    pub records_replayed: usize,
    pub truncated_tail: bool,
    pub corrupt: bool,
    pub corrupt_path: Option<PathBuf>,
}

/// Compaction thresholds (`§2.5`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalLimits {
    pub snapshot_records: usize,
    pub snapshot_bytes: u64,
}

impl Default for JournalLimits {
    fn default() -> Self {
        Self {
            snapshot_records: DEFAULT_SNAPSHOT_RECORDS,
            snapshot_bytes: DEFAULT_SNAPSHOT_BYTES,
        }
    }
}

/// Result of sweeping a queue root (`§2.5`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub scanned: usize,
    pub removed: usize,
    pub bytes_removed: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("journal io error: {0}")]
    Io(#[from] io::Error),
    #[error("journal encode error: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("journal lifecycle error: {0}")]
    Lifecycle(String),
    #[error("unknown prompt: {0}")]
    UnknownPrompt(String),
}

enum ValidatedAppend {
    Write(JournalBody),
    NoOp(u64),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct JournalSnapshot {
    version: u32,
    conversation_id: String,
    seq: u64,
    state: DerivedState,
}

/// Single-writer append handle plus the derived state for one conversation.
pub struct Journal {
    root: PathBuf,
    conversation_id: String,
    path: PathBuf,
    snapshot: PathBuf,
    file: File,
    state: DerivedState,
    records_since_snapshot: usize,
    journal_bytes: u64,
    limits: JournalLimits,
    report: JournalOpenReport,
}

impl Journal {
    /// Open (creating if needed) the journal for `conversation_id` under `root`.
    pub fn open(root: impl AsRef<Path>, conversation_id: impl Into<String>) -> Result<Self, JournalError> {
        Self::open_with(root, conversation_id, JournalLimits::default())
    }

    /// [`Self::open`] with explicit compaction thresholds.
    pub fn open_with(
        root: impl AsRef<Path>,
        conversation_id: impl Into<String>,
        limits: JournalLimits,
    ) -> Result<Self, JournalError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        let conversation_id = conversation_id.into();
        let path = journal_path(&root, &conversation_id);
        let snapshot = snapshot_path(&root, &conversation_id);

        let mut state = DerivedState::default();
        let mut report = JournalOpenReport::default();
        match fs::read_to_string(&snapshot) {
            Ok(raw) => match serde_json::from_str::<JournalSnapshot>(&raw) {
                Ok(loaded) => {
                    state = loaded.state;
                    state.last_seq = state.last_seq.max(loaded.seq);
                    report.snapshot_loaded = true;
                }
                Err(error) => {
                    tracing::warn!(path = %snapshot.display(), error = %error, "ignoring corrupt journal snapshot");
                    report.snapshot_corrupt = true;
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        let journal_bytes = Self::replay_file(&path, &mut state, &mut report, true)?;

        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            root,
            conversation_id,
            path,
            snapshot,
            file,
            state,
            records_since_snapshot: report.records_replayed,
            journal_bytes,
            limits,
            report,
        })
    }

    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    pub fn last_seq(&self) -> u64 {
        self.state.last_seq
    }

    pub fn state(&self) -> &DerivedState {
        &self.state
    }

    pub fn report(&self) -> &JournalOpenReport {
        &self.report
    }

    /// Append a state change, fsync it, and only then advance memory (`§2.4`).
    /// Returns the durable `seq`; idempotent drafts return the original `seq`.
    pub fn append(&mut self, draft: JournalDraft) -> Result<u64, JournalError> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or_default();
        self.append_at(draft, ts)
    }

    /// [`Self::append`] with an explicit timestamp, for tests.
    pub fn append_at(&mut self, draft: JournalDraft, ts: i64) -> Result<u64, JournalError> {
        match self.validate(draft)? {
            ValidatedAppend::NoOp(seq) => Ok(seq),
            ValidatedAppend::Write(body) => {
                let record = JournalRecord {
                    seq: self.state.last_seq + 1,
                    ts,
                    body,
                };
                self.write_record(&record)?;
                self.state.apply(&record);
                self.records_since_snapshot += 1;
                self.compact_if_needed()?;
                Ok(record.seq)
            }
        }
    }

    /// Roll the current state into a snapshot and start a fresh journal (`§2.5`).
    pub fn compact(&mut self) -> Result<(), JournalError> {
        let snapshot = JournalSnapshot {
            version: JOURNAL_VERSION,
            conversation_id: self.conversation_id.clone(),
            seq: self.state.last_seq,
            state: self.state.clone(),
        };
        let raw = serde_json::to_vec(&snapshot)?;
        let tmp = PathBuf::from(format!("{}.tmp", self.snapshot.display()));
        {
            let mut file = File::create(&tmp)?;
            file.write_all(&raw)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, &self.snapshot)?;
        sync_dir(&self.root)?;

        let rotated = PathBuf::from(format!("{}.1", self.path.display()));
        let _ = fs::remove_file(&rotated);
        fs::rename(&self.path, &rotated)?;
        fs::remove_file(&rotated)?;
        self.file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        self.records_since_snapshot = 0;
        self.journal_bytes = 0;
        Ok(())
    }

    /// Build a recovery plan for the orphan turn (`§4.3`). `repaired` is the
    /// helper's `repaired_tool_calls` report; missing ids are treated as
    /// outcome-unknown.
    pub fn plan_orphan_recovery(
        &self,
        repaired: &BTreeMap<String, RepairStatus>,
    ) -> Option<RecoveryPlan> {
        let turn_id = self.state.orphan_turn_id()?.to_string();
        let mut poisoned = false;
        let mut tool_terminals = Vec::new();
        for (_, call) in self.state.orphan_calls() {
            let source = match repaired.get(&call.tool_call_id) {
                Some(RepairStatus::Repaired) => ToolTerminalSource::HelperRepaired,
                Some(RepairStatus::Unrepaired) => {
                    poisoned = true;
                    ToolTerminalSource::SynthesizedOrphan
                }
                None => ToolTerminalSource::SynthesizedOrphan,
            };
            tool_terminals.push((call.tool_call_id.clone(), source));
        }
        Some(RecoveryPlan {
            turn_id,
            tool_terminals,
            poisoned,
        })
    }

    /// Write the outcomes that close an interrupted turn: one terminal per call,
    /// the turn terminal, and the audit record (`§4.3`, `§4.4`). No prompt is
    /// replayed and no tool is re-issued.
    pub fn apply_orphan_recovery(
        &mut self,
        plan: &RecoveryPlan,
        note: &str,
    ) -> Result<(), JournalError> {
        let calls_failed = plan.tool_terminals.len() as u64;
        for (tool_call_id, source) in &plan.tool_terminals {
            self.append(JournalDraft::ToolTerminal {
                turn_id: plan.turn_id.clone(),
                tool_call_id: tool_call_id.clone(),
                status: "failed".to_string(),
                source: *source,
            })?;
        }
        self.append(JournalDraft::TurnTerminal {
            turn_id: plan.turn_id.clone(),
            outcome: TurnOutcome::OrphanReconciled,
            code: None,
        })?;
        self.append(JournalDraft::RecoveryApplied {
            turn_id: plan.turn_id.clone(),
            calls_failed,
            note: note.to_string(),
        })?;
        Ok(())
    }

    fn validate(&self, draft: JournalDraft) -> Result<ValidatedAppend, JournalError> {
        match draft {
            JournalDraft::PromptAdmitted {
                enqueue_id,
                text,
                origin,
                delivery,
            } => {
                if let Some(existing) = self.state.prompts.get(&enqueue_id) {
                    if existing.text == text {
                        return Ok(ValidatedAppend::NoOp(existing.admitted_seq));
                    }
                    return Err(JournalError::Lifecycle(format!(
                        "enqueue id {enqueue_id} was reused with different text"
                    )));
                }
                Ok(ValidatedAppend::Write(JournalBody::PromptAdmitted(
                    PromptAdmitted {
                        enqueue_id,
                        text,
                        origin,
                        enqueued_seq: self.state.max_enqueued_seq + 1,
                        delivery,
                    },
                )))
            }
            JournalDraft::PromptStarted {
                enqueue_id,
                turn_id,
                exchange_id,
                generation,
            } => {
                let prompt = self
                    .state
                    .prompts
                    .get(&enqueue_id)
                    .ok_or_else(|| JournalError::UnknownPrompt(enqueue_id.clone()))?;
                match prompt.status {
                    PromptStatus::Admitted => {}
                    PromptStatus::Running => {
                        if prompt.turn_id.as_deref() == Some(turn_id.as_str()) {
                            return Ok(ValidatedAppend::NoOp(
                                prompt.started_seq.unwrap_or(prompt.admitted_seq),
                            ));
                        }
                        return Err(JournalError::Lifecycle(format!(
                            "prompt {enqueue_id} already started with a different turn"
                        )));
                    }
                    PromptStatus::Cancelled => {
                        return Err(JournalError::Lifecycle(format!(
                            "prompt {enqueue_id} was cancelled and cannot start"
                        )));
                    }
                    PromptStatus::Settled => {
                        return Err(JournalError::Lifecycle(format!(
                            "prompt {enqueue_id} already settled"
                        )));
                    }
                }
                Ok(ValidatedAppend::Write(JournalBody::PromptStarted(
                    PromptStarted {
                        enqueue_id,
                        turn_id,
                        exchange_id,
                        generation,
                    },
                )))
            }
            JournalDraft::PromptRequeued { enqueue_id, reason } => {
                let prompt = self
                    .state
                    .prompts
                    .get(&enqueue_id)
                    .ok_or_else(|| JournalError::UnknownPrompt(enqueue_id.clone()))?;
                match prompt.status {
                    PromptStatus::Running => Ok(ValidatedAppend::Write(JournalBody::PromptRequeued(
                        PromptRequeued { enqueue_id, reason },
                    ))),
                    PromptStatus::Admitted => Ok(ValidatedAppend::NoOp(prompt.admitted_seq)),
                    PromptStatus::Cancelled | PromptStatus::Settled => {
                        Err(JournalError::Lifecycle(format!(
                            "prompt {enqueue_id} cannot be requeued from {:?}",
                            prompt.status
                        )))
                    }
                }
            }
            JournalDraft::PromptCancelled { enqueue_id, reason } => {
                let prompt = self
                    .state
                    .prompts
                    .get(&enqueue_id)
                    .ok_or_else(|| JournalError::UnknownPrompt(enqueue_id.clone()))?;
                match prompt.status {
                    PromptStatus::Admitted => Ok(ValidatedAppend::Write(JournalBody::PromptCancelled(
                        PromptCancelled { enqueue_id, reason },
                    ))),
                    PromptStatus::Cancelled => Ok(ValidatedAppend::NoOp(
                        prompt.cancelled_seq.unwrap_or(prompt.admitted_seq),
                    )),
                    PromptStatus::Running | PromptStatus::Settled => Err(JournalError::Lifecycle(
                        format!("prompt {enqueue_id} has already started and cannot be cancelled while queued"),
                    )),
                }
            }
            JournalDraft::PromptPromoted { enqueue_id, position } => {
                let prompt = self
                    .state
                    .prompts
                    .get(&enqueue_id)
                    .ok_or_else(|| JournalError::UnknownPrompt(enqueue_id.clone()))?;
                if prompt.status != PromptStatus::Admitted {
                    return Err(JournalError::Lifecycle(format!(
                        "prompt {enqueue_id} is not queued and cannot be promoted"
                    )));
                }
                Ok(ValidatedAppend::Write(JournalBody::PromptPromoted(
                    PromptPromoted {
                        enqueue_id,
                        position,
                    },
                )))
            }
            JournalDraft::ToolCall {
                turn_id,
                tool_call_id,
                name,
                args_bytes,
            } => {
                if let Some(existing) = self
                    .state
                    .tools
                    .get(&turn_id)
                    .and_then(|calls| calls.get(&tool_call_id))
                {
                    if existing.name == name && existing.args_bytes == args_bytes {
                        return Ok(ValidatedAppend::NoOp(existing.call_seq));
                    }
                    return Err(JournalError::Lifecycle(format!(
                        "tool call {turn_id}/{tool_call_id} was reused with different content"
                    )));
                }
                Ok(ValidatedAppend::Write(JournalBody::ToolCall(ToolCall {
                    turn_id,
                    tool_call_id,
                    name,
                    args_bytes,
                })))
            }
            JournalDraft::ToolTerminal {
                turn_id,
                tool_call_id,
                status,
                source,
            } => {
                if let Some(existing) = self
                    .state
                    .tools
                    .get(&turn_id)
                    .and_then(|calls| calls.get(&tool_call_id))
                    && let Some(seq) = existing.terminal_seq
                {
                    tracing::debug!(
                        turn_id,
                        tool_call_id,
                        "duplicate tool terminal dropped"
                    );
                    return Ok(ValidatedAppend::NoOp(seq));
                }
                Ok(ValidatedAppend::Write(JournalBody::ToolTerminal(
                    ToolTerminal {
                        turn_id,
                        tool_call_id,
                        status,
                        source,
                    },
                )))
            }
            JournalDraft::TurnTerminal {
                turn_id,
                outcome,
                code,
            } => {
                if let Some(existing) = self.state.turns.get(&turn_id)
                    && let Some(seq) = existing.terminal_seq
                {
                    tracing::debug!(turn_id, "duplicate turn terminal dropped");
                    return Ok(ValidatedAppend::NoOp(seq));
                }
                Ok(ValidatedAppend::Write(JournalBody::TurnTerminal(
                    TurnTerminal {
                        turn_id,
                        outcome,
                        code,
                    },
                )))
            }
            JournalDraft::SessionOpened(payload) => {
                Ok(ValidatedAppend::Write(JournalBody::SessionOpened(payload)))
            }
            JournalDraft::RecoveryApplied {
                turn_id,
                calls_failed,
                note,
            } => {
                if self.state.recoveries.contains_key(&turn_id) {
                    return Ok(ValidatedAppend::NoOp(self.state.last_seq));
                }
                Ok(ValidatedAppend::Write(JournalBody::RecoveryApplied(
                    RecoveryApplied {
                        turn_id,
                        calls_failed,
                        note,
                    },
                )))
            }
        }
    }

    fn write_record(&mut self, record: &JournalRecord) -> Result<(), JournalError> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.file.sync_data()?;
        self.journal_bytes += line.len() as u64;
        Ok(())
    }

    fn compact_if_needed(&mut self) -> Result<(), JournalError> {
        if self.records_since_snapshot >= self.limits.snapshot_records
            || self.journal_bytes >= self.limits.snapshot_bytes
        {
            self.compact()?;
        }
        Ok(())
    }

    fn replay_file(
        path: &Path,
        state: &mut DerivedState,
        report: &mut JournalOpenReport,
        quarantine: bool,
    ) -> Result<u64, JournalError> {
        let raw = match fs::read(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        let mut lines: Vec<&[u8]> = Vec::new();
        let mut position = 0;
        while position < raw.len() {
            let Some(newline) = raw[position..].iter().position(|byte| *byte == b'\n') else {
                break;
            };
            lines.push(&raw[position..position + newline]);
            position += newline + 1;
        }
        let non_empty: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.iter().any(|byte| !byte.is_ascii_whitespace()))
            .map(|(index, _)| index)
            .collect();

        let mut expected = if state.last_seq == 0 {
            None
        } else {
            Some(state.last_seq + 1)
        };
        let mut corrupt = false;
        for (rank, &index) in non_empty.iter().enumerate() {
            match serde_json::from_slice::<JournalRecord>(lines[index]) {
                Ok(record) => {
                    if record.seq <= state.last_seq {
                        continue;
                    }
                    if let Some(next) = expected
                        && record.seq != next
                    {
                        corrupt = true;
                        break;
                    }
                    state.apply(&record);
                    expected = Some(record.seq + 1);
                    report.records_replayed += 1;
                }
                Err(error) => {
                    if rank + 1 == non_empty.len() {
                        tracing::debug!(path = %path.display(), error = %error, "ignoring torn journal tail");
                        report.truncated_tail = true;
                    } else {
                        corrupt = true;
                    }
                    break;
                }
            }
        }
        if position < raw.len() && raw[position..].iter().any(|byte| !byte.is_ascii_whitespace())
        {
            report.truncated_tail = true;
        }

        if corrupt {
            report.corrupt = true;
            if quarantine {
                let corrupt_path = PathBuf::from(format!("{}.corrupt", path.display()));
                let _ = fs::remove_file(&corrupt_path);
                fs::rename(path, &corrupt_path)?;
                report.corrupt_path = Some(corrupt_path);
                tracing::warn!(path = %path.display(), "quarantined corrupt journal and kept the valid prefix");
            }
        }
        Ok(raw.len() as u64)
    }

    /// Read-only derived state, tolerant of a concurrently appending writer
    /// (`§2.1`). Never quarantines: the app must not rename the writer's file.
    pub fn read_state(
        root: impl AsRef<Path>,
        conversation_id: &str,
    ) -> Result<DerivedState, JournalError> {
        Self::read_state_with_report(root, conversation_id).map(|(state, _)| state)
    }

    /// [`Self::read_state`] plus the open report (torn tail, corrupt prefix).
    pub fn read_state_with_report(
        root: impl AsRef<Path>,
        conversation_id: &str,
    ) -> Result<(DerivedState, JournalOpenReport), JournalError> {
        let root = root.as_ref();
        let mut state = DerivedState::default();
        let mut report = JournalOpenReport::default();
        if let Ok(raw) = fs::read_to_string(snapshot_path(root, conversation_id))
            && let Ok(loaded) = serde_json::from_str::<JournalSnapshot>(&raw)
        {
            state = loaded.state;
            state.last_seq = state.last_seq.max(loaded.seq);
            report.snapshot_loaded = true;
        }
        Self::replay_file(
            &journal_path(root, conversation_id),
            &mut state,
            &mut report,
            false,
        )?;
        Ok((state, report))
    }

    /// Rows the app rehydrates into `QueuedQueryModel` (`§3.2`).
    pub fn read_queue(
        root: impl AsRef<Path>,
        conversation_id: &str,
    ) -> Result<Vec<QueuedPrompt>, JournalError> {
        Ok(Self::read_state(root, conversation_id)?.queued_prompts())
    }

    /// Delete every journal artifact for a conversation (`§2.5`).
    pub fn delete(root: impl AsRef<Path>, conversation_id: &str) -> io::Result<()> {
        let root = root.as_ref();
        for path in [
            journal_path(root, conversation_id),
            snapshot_path(root, conversation_id),
            PathBuf::from(format!(
                "{}.corrupt",
                journal_path(root, conversation_id).display()
            )),
            PathBuf::from(format!("{}.1", journal_path(root, conversation_id).display())),
        ] {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Sweep stale, queue-free journals and enforce the global byte cap
    /// (`§2.5`). `now` is Unix millis.
    pub fn sweep(root: impl AsRef<Path>, now_millis: i64, max_bytes: u64) -> io::Result<SweepReport> {
        let root = root.as_ref();
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(SweepReport::default()),
            Err(error) => return Err(error),
        };
        struct Candidate {
            conversation_id: String,
            size: u64,
            modified_millis: i64,
            live: bool,
        }
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut report = SweepReport::default();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(conversation_id) = name.strip_suffix(JOURNAL_SUFFIX) else {
                continue;
            };
            let metadata = entry.metadata()?;
            let modified_millis = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as i64)
                .unwrap_or(now_millis);
            let live = Self::read_state(root, conversation_id)
                .map(|state| !state.queued_prompts().is_empty())
                .unwrap_or(true);
            report.scanned += 1;
            candidates.push(Candidate {
                conversation_id: conversation_id.to_string(),
                size: metadata.len(),
                modified_millis,
                live,
            });
        }

        let retention_cutoff = now_millis - (DEFAULT_RETENTION_DAYS as i64) * MILLIS_PER_DAY;
        let mut total: u64 = candidates.iter().map(|candidate| candidate.size).sum();
        candidates.sort_by_key(|candidate| candidate.modified_millis);
        for candidate in &candidates {
            let too_old = candidate.modified_millis < retention_cutoff;
            let over_cap = total > max_bytes;
            if candidate.live || (!too_old && !over_cap) {
                continue;
            }
            if Self::delete(root, &candidate.conversation_id).is_ok() {
                report.removed += 1;
                report.bytes_removed += candidate.size;
                total = total.saturating_sub(candidate.size);
            }
        }
        Ok(report)
    }
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
#[path = "journal_tests.rs"]
mod tests;
