//! Live token usage and context-window facts for standalone responses.
//!
//! `run_exchange` records one [`StandaloneUsageEntry`] per standalone request and
//! the latest [`StandaloneContextReading`] per conversation as the bridge's usage
//! events arrive. The per-response footer and the input-footer context meter
//! read them back on every render. The map is deliberately process-local: the
//! durable source is the usage ledger, and a cold (restored) conversation falls
//! back to the native request-metadata records.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use standalone_agent::usage_ledger::{ContextSource, UsageRecord};

use crate::ai::agent::request_metadata::RequestMetadataRecord;

/// Percent of the context window used at which the meter turns amber.
pub const CONTEXT_METER_WARNING_PERCENT: f32 = 75.;
/// Percent of the context window used at which the meter turns red.
pub const CONTEXT_METER_CRITICAL_PERCENT: f32 = 90.;
/// Percent of the context window used at which the tooltip warns about the next
/// turn potentially triggering compaction.
pub const CONTEXT_METER_COMPACTION_PERCENT: f32 = 95.;

/// One context-window reading as reported by the helper. Every field is
/// optional so a provider that reports no tokens degrades to "unavailable"
/// rather than a fabricated `0%`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StandaloneContextReading {
    pub tokens: Option<u64>,
    pub context_window: Option<u64>,
    pub percent: Option<f64>,
    pub source: Option<ContextSource>,
}

impl StandaloneContextReading {
    /// The reading as a 0–100 percentage, preferring the provider percentage and
    /// falling back to the token/window ratio. `None` when neither is known.
    pub fn percent_used(&self) -> Option<f32> {
        if let Some(percent) = self.percent.filter(|percent| percent.is_finite()) {
            return Some(percent.clamp(0., 100.) as f32);
        }
        match (self.tokens, self.context_window) {
            (Some(tokens), Some(window)) if window > 0 => {
                Some(((tokens as f64 / window as f64) * 100.).clamp(0., 100.) as f32)
            }
            _ => None,
        }
    }

    /// The 0–1 fraction used, for the meter icon.
    pub fn fraction_used(&self) -> Option<f32> {
        self.percent_used().map(|percent| percent / 100.)
    }
}

/// Aggregated usage for one standalone request (one Warp exchange).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StandaloneUsageEntry {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub generation_ms: Option<u64>,
    pub first_token_ms: Option<u64>,
    pub cost_usd: Option<f64>,
}

impl StandaloneUsageEntry {
    /// Build the live entry from a priced ledger record.
    pub fn from_ledger_record(record: &UsageRecord) -> Self {
        let usage = &record.usage;
        Self {
            input_tokens: usage.input,
            output_tokens: usage.output,
            cache_read_tokens: usage.cache_read,
            cache_write_tokens: usage.cache_write,
            reasoning_tokens: (usage.reasoning > 0).then_some(usage.reasoning),
            total_tokens: Some(usage.total_tokens()),
            generation_ms: record.timing.and_then(|timing| timing.generation_ms),
            first_token_ms: record.timing.and_then(|timing| timing.first_token_ms),
            cost_usd: record.cost.map(|cost| cost.total_usd),
        }
    }

    /// Build the cold (restored) entry from the native request-metadata records.
    /// This path has tokens, timing, and cost but never reasoning. `None` when
    /// the records carry no usage at all.
    pub fn from_request_metadata(records: &[RequestMetadataRecord]) -> Option<Self> {
        let mut entry = Self::default();
        let mut saw_charge = false;
        let mut generation_ms: u64 = 0;
        let mut cost_cents: f64 = 0.;
        for record in records {
            for charge in &record.model_charges {
                entry.input_tokens = entry
                    .input_tokens
                    .saturating_add(u64::from(charge.input_tokens));
                entry.output_tokens = entry
                    .output_tokens
                    .saturating_add(u64::from(charge.output_tokens));
                entry.cache_read_tokens = entry
                    .cache_read_tokens
                    .saturating_add(u64::from(charge.cache_read_tokens));
                entry.cache_write_tokens = entry
                    .cache_write_tokens
                    .saturating_add(u64::from(charge.cache_write_tokens));
                cost_cents += f64::from(charge.cost_in_cents());
                saw_charge = true;
            }
            for span in &record.llm_generation_spans {
                if let Some(ms) = span.duration_ms() {
                    generation_ms = generation_ms.saturating_add(ms.max(0) as u64);
                }
            }
        }
        if !saw_charge && generation_ms == 0 {
            return None;
        }
        entry.total_tokens = Some(
            entry
                .input_tokens
                .saturating_add(entry.output_tokens)
                .saturating_add(entry.cache_read_tokens)
                .saturating_add(entry.cache_write_tokens),
        );
        entry.generation_ms = (generation_ms > 0).then_some(generation_ms);
        entry.cost_usd = (cost_cents > 0.).then_some(cost_cents / 100.);
        Some(entry)
    }

    fn accumulate(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
        self.reasoning_tokens = match (self.reasoning_tokens, other.reasoning_tokens) {
            (Some(current), Some(added)) => Some(current.saturating_add(added)),
            (current, added) => current.or(added),
        };
        self.total_tokens = match (self.total_tokens, other.total_tokens) {
            (Some(current), Some(added)) => Some(current.saturating_add(added)),
            (current, added) => current.or(added),
        };
        self.generation_ms = match (self.generation_ms, other.generation_ms) {
            (Some(current), Some(added)) => Some(current.saturating_add(added)),
            (current, added) => current.or(added),
        };
        self.first_token_ms = self.first_token_ms.or(other.first_token_ms);
        self.cost_usd = match (self.cost_usd, other.cost_usd) {
            (Some(current), Some(added)) => Some(current + added),
            (current, added) => current.or(added),
        };
    }

    /// Output tokens per second of generation, or `None` when either the output
    /// or the generation duration is unknown.
    pub fn tokens_per_second(&self) -> Option<f64> {
        let generation_ms = self.generation_ms.filter(|ms| *ms > 0)?;
        if self.output_tokens == 0 {
            return None;
        }
        Some(self.output_tokens as f64 / generation_ms as f64 * 1000.)
    }
}

#[derive(Default)]
struct StandaloneUsageStore {
    by_request: HashMap<String, StandaloneUsageEntry>,
    context_by_conversation: HashMap<String, StandaloneContextReading>,
}

fn store() -> &'static Mutex<StandaloneUsageStore> {
    static STORE: OnceLock<Mutex<StandaloneUsageStore>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(StandaloneUsageStore::default()))
}

fn with_store<T>(update: impl FnOnce(&mut StandaloneUsageStore) -> T) -> T {
    let mut guard = store()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    update(&mut guard)
}

/// Record (or accumulate) one request's usage. Repeated calls with the same
/// request id fold into the existing entry.
pub fn record_message_usage(request_id: &str, entry: StandaloneUsageEntry) {
    if request_id.is_empty() {
        return;
    }
    with_store(|store| {
        store
            .by_request
            .entry(request_id.to_string())
            .and_modify(|existing| existing.accumulate(&entry))
            .or_insert(entry);
    });
}

/// Record the latest context-window reading for a conversation.
pub fn record_context(conversation_id: &str, reading: StandaloneContextReading) {
    if conversation_id.is_empty() {
        return;
    }
    with_store(|store| {
        store
            .context_by_conversation
            .insert(conversation_id.to_string(), reading);
    });
}

pub fn entry_for_request(request_id: &str) -> Option<StandaloneUsageEntry> {
    with_store(|store| store.by_request.get(request_id).cloned())
}

pub fn context_for_conversation(conversation_id: &str) -> Option<StandaloneContextReading> {
    with_store(|store| store.context_by_conversation.get(conversation_id).copied())
}

/// Compact token count for the footer: `485`, `1.2k`, `4M`.
pub fn format_token_count(tokens: u64) -> String {
    const THOUSAND: f64 = 1_000.;
    const MILLION: f64 = 1_000_000.;
    const BILLION: f64 = 1_000_000_000.;
    let value = tokens as f64;
    if value >= BILLION {
        format_scaled(value / BILLION, "B")
    } else if value >= MILLION {
        format_scaled(value / MILLION, "M")
    } else if value >= THOUSAND {
        format_scaled(value / THOUSAND, "k")
    } else {
        tokens.to_string()
    }
}

fn format_scaled(value: f64, suffix: &str) -> String {
    let formatted = format!("{value:.1}");
    let trimmed = formatted.strip_suffix(".0").unwrap_or(&formatted);
    format!("{trimmed}{suffix}")
}

/// Compact generation duration: `812ms`, `5.1s`.
pub fn format_duration_ms(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.)
    }
}

pub fn format_tokens_per_second(tokens_per_second: f64) -> String {
    format!("{tokens_per_second:.1} tok/s")
}

/// Every footer segment for an entry, in display order. Segments that are
/// unknown are omitted so the line never fabricates a value; the caller can
/// drop segments right-to-left when the pane is narrow.
pub fn usage_line_segments(entry: &StandaloneUsageEntry) -> Vec<String> {
    let mut segments = Vec::new();
    if entry.input_tokens > 0 {
        segments.push(format!("{} in", format_token_count(entry.input_tokens)));
    }
    if entry.output_tokens > 0 {
        let mut output = format!("{} out", format_token_count(entry.output_tokens));
        if let Some(reasoning) = entry.reasoning_tokens.filter(|reasoning| *reasoning > 0) {
            output.push_str(&format!(" ({} reasoning)", format_token_count(reasoning)));
        }
        segments.push(output);
    }
    let cached = entry
        .cache_read_tokens
        .saturating_add(entry.cache_write_tokens);
    if cached > 0 {
        segments.push(format!("{} cached", format_token_count(cached)));
    }
    if let Some(total) = entry.total_tokens.filter(|total| *total > 0) {
        segments.push(format!("{} total", format_token_count(total)));
    }
    if let Some(tokens_per_second) = entry.tokens_per_second() {
        segments.push(format_tokens_per_second(tokens_per_second));
    }
    if let Some(ms) = entry.generation_ms.filter(|ms| *ms > 0) {
        segments.push(format_duration_ms(ms));
    }
    if let Some(usd) = entry.cost_usd.filter(|usd| usd.is_finite() && *usd > 0.) {
        segments.push(if usd < 0.01 {
            format!("est. ${usd:.4}")
        } else {
            format!("est. ${usd:.2}")
        });
    }
    segments
}

/// The full footer line, or `None` when there is nothing to show.
pub fn format_usage_line(entry: &StandaloneUsageEntry) -> Option<String> {
    let segments = usage_line_segments(entry);
    (!segments.is_empty()).then(|| segments.join(" · "))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextMeterLevel {
    /// No provider reading; the meter shows no percentage.
    Unavailable,
    Normal,
    Warning,
    Critical,
}

pub fn context_meter_level(reading: Option<&StandaloneContextReading>) -> ContextMeterLevel {
    match reading.and_then(StandaloneContextReading::percent_used) {
        None => ContextMeterLevel::Unavailable,
        Some(percent) if percent >= CONTEXT_METER_CRITICAL_PERCENT => ContextMeterLevel::Critical,
        Some(percent) if percent >= CONTEXT_METER_WARNING_PERCENT => ContextMeterLevel::Warning,
        Some(_) => ContextMeterLevel::Normal,
    }
}

/// The meter's percentage label, or `None` when the reading is unavailable (so
/// the caller renders no label instead of a fake `0%`).
pub fn context_meter_label(reading: Option<&StandaloneContextReading>) -> Option<String> {
    reading
        .and_then(StandaloneContextReading::percent_used)
        .map(|percent| format!("{percent:.0}%"))
}

pub fn context_meter_tooltip(reading: Option<&StandaloneContextReading>) -> String {
    let Some(percent) = reading.and_then(StandaloneContextReading::percent_used) else {
        return "Context usage unavailable (provider did not report tokens)".to_string();
    };
    let window = reading.and_then(|reading| reading.context_window);
    let mut tooltip = match window.filter(|window| *window > 0) {
        Some(window) => format!(
            "{percent:.0}% of {} tokens used",
            format_token_count(window)
        ),
        None => format!("{percent:.0}% of the context window used"),
    };
    if percent >= CONTEXT_METER_COMPACTION_PERCENT {
        tooltip.push_str(" · the next turn may trigger compaction");
    }
    tooltip
}

#[cfg(test)]
#[path = "usage_model_tests.rs"]
mod tests;
