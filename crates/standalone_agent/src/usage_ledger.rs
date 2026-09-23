//! Append-only local usage ledger for standalone conversations.
//!
//! Pure Rust and unit-testable without the GUI: the caller injects the ledger
//! directory (the app passes `StandaloneConfig::data_dir()/usage`, matching the
//! `data_dir().join("pi")` convention in [`crate::bridge`]). Layout and
//! retention follow `standalone/research/token-observability-spec.md` §3/§4:
//!
//! - `ledger-YYYY-MM.jsonl`: one JSON record per line, rotated to a numbered
//!   sibling (`ledger-YYYY-MM-02.jsonl`, …) at a calendar-month boundary or when
//!   the active file passes [`DEFAULT_MAX_FILE_BYTES`].
//! - `backfill.json`: per-session import markers so an unchanged Pi session file
//!   is not rescanned.
//!
//! Records hold ids, counters, timings, and money only — never prompts, tool
//! arguments, or model text. Writes are best-effort by design: use
//! [`UsageLedger::append_best_effort`] so a ledger problem never fails a turn.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Schema version written into every record.
pub const LEDGER_VERSION: u32 = 1;
/// Version of the pricing rules a [`CostEstimate`] was computed with.
pub const PRICING_VERSION: u32 = 1;
/// Rotate the active file once it reaches this size.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Byte budget enforced by pruning, overridable with `WARPI_LEDGER_MAX_BYTES`.
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
/// Retention window enforced by pruning.
pub const DEFAULT_RETENTION_MONTHS: u32 = 24;
/// Environment override for the pruning byte budget.
pub const MAX_TOTAL_BYTES_ENV: &str = "WARPI_LEDGER_MAX_BYTES";

const MILLIS_PER_SECOND: i64 = 1_000;
const MILLIS_PER_DAY: i64 = 86_400_000;
const DEFAULT_FALLBACK_ID: &str = "unknown";

/// Ledger directory for a standalone data directory (`<data dir>/standalone`),
/// mirroring `spec.data_dir.join("pi")` in `bridge.rs`.
pub fn usage_dir(standalone_data_dir: &Path) -> PathBuf {
    standalone_data_dir.join("usage")
}

/// Current UTC time as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn now_rfc3339() -> String {
    format_rfc3339_millis(unix_millis(SystemTime::now()))
}

/// Milliseconds since the Unix epoch, negative before it.
pub fn unix_millis(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration_millis(duration),
        Err(error) => -duration_millis(error.duration()),
    }
}

fn duration_millis(duration: Duration) -> i64 {
    let millis = duration
        .as_secs()
        .saturating_mul(MILLIS_PER_SECOND as u64)
        .saturating_add(u64::from(duration.subsec_millis()));
    i64::try_from(millis).unwrap_or(i64::MAX)
}

/// Format epoch milliseconds as RFC 3339 UTC with millisecond precision.
pub fn format_rfc3339_millis(millis: i64) -> String {
    let days = millis.div_euclid(MILLIS_PER_DAY);
    let remainder = millis.rem_euclid(MILLIS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    let hour = remainder / 3_600_000;
    let minute = (remainder / 60_000) % 60;
    let second = (remainder / 1_000) % 60;
    let millis = remainder % 1_000;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Parse the `YYYY-MM-DDTHH:MM:SS[.fff]Z` timestamps written by the ledger and
/// by Pi session files. Offsets and missing components yield `None`.
pub fn parse_rfc3339_millis(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year: i64 = value.get(0..4)?.parse().ok()?;
    let month: u32 = value.get(5..7)?.parse().ok()?;
    let day: u32 = value.get(8..10)?.parse().ok()?;
    let hour: u32 = value.get(11..13)?.parse().ok()?;
    let minute: u32 = value.get(14..16)?.parse().ok()?;
    let second: u32 = value.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let rest = value.get(19..)?;
    let (fraction_millis, tail) = match rest.strip_prefix('.') {
        Some(fraction) => {
            let digits: String = fraction.chars().take_while(char::is_ascii_digit).collect();
            if digits.is_empty() {
                return None;
            }
            let mut millis = 0_u32;
            for (index, digit) in digits.chars().take(3).enumerate() {
                millis += (digit as u32 - '0' as u32) * 10_u32.pow(2 - index as u32);
            }
            (millis, &fraction[digits.len()..])
        }
        None => (0, rest),
    };
    if tail != "Z" {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(
        days * MILLIS_PER_DAY
            + i64::from(hour) * 3_600_000
            + i64::from(minute) * 60_000
            + i64::from(second) * MILLIS_PER_SECOND
            + i64::from(fraction_millis),
    )
}

/// UTC calendar day (`YYYY-MM-DD`) of a ledger timestamp.
pub fn utc_day_of(value: &str) -> Option<String> {
    let day = value.get(0..10)?;
    let bytes = day.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let month: u32 = day.get(5..7)?.parse().ok()?;
    let day_of_month: u32 = day.get(8..10)?.parse().ok()?;
    let year: i64 = day.get(0..4)?.parse().ok()?;
    (year > 0 && (1..=12).contains(&month) && (1..=31).contains(&day_of_month))
        .then(|| day.to_string())
}

/// Days since the Unix epoch for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`; the calendar arithmetic is not expressible with `SystemTime`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = (year - era * 400) as u64;
    let month_prime = if month > 2 { month - 3 } else { month + 9 } as u64;
    let day_of_year = (153 * month_prime + 2) / 5 + u64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era as i64 - 719_468
}

/// Inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (year + i64::from(month <= 2), month, day)
}

fn utc_month_index(millis: i64) -> i64 {
    let (year, month, _) = civil_from_days(millis.div_euclid(MILLIS_PER_DAY));
    year * 12 + i64::from(month) - 1
}

fn utc_year_month(millis: i64) -> String {
    let (year, month, _) = civil_from_days(millis.div_euclid(MILLIS_PER_DAY));
    format!("{year:04}-{month:02}")
}

/// Token counters for one model call. `reasoning` is a reported subset of
/// `output` (as in the Pi SDK) and is never billed on top of it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageCounters {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    #[serde(default)]
    pub reasoning: u64,
    /// Provider-reported total; falls back to `input + output + cache_read +
    /// cache_write` when absent or zero.
    #[serde(default)]
    pub total: u64,
}

impl UsageCounters {
    /// The provider total when it is non-zero, otherwise the sum of the parts.
    pub fn total_tokens(&self) -> u64 {
        if self.total != 0 {
            self.total
        } else {
            self.input
                .saturating_add(self.output)
                .saturating_add(self.cache_read)
                .saturating_add(self.cache_write)
        }
    }

    pub fn is_zero(&self) -> bool {
        self.input == 0
            && self.output == 0
            && self.cache_read == 0
            && self.cache_write == 0
            && self.reasoning == 0
            && self.total == 0
    }

    fn add(&mut self, other: &UsageCounters) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.reasoning = self.reasoning.saturating_add(other.reasoning);
        self.total = self.total.saturating_add(other.total_tokens());
    }
}

/// Wall-clock facts for one model call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTiming {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_token_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_ms: Option<u64>,
}

/// Where a context reading came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSource {
    Usage,
    Estimate,
    CompactionEstimate,
}

/// Context-window reading captured after a model call.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ContextUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<f64>,
    pub source: ContextSource,
}

/// Compaction facts, recorded either alone or alongside the summary call usage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionRecord {
    pub reason: String,
    #[serde(default)]
    pub summarized: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_before: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_after: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_usage: Option<UsageCounters>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// Estimated USD cost. Every standalone cost surface must label this
/// "estimated"; a missing pricing row leaves the record's `cost` as `None`
/// rather than a false `$0.00`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CostEstimate {
    pub total_usd: f64,
    pub estimated: bool,
    pub pricing_version: u32,
}

impl CostEstimate {
    pub fn new(total_usd: f64) -> Self {
        Self {
            total_usd,
            estimated: true,
            pricing_version: PRICING_VERSION,
        }
    }
}

/// How a record entered the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordSource {
    Live,
    Backfill,
}

/// Whether a record belongs to a primary-agent call, a compaction, or a child
/// `task` subagent. Child records are kept distinct so their spend is never
/// folded into (or double-counted with) the parent's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordCategory {
    PrimaryAgent,
    Compaction,
    Subagent,
}

/// One ledger line: the durable per-model-call usage fact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageRecord {
    #[serde(default = "default_ledger_version")]
    pub v: u32,
    /// UTC timestamp (`YYYY-MM-DDTHH:MM:SS.mmmZ`).
    pub ts: String,
    pub source: RecordSource,
    pub conversation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exchange_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    pub profile_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default = "default_record_category")]
    pub category: RecordCategory,
    pub usage: UsageCounters,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<UsageTiming>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostEstimate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionRecord>,
    /// Pi session JSONL this record was backfilled from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
}

impl UsageRecord {
    /// Fill in `cost` from the pricing table, or clear it when the model has no
    /// pricing row.
    pub fn apply_pricing(&mut self, pricing: &BTreeMap<String, ModelPricing>) {
        self.cost = estimate_cost(pricing, &self.model_id, &self.usage);
    }
}

fn default_ledger_version() -> u32 {
    LEDGER_VERSION
}

fn default_record_category() -> RecordCategory {
    RecordCategory::PrimaryAgent
}

/// USD per 1M tokens for one model. Cache rates are optional: when absent the
/// conservative upper bound bills cache tokens at the input rate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_read: Option<f64>,
    #[serde(default)]
    pub cache_write: Option<f64>,
}

impl ModelPricing {
    /// Rates must be finite and non-negative; anything else is treated as
    /// "pricing unknown" instead of producing a bogus number.
    pub fn is_valid(&self) -> bool {
        let valid = |rate: f64| rate.is_finite() && rate >= 0.0;
        valid(self.input)
            && valid(self.output)
            && self.cache_read.is_none_or(valid)
            && self.cache_write.is_none_or(valid)
    }

    /// Estimated USD for these counters, or `None` for invalid rates.
    pub fn estimate(&self, usage: &UsageCounters) -> Option<f64> {
        if !self.is_valid() {
            return None;
        }
        let cache_read = self.cache_read.unwrap_or(self.input);
        let cache_write = self.cache_write.unwrap_or(self.input);
        let usd_per_million = usage.input as f64 * self.input
            + usage.output as f64 * self.output
            + usage.cache_read as f64 * cache_read
            + usage.cache_write as f64 * cache_write;
        Some(usd_per_million / 1_000_000.0)
    }
}

/// Estimated cost for a model, or `None` when the table has no row for it.
pub fn estimate_cost(
    pricing: &BTreeMap<String, ModelPricing>,
    model_id: &str,
    usage: &UsageCounters,
) -> Option<CostEstimate> {
    let usd = pricing.get(model_id)?.estimate(usage)?;
    Some(CostEstimate::new(usd))
}

/// Selection applied before rollups are folded.
#[derive(Debug, Clone, Default)]
pub struct UsageFilter {
    pub conversation_id: Option<String>,
    pub profile_id: Option<String>,
    pub model_id: Option<String>,
    pub source: Option<RecordSource>,
    /// Inclusive lower bound, RFC 3339 UTC.
    pub since: Option<String>,
    /// Exclusive upper bound, RFC 3339 UTC.
    pub until: Option<String>,
}

impl UsageFilter {
    pub fn matches(&self, record: &UsageRecord) -> bool {
        if let Some(conversation_id) = &self.conversation_id
            && &record.conversation_id != conversation_id
        {
            return false;
        }
        if let Some(profile_id) = &self.profile_id
            && &record.profile_id != profile_id
        {
            return false;
        }
        if let Some(model_id) = &self.model_id
            && &record.model_id != model_id
        {
            return false;
        }
        if let Some(source) = self.source
            && record.source != source
        {
            return false;
        }
        if let Some(since) = &self.since
            && let (Some(record_millis), Some(since_millis)) = (
                parse_rfc3339_millis(&record.ts),
                parse_rfc3339_millis(since),
            )
            && record_millis < since_millis
        {
            return false;
        }
        if let Some(until) = &self.until
            && let (Some(record_millis), Some(until_millis)) = (
                parse_rfc3339_millis(&record.ts),
                parse_rfc3339_millis(until),
            )
            && record_millis >= until_millis
        {
            return false;
        }
        true
    }
}

/// Folded counters and money for a set of records.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageTotals {
    pub records: u64,
    /// Records with a known pricing row; only these contribute `cost_usd`.
    pub cost_known_records: u64,
    /// Records whose model has no pricing row ("cost unknown").
    pub cost_unknown_records: u64,
    pub usage: UsageCounters,
    pub cost_usd: f64,
}

impl UsageTotals {
    fn add(&mut self, record: &UsageRecord) {
        self.records = self.records.saturating_add(1);
        self.usage.add(&record.usage);
        match &record.cost {
            Some(cost) => {
                self.cost_known_records = self.cost_known_records.saturating_add(1);
                self.cost_usd += cost.total_usd;
            }
            None => {
                self.cost_unknown_records = self.cost_unknown_records.saturating_add(1);
            }
        }
    }

    fn merge(&mut self, other: &UsageTotals) {
        self.records = self.records.saturating_add(other.records);
        self.cost_known_records = self
            .cost_known_records
            .saturating_add(other.cost_known_records);
        self.cost_unknown_records = self
            .cost_unknown_records
            .saturating_add(other.cost_unknown_records);
        self.usage.add(&other.usage);
        self.cost_usd += other.cost_usd;
    }
}

/// Primary-agent spend split from compaction and subagent spend.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageSplit {
    pub primary: UsageTotals,
    pub compaction: UsageTotals,
    /// Child `task` runs, attributed separately from the parent agent.
    pub subagent: UsageTotals,
}

impl UsageSplit {
    fn add(&mut self, record: &UsageRecord) {
        match record.category {
            RecordCategory::PrimaryAgent => self.primary.add(record),
            RecordCategory::Compaction => self.compaction.add(record),
            RecordCategory::Subagent => self.subagent.add(record),
        }
    }

    pub fn total(&self) -> UsageTotals {
        let mut total = self.primary.clone();
        total.merge(&self.compaction);
        total.merge(&self.subagent);
        total
    }
}

/// Rollups by conversation, profile, model, and UTC day.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LedgerSummary {
    pub overall: UsageSplit,
    pub by_conversation: BTreeMap<String, UsageSplit>,
    pub by_profile: BTreeMap<String, UsageSplit>,
    pub by_model: BTreeMap<String, UsageSplit>,
    /// Keyed by `YYYY-MM-DD`.
    pub by_day: BTreeMap<String, UsageSplit>,
}

fn add_to_summary(summary: &mut LedgerSummary, record: &UsageRecord) {
    summary.overall.add(record);
    summary
        .by_conversation
        .entry(record.conversation_id.clone())
        .or_default()
        .add(record);
    summary
        .by_profile
        .entry(record.profile_id.clone())
        .or_default()
        .add(record);
    summary
        .by_model
        .entry(record.model_id.clone())
        .or_default()
        .add(record);
    if let Some(day) = utc_day_of(&record.ts) {
        summary.by_day.entry(day).or_default().add(record);
    }
}

/// Fold records into rollups.
pub fn summarize<'a>(records: impl IntoIterator<Item = &'a UsageRecord>) -> LedgerSummary {
    summarize_filtered(records, &UsageFilter::default())
}

/// Fold the records matching `filter` into rollups.
pub fn summarize_filtered<'a>(
    records: impl IntoIterator<Item = &'a UsageRecord>,
    filter: &UsageFilter,
) -> LedgerSummary {
    let mut summary = LedgerSummary::default();
    for record in records {
        if filter.matches(record) {
            add_to_summary(&mut summary, record);
        }
    }
    summary
}

/// Result of one pruning pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub files_removed: usize,
    pub bytes_removed: u64,
}

/// Result of one backfill run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackfillReport {
    pub files_scanned: usize,
    /// Files skipped because size and mtime match `backfill.json`.
    pub files_skipped: usize,
    /// Session files with no conversation in the supplied map.
    pub files_unmapped: usize,
    pub records_imported: usize,
    /// Parsed records already present in the ledger.
    pub records_skipped: usize,
}

impl BackfillReport {
    fn merge(&mut self, other: BackfillReport) {
        self.files_scanned += other.files_scanned;
        self.files_skipped += other.files_skipped;
        self.files_unmapped += other.files_unmapped;
        self.records_imported += other.records_imported;
        self.records_skipped += other.records_skipped;
    }
}

struct LedgerFile {
    month_index: i64,
    suffix: u32,
    path: PathBuf,
    size: u64,
}

/// Append-only JSONL writer and reader for one injected ledger directory.
#[derive(Debug, Clone)]
pub struct UsageLedger {
    dir: PathBuf,
    max_file_bytes: u64,
    max_total_bytes: u64,
    retention_months: u32,
}

impl UsageLedger {
    /// Ledger rooted at `dir`. The pruning byte budget honours
    /// [`MAX_TOTAL_BYTES_ENV`].
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let max_total_bytes = std::env::var(MAX_TOTAL_BYTES_ENV)
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(DEFAULT_MAX_TOTAL_BYTES);
        Self {
            dir: dir.into(),
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_total_bytes,
            retention_months: DEFAULT_RETENTION_MONTHS,
        }
    }

    /// Ledger at `<standalone_data_dir>/usage`.
    pub fn for_data_dir(standalone_data_dir: impl AsRef<Path>) -> Self {
        Self::new(usage_dir(standalone_data_dir.as_ref()))
    }

    pub fn with_limits(mut self, max_file_bytes: u64, max_total_bytes: u64) -> Self {
        self.max_file_bytes = max_file_bytes.max(1);
        self.max_total_bytes = max_total_bytes;
        self
    }

    pub fn with_retention_months(mut self, months: u32) -> Self {
        self.retention_months = months;
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Append one record, rotating the active file first when it is full.
    pub fn append(&self, record: &UsageRecord) -> io::Result<PathBuf> {
        self.append_at(record, SystemTime::now())
    }

    /// Append at an explicit wall clock. The record's timestamp picks the month
    /// file (so backfilled history stays chronological) and `now` drives
    /// rotation pruning.
    pub fn append_at(&self, record: &UsageRecord, now: SystemTime) -> io::Result<PathBuf> {
        fs::create_dir_all(&self.dir)?;
        let mut line = serde_json::to_string(record).map_err(io::Error::other)?;
        line.push('\n');
        let placement = parse_rfc3339_millis(&record.ts).unwrap_or_else(|| unix_millis(now));
        let month = utc_year_month(placement);
        let (path, created) =
            self.active_file(utc_month_index(placement), &month, line.len() as u64)?;
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        file.write_all(line.as_bytes())?;
        file.sync_data()?;
        drop(file);
        if created {
            self.prune_at(now)?;
        }
        Ok(path)
    }

    /// [`Self::append`] without an error path: a ledger failure is logged and
    /// swallowed so it can never fail a turn.
    pub fn append_best_effort(&self, record: &UsageRecord) -> Option<PathBuf> {
        match self.append(record) {
            Ok(path) => Some(path),
            Err(error) => {
                tracing::warn!(
                    conversation_id = %record.conversation_id,
                    error = %error,
                    "standalone usage ledger append failed"
                );
                None
            }
        }
    }

    /// Every record, oldest file first; malformed lines are skipped.
    pub fn read_records(&self) -> io::Result<Vec<UsageRecord>> {
        let mut records = Vec::new();
        for file in self.ledger_files()? {
            let handle = File::open(&file.path)?;
            for (index, line) in BufReader::new(handle).lines().enumerate() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<UsageRecord>(&line) {
                    Ok(record) => records.push(record),
                    Err(error) => tracing::warn!(
                        path = %file.path.display(),
                        line = index + 1,
                        error = %error,
                        "skipping malformed usage ledger line"
                    ),
                }
            }
        }
        Ok(records)
    }

    /// Fold the matching records into rollups.
    pub fn summary(&self, filter: &UsageFilter) -> io::Result<LedgerSummary> {
        let records = self.read_records()?;
        Ok(summarize_filtered(records.iter(), filter))
    }

    /// Enforce the retention window and byte budget, never removing the active
    /// file. Files older than the retention window go first, then the oldest
    /// remaining files while the total exceeds the byte budget.
    pub fn prune(&self) -> io::Result<PruneReport> {
        self.prune_at(SystemTime::now())
    }

    pub fn prune_at(&self, now: SystemTime) -> io::Result<PruneReport> {
        let millis = unix_millis(now);
        let month = utc_year_month(millis);
        let (active, _) = self.active_file(utc_month_index(millis), &month, 0)?;
        let cutoff = utc_month_index(millis) - i64::from(self.retention_months);
        let mut total: u64 = self.ledger_files()?.iter().map(|file| file.size).sum();
        let mut report = PruneReport::default();
        for file in self.ledger_files()? {
            if file.path == active {
                continue;
            }
            let too_old = file.month_index < cutoff;
            let over_budget = total > self.max_total_bytes;
            if !too_old && !over_budget {
                continue;
            }
            match fs::remove_file(&file.path) {
                Ok(()) => {
                    report.files_removed += 1;
                    report.bytes_removed += file.size;
                    total = total.saturating_sub(file.size);
                }
                Err(error) => tracing::warn!(
                    path = %file.path.display(),
                    error = %error,
                    "could not prune usage ledger file"
                ),
            }
        }
        Ok(report)
    }

    /// Parse one Pi session JSONL file and append the records the ledger does
    /// not already hold. Idempotent by `(conversation_id, session_file,
    /// message_id, timestamp)`, so re-running never double-counts.
    pub fn backfill_session(
        &self,
        conversation_id: &str,
        session_file: &Path,
        pricing: &BTreeMap<String, ModelPricing>,
    ) -> io::Result<BackfillReport> {
        let mut report = BackfillReport::default();
        let metadata = match fs::metadata(session_file) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(report),
            Err(error) => return Err(error),
        };
        report.files_scanned = 1;
        let session_key = session_file.to_string_lossy().into_owned();
        let mut state = self.read_backfill_state();
        let marker = BackfillFileState::of(&metadata);
        if state.files.get(&session_key) == Some(&marker) {
            report.files_skipped = 1;
            return Ok(report);
        }
        let contents = fs::read_to_string(session_file)?;
        let parsed = parse_session_jsonl(session_file, &contents, conversation_id, pricing);
        let existing: HashSet<BackfillKey> = self
            .read_records()?
            .iter()
            .filter_map(BackfillKey::of_record)
            .collect();
        for record in parsed {
            let Some(key) = BackfillKey::of_record(&record) else {
                continue;
            };
            if existing.contains(&key) {
                report.records_skipped += 1;
                continue;
            }
            self.append(&record)?;
            report.records_imported += 1;
        }
        state.files.insert(session_key, marker);
        self.write_backfill_state(&state)?;
        Ok(report)
    }

    /// Backfill every `*.jsonl` under `session_dir` that a conversation in
    /// `conversation_map` (conversation id → session file) points at.
    pub fn backfill_from_sessions(
        &self,
        session_dir: &Path,
        conversation_map: &BTreeMap<String, String>,
        pricing: &BTreeMap<String, ModelPricing>,
    ) -> io::Result<BackfillReport> {
        let mut report = BackfillReport::default();
        let entries = match fs::read_dir(session_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(report),
            Err(error) => return Err(error),
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "jsonl")
            })
            .collect();
        files.sort();
        for session_file in files {
            let conversation_id = conversation_map.iter().find_map(|(conversation, mapped)| {
                let mapped = Path::new(mapped);
                (mapped == session_file || mapped.file_name() == session_file.file_name())
                    .then(|| conversation.clone())
            });
            let Some(conversation_id) = conversation_id else {
                report.files_unmapped += 1;
                continue;
            };
            report.merge(self.backfill_session(&conversation_id, &session_file, pricing)?);
        }
        Ok(report)
    }

    fn backfill_state_path(&self) -> PathBuf {
        self.dir.join("backfill.json")
    }

    fn read_backfill_state(&self) -> BackfillState {
        fs::read_to_string(self.backfill_state_path())
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    fn write_backfill_state(&self, state: &BackfillState) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let raw = serde_json::to_string_pretty(state).map_err(io::Error::other)?;
        fs::write(self.backfill_state_path(), raw)
    }

    fn ledger_files(&self) -> io::Result<Vec<LedgerFile>> {
        let mut files = Vec::new();
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(files),
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some((month_index, suffix)) = parse_ledger_file_name(name) else {
                continue;
            };
            let size = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            files.push(LedgerFile {
                month_index,
                suffix,
                path: entry.path(),
                size,
            });
        }
        files.sort_by_key(|file| (file.month_index, file.suffix));
        Ok(files)
    }

    /// The file the next record goes into: the highest-suffix file for `month`
    /// while it still has room, otherwise a fresh numbered sibling.
    fn active_file(
        &self,
        month_index: i64,
        month: &str,
        incoming: u64,
    ) -> io::Result<(PathBuf, bool)> {
        let files = self.ledger_files()?;
        let suffix = files
            .iter()
            .filter(|file| file.month_index == month_index)
            .max_by_key(|file| file.suffix)
            .and_then(|file| {
                file.size
                    .checked_add(incoming)
                    .is_some_and(|total| total <= self.max_file_bytes)
                    .then_some(file.suffix)
            })
            .unwrap_or_else(|| {
                files
                    .iter()
                    .filter(|file| file.month_index == month_index)
                    .map(|file| file.suffix)
                    .max()
                    .map_or(1, |suffix| suffix + 1)
            });
        let path = self.dir.join(ledger_file_name(month, suffix));
        let created = !path.exists();
        Ok((path, created))
    }
}

fn ledger_file_name(month: &str, suffix: u32) -> String {
    if suffix <= 1 {
        format!("ledger-{month}.jsonl")
    } else {
        format!("ledger-{month}-{suffix:02}.jsonl")
    }
}

fn parse_ledger_file_name(name: &str) -> Option<(i64, u32)> {
    let rest = name.strip_prefix("ledger-")?.strip_suffix(".jsonl")?;
    let (month, suffix) = match rest.len() {
        7 => (rest, 1),
        10 => (rest.get(..7)?, rest.get(8..)?.parse().ok()?),
        _ => return None,
    };
    if rest.as_bytes().get(7).is_some_and(|byte| *byte != b'-') {
        return None;
    }
    let year: i64 = month.get(0..4)?.parse().ok()?;
    let month_number: u32 = month.get(5..7)?.parse().ok()?;
    if month.as_bytes().get(4) != Some(&b'-')
        || suffix == 0
        || !(1..=12).contains(&month_number)
        || !(0..=9999).contains(&year)
    {
        return None;
    }
    Some((year * 12 + i64::from(month_number) - 1, suffix))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
struct BackfillFileState {
    size: u64,
    mtime_millis: i64,
}

impl BackfillFileState {
    fn of(metadata: &fs::Metadata) -> Self {
        let mtime_millis = metadata.modified().map(unix_millis).unwrap_or_default();
        Self {
            size: metadata.len(),
            mtime_millis,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BackfillState {
    #[serde(default)]
    files: BTreeMap<String, BackfillFileState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BackfillKey {
    conversation_id: String,
    session_file: String,
    message_id: String,
    ts: String,
}

impl BackfillKey {
    fn of_record(record: &UsageRecord) -> Option<Self> {
        let session_file = record.session_file.as_ref()?;
        Some(Self {
            conversation_id: record.conversation_id.clone(),
            session_file: session_file.clone(),
            message_id: record.message_id.clone().unwrap_or_default(),
            ts: record.ts.clone(),
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PiSessionLine {
    Session {
        #[serde(default)]
        timestamp: Option<String>,
    },
    ModelChange {
        #[serde(default)]
        provider: Option<String>,
        #[serde(default, rename = "modelId")]
        model_id: Option<String>,
    },
    Message {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        timestamp: Option<String>,
        message: PiMessage,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct PiMessage {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default, rename = "stopReason")]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<PiUsage>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
struct PiUsage {
    #[serde(default)]
    input: u64,
    #[serde(default)]
    output: u64,
    #[serde(default, rename = "cacheRead")]
    cache_read: u64,
    #[serde(default, rename = "cacheWrite")]
    cache_write: u64,
    #[serde(default)]
    reasoning: u64,
    #[serde(default, rename = "totalTokens")]
    total_tokens: Option<u64>,
}

impl PiUsage {
    fn counters(self) -> UsageCounters {
        UsageCounters {
            input: self.input,
            output: self.output,
            cache_read: self.cache_read,
            cache_write: self.cache_write,
            reasoning: self.reasoning,
            total: self.total_tokens.unwrap_or_else(|| {
                self.input
                    .saturating_add(self.output)
                    .saturating_add(self.cache_read)
                    .saturating_add(self.cache_write)
            }),
        }
    }
}

/// Parse one Pi session JSONL file into backfill records: every assistant
/// `message` with non-zero `usage` becomes one record, and provider/model fall
/// back to the most recent `model_change` line. Pure: no I/O, no clock.
pub fn parse_session_jsonl(
    session_file: &Path,
    contents: &str,
    conversation_id: &str,
    pricing: &BTreeMap<String, ModelPricing>,
) -> Vec<UsageRecord> {
    let session_key = session_file.to_string_lossy().into_owned();
    let mut records = Vec::new();
    let mut session_timestamp: Option<String> = None;
    let mut current_provider: Option<String> = None;
    let mut current_model: Option<String> = None;
    for (index, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim_start_matches('\u{feff}').trim();
        if line.is_empty() {
            continue;
        }
        let parsed = match serde_json::from_str::<PiSessionLine>(line) {
            Ok(parsed) => parsed,
            Err(error) => {
                tracing::debug!(
                    session_file = %session_key,
                    line = index + 1,
                    error = %error,
                    "skipping unparseable Pi session line"
                );
                continue;
            }
        };
        match parsed {
            PiSessionLine::Session { timestamp, .. } => {
                session_timestamp = timestamp;
            }
            PiSessionLine::ModelChange { provider, model_id } => {
                current_provider = provider.or(current_provider);
                current_model = model_id.or(current_model);
            }
            PiSessionLine::Message {
                id,
                timestamp,
                message,
            } => {
                if message.role.as_deref() != Some("assistant") {
                    continue;
                }
                let Some(usage) = message.usage else {
                    continue;
                };
                let counters = usage.counters();
                if counters.is_zero() {
                    continue;
                }
                let provider = message.provider.or_else(|| current_provider.clone());
                let model = message
                    .model
                    .or_else(|| current_model.clone())
                    .unwrap_or_else(|| DEFAULT_FALLBACK_ID.to_string());
                let profile_id = provider
                    .as_deref()
                    .map(|provider| {
                        provider
                            .strip_prefix("warpi-")
                            .unwrap_or(provider)
                            .to_string()
                    })
                    .filter(|profile| !profile.is_empty())
                    .unwrap_or_else(|| DEFAULT_FALLBACK_ID.to_string());
                let ts = timestamp
                    .or_else(|| session_timestamp.clone())
                    .unwrap_or_default();
                let mut record = UsageRecord {
                    v: LEDGER_VERSION,
                    ts,
                    source: RecordSource::Backfill,
                    conversation_id: conversation_id.to_string(),
                    exchange_id: None,
                    message_id: id,
                    profile_id,
                    provider_id: provider,
                    model_id: model,
                    stop_reason: message.stop_reason,
                    category: RecordCategory::PrimaryAgent,
                    usage: counters,
                    timing: None,
                    cost: None,
                    context: None,
                    compaction: None,
                    session_file: Some(session_key.clone()),
                };
                record.apply_pricing(pricing);
                records.push(record);
            }
            PiSessionLine::Other => {}
        }
    }
    records
}

#[cfg(test)]
#[path = "usage_ledger_tests.rs"]
mod tests;
