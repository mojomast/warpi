//! Durable per-conversation event log for standalone conversations.
//!
//! This is the writer/reader half of `standalone/research/event-log-delivery-spec.md`
//! §1 (location, schema, invariants, rotation/pruning, read API) and the pure part of
//! §3 (`DeliveryMode`). It has no bridge or GUI dependency: the caller injects the
//! events root (`StandaloneConfig::data_dir()/events`, overridable in tests with
//! `WARPI_EVENT_LOG_DIR`) and owns one writer per conversation.
//!
//! Layout (`§1.1`):
//!
//! ```text
//! <root>/<conversation_id>/
//!   writer.lock          # O_EXCL; {writer_id, pid, generation, started_at}
//!   seg-000001.jsonl     # one [`LogRecord`] per line, append-only
//!   seg-000002.jsonl
//!   head.json            # cache only: {last_seq, last_generation, segments[]}
//! ```
//!
//! `append` allocates `seq = last_seq + 1` and writes before the caller mutates bridge
//! state (`§1.4.3`). Idempotency is first-wins by `idem` (`§1.4.4`); replay divergence,
//! sequence gaps, and a torn trailing line are detected by [`EventLog::verify`] and
//! [`read_conversation`] (`§1.4.5`).

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::usage_ledger::{now_rfc3339, unix_millis};

/// Schema version written into every record (`v`).
pub const EVENT_LOG_VERSION: u32 = 1;
/// Rotate the active segment at this size (`§1.6`).
pub const DEFAULT_SEGMENT_BYTES: u64 = 8 * 1024 * 1024;
/// Per-conversation byte budget (`§1.6`); `WARPI_EVENT_LOG_MAX_BYTES` overrides.
pub const DEFAULT_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Per-conversation retention window in days; `WARPI_EVENT_LOG_RETENTION_DAYS` overrides.
pub const DEFAULT_RETENTION_DAYS: u32 = 30;
/// Global ceiling across conversations (`§1.6`).
pub const DEFAULT_GLOBAL_MAX_BYTES: u64 = 512 * 1024 * 1024;
pub const EVENT_LOG_MAX_BYTES_ENV: &str = "WARPI_EVENT_LOG_MAX_BYTES";
pub const EVENT_LOG_RETENTION_DAYS_ENV: &str = "WARPI_EVENT_LOG_RETENTION_DAYS";
/// Overrides the events root; used by tests (`§1.1`).
pub const EVENT_LOG_DIR_ENV: &str = "WARPI_EVENT_LOG_DIR";

const WRITER_LOCK: &str = "writer.lock";
const HEAD_FILE: &str = "head.json";
const SEGMENT_PREFIX: &str = "seg-";
const SEGMENT_SUFFIX: &str = ".jsonl";
const MILLIS_PER_DAY: i64 = 86_400_000;

/// Root holding every conversation's log directory for a standalone data dir.
pub fn events_dir(standalone_data_dir: &Path) -> PathBuf {
    standalone_data_dir.join("events")
}

/// Who caused an event (`§1.2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    Bridge,
    App,
    Helper,
    User,
}

/// Prompt delivery intent (`§3`). The durable log records the intent; the bridge
/// chooses the matching frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    /// Admit and start when idle (default; `§3.2`).
    #[default]
    Queue,
    /// Merge into the running turn at the next boundary (`§3.3`).
    Steer,
    /// Abort the active run, then start fresh (`§3.4`).
    CancelAndSendNow,
}

impl DeliveryMode {
    /// Whether the mode intentionally discards in-progress model work.
    pub fn is_destructive(self) -> bool {
        matches!(self, Self::CancelAndSendNow)
    }

    /// The `source` recorded on `queue.enqueued`.
    pub fn queue_source(self) -> &'static str {
        match self {
            Self::Queue => "queued",
            Self::Steer => "steer",
            Self::CancelAndSendNow => "immediate",
        }
    }
}

/// Record kind strings from the `§1.3` table.
pub mod kinds {
    pub const LOG_OPENED: &str = "log.opened";
    pub const SESSION_OPENED: &str = "session.opened";
    pub const QUEUE_ENQUEUED: &str = "queue.enqueued";
    pub const QUEUE_DEQUEUED: &str = "queue.dequeued";
    pub const QUEUE_CANCELLED: &str = "queue.cancelled";
    pub const TURN_STARTED: &str = "turn.started";
    pub const TURN_PAUSED: &str = "turn.paused";
    pub const TURN_RESUMED: &str = "turn.resumed";
    pub const TURN_SETTLED: &str = "turn.settled";
    pub const TURN_FAILED: &str = "turn.failed";
    pub const TURN_CANCELLED: &str = "turn.cancelled";
    pub const TURN_EXPIRED: &str = "turn.expired";
    pub const CANCEL_REQUESTED: &str = "cancel.requested";
    pub const TOOL_CALLS: &str = "tool.calls";
    pub const TOOL_RESULT: &str = "tool.result";
    pub const TOOL_DENIED: &str = "tool.denied";
    pub const TOOL_UNTRANSLATABLE: &str = "tool.untranslatable";
    pub const USAGE_RECORDED: &str = "usage.recorded";
    pub const COMPACTION: &str = "compaction";
    pub const ERROR: &str = "error";
    pub const HELPER_EXITED: &str = "helper.exited";
    pub const TURN_UNRESOLVED: &str = "turn.unresolved";
}

/// One durable log line (`§1.2`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogRecord {
    pub v: u32,
    pub seq: u64,
    /// RFC 3339 UTC; ordering is by `seq`, never by `ts`.
    pub ts: String,
    pub conversation_id: String,
    #[serde(rename = "gen")]
    pub generation: u64,
    pub writer_id: String,
    pub kind: String,
    pub idem: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exchange_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub actor: Actor,
    pub data: Value,
}

impl LogRecord {
    /// The fields the `§1.4.4` divergence check compares.
    fn same_identity(&self, other: &Self) -> bool {
        self.idem == other.idem
            && self.kind == other.kind
            && self.turn_id == other.turn_id
            && self.exchange_id == other.exchange_id
            && self.data == other.data
    }
}

/// A caller-supplied event; `seq`, `ts`, `gen`, and `writer_id` are assigned on append.
#[derive(Debug, Clone, PartialEq)]
pub struct LogRecordDraft {
    pub kind: String,
    pub idem: String,
    pub turn_id: Option<String>,
    pub exchange_id: Option<String>,
    pub run_id: Option<String>,
    pub actor: Actor,
    pub data: Value,
}

impl LogRecordDraft {
    pub fn new(
        kind: impl Into<String>,
        idem: impl Into<String>,
        actor: Actor,
        data: Value,
    ) -> Self {
        Self {
            kind: kind.into(),
            idem: idem.into(),
            turn_id: None,
            exchange_id: None,
            run_id: None,
            actor,
            data,
        }
    }

    pub fn with_turn(mut self, turn_id: impl Into<String>) -> Self {
        self.turn_id = Some(turn_id.into());
        self
    }

    pub fn with_exchange(mut self, exchange_id: impl Into<String>) -> Self {
        self.exchange_id = Some(exchange_id.into());
        self
    }

    pub fn with_run(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = Some(run_id.into());
        self
    }
}

/// A replay-supplied record that disagrees with what is stored (`§1.4.5`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    pub seq: u64,
    pub idem: String,
}

/// A missing sequence number (`§1.4.5`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequenceGap {
    pub expected: u64,
    pub found: u64,
}

/// Result of [`EventLog::verify`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    pub last_seq: u64,
    pub generation: u64,
    pub records: usize,
    pub truncated_tail: bool,
    pub corrupt_segments: Vec<String>,
    pub divergences: Vec<Divergence>,
    pub gaps: Vec<SequenceGap>,
}

/// Read-only view returned by [`read_conversation`] (`§1.7`).
#[derive(Debug, Clone, PartialEq)]
pub struct ConversationLogView {
    pub conversation_id: String,
    pub generation: u64,
    pub last_seq: u64,
    pub truncated_tail: bool,
    pub corrupt_segments: Vec<String>,
    pub records: Vec<LogRecord>,
}

/// Limits and location for one events root.
#[derive(Debug, Clone)]
pub struct EventLogConfig {
    pub root: PathBuf,
    pub max_bytes: u64,
    pub retention_days: u32,
    pub segment_bytes: u64,
}

impl EventLogConfig {
    /// Root plus environment-overridable limits (`§1.6`). `0` disables a limit.
    /// `WARPI_EVENT_LOG_DIR` replaces the supplied root.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = std::env::var(EVENT_LOG_DIR_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|_| root.into());
        let max_bytes = env_u64(EVENT_LOG_MAX_BYTES_ENV).unwrap_or(DEFAULT_MAX_BYTES);
        let retention_days = env_u64(EVENT_LOG_RETENTION_DAYS_ENV)
            .map(|value| value as u32)
            .unwrap_or(DEFAULT_RETENTION_DAYS);
        Self {
            root,
            max_bytes,
            retention_days,
            segment_bytes: DEFAULT_SEGMENT_BYTES,
        }
    }

    pub fn with_limits(mut self, max_bytes: u64, retention_days: u32) -> Self {
        self.max_bytes = max_bytes;
        self.retention_days = retention_days;
        self
    }

    pub fn with_segment_bytes(mut self, segment_bytes: u64) -> Self {
        self.segment_bytes = segment_bytes.max(1);
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EventLogError {
    #[error("event log io error: {0}")]
    Io(#[from] io::Error),
    #[error("event log encode error: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("event log is already owned by a live writer")]
    Locked,
    #[error("replay diverged for {idem} at seq {existing_seq}")]
    ReplayDiverged { idem: String, existing_seq: u64 },
    #[error("stale generation: record has {found}, log is at {expected}")]
    StaleGeneration { expected: u64, found: u64 },
}

#[derive(Debug, Clone)]
struct IdemEntry {
    seq: u64,
    kind: String,
    data: Value,
}

#[derive(Debug, Default)]
struct Scan {
    records: Vec<LogRecord>,
    last_seq: u64,
    last_generation: u64,
    highest_segment: u32,
    truncated_tail: bool,
    corrupt_segments: Vec<String>,
    divergences: Vec<Divergence>,
    gaps: Vec<SequenceGap>,
}

/// Single-writer append handle for one conversation log.
pub struct EventLog {
    dir: PathBuf,
    conversation_id: String,
    writer_id: String,
    generation: u64,
    last_seq: u64,
    file: File,
    segment_index: u32,
    segment_bytes: u64,
    segment_limit: u64,
    idem: BTreeMap<String, IdemEntry>,
    config: EventLogConfig,
    degraded: bool,
}

impl EventLog {
    /// Open the log for `conversation_id` under `root`, acquire the writer lock,
    /// and allocate `generation = persisted_last_generation + 1` (`§1.4.1`–`§1.4.2`).
    pub fn open(
        root: impl AsRef<Path>,
        conversation_id: &str,
        writer_id: &str,
    ) -> Result<Self, EventLogError> {
        Self::open_with_config(&EventLogConfig::new(root.as_ref()), conversation_id, writer_id)
    }

    /// [`Self::open`] with explicit limits.
    pub fn open_with_config(
        config: &EventLogConfig,
        conversation_id: &str,
        writer_id: &str,
    ) -> Result<Self, EventLogError> {
        let dir = config.root.join(conversation_id);
        fs::create_dir_all(&dir)?;
        let scan = scan(&dir, true)?;
        let generation = scan.last_generation + 1;
        acquire_lock(&dir, writer_id, generation)?;

        let segment_index = scan.highest_segment + 1;
        let path = segment_path(&dir, segment_index);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;

        let mut idem = BTreeMap::new();
        for record in &scan.records {
            idem.entry(record.idem.clone()).or_insert_with(|| IdemEntry {
                seq: record.seq,
                kind: record.kind.clone(),
                data: record.data.clone(),
            });
        }

        let mut log = Self {
            dir,
            conversation_id: conversation_id.to_string(),
            writer_id: writer_id.to_string(),
            generation,
            last_seq: scan.last_seq,
            file,
            segment_index,
            segment_bytes: 0,
            segment_limit: config.segment_bytes,
            idem,
            config: config.clone(),
            degraded: scan.truncated_tail
                || !scan.corrupt_segments.is_empty()
                || !scan.gaps.is_empty(),
        };

        let previous = if scan.last_generation == 0 {
            Value::Null
        } else {
            json!(scan.last_generation)
        };
        let draft = LogRecordDraft::new(
            kinds::LOG_OPENED,
            format!("{}:{generation}", kinds::LOG_OPENED),
            Actor::Bridge,
            json!({
                "writer_id": writer_id,
                "resumed_from_seq": scan.last_seq,
                "prev_generation": previous,
            }),
        );
        log.append(draft)?;
        Ok(log)
    }

    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    pub fn writer_id(&self) -> &str {
        &self.writer_id
    }

    /// The fencing token allocated at open (`§1.4.2`).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Whether the scan found a torn tail or a corrupt segment at open.
    pub fn is_degraded(&self) -> bool {
        self.degraded
    }

    pub fn config(&self) -> &EventLogConfig {
        &self.config
    }

    /// Reject a record or helper frame from an older generation (`§1.4.2`).
    pub fn check_generation(&self, generation: u64) -> Result<(), EventLogError> {
        if generation < self.generation {
            return Err(EventLogError::StaleGeneration {
                expected: self.generation,
                found: generation,
            });
        }
        Ok(())
    }

    /// Append an event. Idempotent by `idem`: identical content returns the
    /// existing `seq`, diverging content returns [`EventLogError::ReplayDiverged`].
    pub fn append(&mut self, draft: LogRecordDraft) -> Result<u64, EventLogError> {
        if let Some(existing) = self.idem.get(&draft.idem) {
            if existing.kind == draft.kind && existing.data == draft.data {
                return Ok(existing.seq);
            }
            return Err(EventLogError::ReplayDiverged {
                idem: draft.idem,
                existing_seq: existing.seq,
            });
        }

        let seq = self.last_seq + 1;
        let record = LogRecord {
            v: EVENT_LOG_VERSION,
            seq,
            ts: now_rfc3339(),
            conversation_id: self.conversation_id.clone(),
            generation: self.generation,
            writer_id: self.writer_id.clone(),
            kind: draft.kind.clone(),
            idem: draft.idem.clone(),
            turn_id: draft.turn_id.clone(),
            exchange_id: draft.exchange_id.clone(),
            run_id: draft.run_id.clone(),
            actor: draft.actor,
            data: draft.data.clone(),
        };
        let mut line = serde_json::to_vec(&record)?;
        line.push(b'\n');

        self.ensure_segment_room(line.len() as u64)?;
        self.file.write_all(&line)?;
        self.file.sync_data()?;
        self.segment_bytes += line.len() as u64;
        self.last_seq = seq;
        self.idem.insert(
            draft.idem,
            IdemEntry {
                seq,
                kind: draft.kind,
                data: draft.data,
            },
        );
        self.write_head()?;
        self.prune_if_needed()?;
        Ok(seq)
    }

    /// Records with `seq > after_seq`, oldest first (`§1.7`).
    pub fn replay(&self, after_seq: u64, limit: usize) -> Result<Vec<LogRecord>, EventLogError> {
        let scan = scan(&self.dir, false)?;
        Ok(scan
            .records
            .into_iter()
            .filter(|record| record.seq > after_seq)
            .take(limit)
            .collect())
    }

    /// Divergence, truncation, and gap scan (`§1.4.5`). Quarantines corrupt
    /// segments as `*.corrupt`.
    pub fn verify(&self) -> Result<VerifyReport, EventLogError> {
        Ok(verify_scan(scan(&self.dir, true)?))
    }

    fn ensure_segment_room(&mut self, incoming: u64) -> Result<(), EventLogError> {
        if self.segment_bytes == 0 || self.segment_bytes + incoming <= self.segment_limit {
            return Ok(());
        }
        self.segment_index += 1;
        let path = segment_path(&self.dir, self.segment_index);
        self.file = OpenOptions::new().create(true).append(true).open(&path)?;
        self.segment_bytes = 0;
        Ok(())
    }

    fn write_head(&self) -> Result<(), EventLogError> {
        let segments = segment_files(&self.dir)?
            .into_iter()
            .map(|(_, path)| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        let head = json!({
            "last_seq": self.last_seq,
            "last_generation": self.generation,
            "segments": segments,
        });
        let path = self.dir.join(HEAD_FILE);
        let tmp = self.dir.join(format!("{HEAD_FILE}.tmp"));
        let mut file = File::create(&tmp)?;
        file.write_all(&serde_json::to_vec(&head)?)?;
        file.sync_all()?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn prune_if_needed(&mut self) -> Result<(), EventLogError> {
        if self.config.max_bytes == 0 && self.config.retention_days == 0 {
            return Ok(());
        }
        if self.needs_recovery_guard()? {
            return Ok(());
        }
        let segments = segment_files(&self.dir)?;
        let now = unix_millis(SystemTime::now());
        let cutoff = now - i64::from(self.config.retention_days) * MILLIS_PER_DAY;
        let mut total: u64 = segments
            .iter()
            .map(|(_, path)| fs::metadata(path).map(|meta| meta.len()).unwrap_or(0))
            .sum();
        let mut removed = false;
        for (index, path) in &segments {
            if *index == self.segment_index {
                continue;
            }
            let size = fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
            let modified = fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as i64)
                .unwrap_or(now);
            let too_old = self.config.retention_days != 0 && modified < cutoff;
            let over_budget = self.config.max_bytes != 0 && total > self.config.max_bytes;
            if !too_old && !over_budget {
                continue;
            }
            if fs::remove_file(path).is_ok() {
                total = total.saturating_sub(size);
                removed = true;
            }
        }
        if removed {
            self.write_head()?;
        }
        Ok(())
    }

    fn needs_recovery_guard(&self) -> Result<bool, EventLogError> {
        let scan = scan(&self.dir, false)?;
        let mut started: HashSet<String> = HashSet::new();
        let mut terminal: HashSet<String> = HashSet::new();
        let mut enqueued: HashSet<String> = HashSet::new();
        let mut resolved: HashSet<String> = HashSet::new();
        for record in &scan.records {
            match record.kind.as_str() {
                kinds::TURN_STARTED => {
                    if let Some(id) = &record.turn_id {
                        started.insert(id.clone());
                    }
                }
                kinds::TURN_SETTLED | kinds::TURN_FAILED | kinds::TURN_CANCELLED
                | kinds::TURN_EXPIRED => {
                    if let Some(id) = &record.turn_id {
                        terminal.insert(id.clone());
                    }
                }
                kinds::QUEUE_ENQUEUED => {
                    if let Some(id) = &record.exchange_id {
                        enqueued.insert(id.clone());
                    }
                }
                kinds::QUEUE_DEQUEUED | kinds::QUEUE_CANCELLED => {
                    if let Some(id) = &record.exchange_id {
                        resolved.insert(id.clone());
                    }
                }
                _ => {}
            }
        }
        Ok(started.iter().any(|id| !terminal.contains(id))
            || enqueued.iter().any(|id| !resolved.contains(id)))
    }
}

impl Drop for EventLog {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.dir.join(WRITER_LOCK));
    }
}

/// Read-only view of a conversation log; safe to call while a writer is live
/// (`§1.7`). Never quarantines.
pub fn read_conversation(
    root: impl AsRef<Path>,
    conversation_id: &str,
) -> Result<ConversationLogView, EventLogError> {
    let dir = root.as_ref().join(conversation_id);
    let scan = scan(&dir, false)?;
    Ok(ConversationLogView {
        conversation_id: conversation_id.to_string(),
        generation: scan.last_generation,
        last_seq: scan.last_seq,
        truncated_tail: scan.truncated_tail,
        corrupt_segments: scan.corrupt_segments,
        records: scan.records,
    })
}

/// [`EventLog::verify`] without needing to hold the writer lock, for diagnostics
/// and for reading a log left behind by a crashed writer.
pub fn verify_conversation(
    root: impl AsRef<Path>,
    conversation_id: &str,
) -> Result<VerifyReport, EventLogError> {
    Ok(verify_scan(scan(&root.as_ref().join(conversation_id), false)?))
}

fn verify_scan(scan: Scan) -> VerifyReport {
    VerifyReport {
        last_seq: scan.last_seq,
        generation: scan.last_generation,
        records: scan.records.len(),
        truncated_tail: scan.truncated_tail,
        corrupt_segments: scan.corrupt_segments,
        divergences: scan.divergences,
        gaps: scan.gaps,
    }
}

fn scan(dir: &Path, quarantine: bool) -> Result<Scan, EventLogError> {
    let mut scan = Scan::default();
    let segments = segment_files(dir)?;
    let mut expected: Option<u64> = None;
    let mut by_seq: BTreeMap<u64, LogRecord> = BTreeMap::new();
    let mut stopped = false;

    for (index, path) in &segments {
        scan.highest_segment = scan.highest_segment.max(*index);
        if stopped {
            continue;
        }
        let raw = match fs::read(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
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

        for (rank, &line_index) in non_empty.iter().enumerate() {
            match serde_json::from_slice::<LogRecord>(lines[line_index]) {
                Ok(record) => {
                    match expected {
                        None => {}
                        Some(next) if record.seq < next => {
                            if let Some(previous) = by_seq.get(&record.seq)
                                && !previous.same_identity(&record)
                            {
                                scan.divergences.push(Divergence {
                                    seq: record.seq,
                                    idem: record.idem.clone(),
                                });
                            }
                            continue;
                        }
                        Some(next) if record.seq > next => {
                            scan.gaps.push(SequenceGap {
                                expected: next,
                                found: record.seq,
                            });
                            stopped = true;
                            break;
                        }
                        Some(_) => {}
                    }
                    expected = Some(record.seq + 1);
                    scan.last_seq = record.seq;
                    scan.last_generation = scan.last_generation.max(record.generation);
                    by_seq.insert(record.seq, record.clone());
                    scan.records.push(record);
                }
                Err(error) => {
                    if rank + 1 == non_empty.len() {
                        tracing::debug!(path = %path.display(), error = %error, "ignoring torn event log tail");
                        scan.truncated_tail = true;
                        break;
                    }
                    let name = path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    scan.corrupt_segments.push(name.clone());
                    if quarantine {
                        let corrupt = path.with_file_name(format!("{name}.corrupt"));
                        let _ = fs::remove_file(&corrupt);
                        fs::rename(path, &corrupt)?;
                        tracing::warn!(path = %path.display(), "quarantined corrupt event log segment");
                    }
                    stopped = true;
                    break;
                }
            }
        }
        if position < raw.len()
            && raw[position..].iter().any(|byte| !byte.is_ascii_whitespace())
        {
            scan.truncated_tail = true;
        }
    }
    Ok(scan)
}

fn segment_files(dir: &Path) -> io::Result<Vec<(u32, PathBuf)>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if let Some(index) = parse_segment_name(name) {
            files.push((index, entry.path()));
        }
    }
    files.sort_by_key(|(index, _)| *index);
    Ok(files)
}

fn parse_segment_name(name: &str) -> Option<u32> {
    let rest = name
        .strip_prefix(SEGMENT_PREFIX)?
        .strip_suffix(SEGMENT_SUFFIX)?;
    if rest.len() != 6 || !rest.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

fn segment_path(dir: &Path, index: u32) -> PathBuf {
    dir.join(format!("{SEGMENT_PREFIX}{index:06}{SEGMENT_SUFFIX}"))
}

fn acquire_lock(dir: &Path, writer_id: &str, generation: u64) -> Result<(), EventLogError> {
    let path = dir.join(WRITER_LOCK);
    for _ in 0..2 {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let content = json!({
                    "writer_id": writer_id,
                    "pid": std::process::id(),
                    "generation": generation,
                    "started_at": now_rfc3339(),
                });
                file.write_all(&serde_json::to_vec(&content)?)?;
                file.sync_all()?;
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let stale = fs::read_to_string(&path)
                    .ok()
                    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                    .and_then(|value| value.get("pid").and_then(Value::as_u64))
                    .is_none_or(|pid| !pid_is_alive(pid as u32));
                if stale {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                return Err(EventLogError::Locked);
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err(EventLogError::Locked)
}

#[cfg(target_os = "linux")]
fn pid_is_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(target_os = "linux"))]
fn pid_is_alive(_pid: u32) -> bool {
    true
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
}

#[cfg(test)]
#[path = "event_log_tests.rs"]
mod tests;
