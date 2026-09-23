use chrono::{Duration as ChronoDuration, Local};
use standalone_agent::usage_ledger::{
    CostEstimate, LEDGER_VERSION, RecordCategory, RecordSource, UsageCounters, UsageRecord,
    UsageTiming,
};

use super::*;
use crate::ai::agent::request_metadata::{
    InferenceUsageType, RequestLlmGenerationSpan, RequestMetadataRecord, RequestModelCharge,
};

fn ledger_record() -> UsageRecord {
    UsageRecord {
        v: LEDGER_VERSION,
        ts: "2026-09-23T00:00:00.000Z".to_string(),
        source: RecordSource::Live,
        conversation_id: "conv".to_string(),
        exchange_id: Some("exchange".to_string()),
        message_id: Some("message".to_string()),
        profile_id: "profile".to_string(),
        provider_id: Some("warpi-profile".to_string()),
        model_id: "model".to_string(),
        stop_reason: Some("stop".to_string()),
        category: RecordCategory::PrimaryAgent,
        usage: UsageCounters {
            input: 1_200,
            output: 485,
            cache_read: 1_400,
            cache_write: 0,
            reasoning: 20,
            total: 1_900,
        },
        timing: Some(UsageTiming {
            generation_ms: Some(5_100),
            first_token_ms: Some(210),
            wall_ms: None,
        }),
        cost: Some(CostEstimate::new(0.0004)),
        context: None,
        compaction: None,
        session_file: None,
    }
}

fn metadata_record() -> RequestMetadataRecord {
    let started = Local::now();
    RequestMetadataRecord {
        message_id: "message".to_string(),
        request_id: "req-1".to_string(),
        model_charges: vec![RequestModelCharge {
            category: "primary_agent".to_string(),
            usage_type: InferenceUsageType::CustomEndpoint,
            model_id: "model".to_string(),
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 20,
            cache_write_tokens: 0,
            input_cost_in_cents: 0.4,
            output_cost_in_cents: 0.6,
            cache_read_cost_in_cents: 0.0,
            cache_write_cost_in_cents: 0.0,
            input_cost_in_credits: 0.0,
            output_cost_in_credits: 0.0,
            cache_read_cost_in_credits: 0.0,
            cache_write_cost_in_credits: 0.0,
            web_search_count: 0,
            web_search_cost_in_cents: 0.0,
            web_search_cost_in_credits: 0.0,
        }],
        llm_generation_spans: vec![RequestLlmGenerationSpan {
            started_at: Some(started),
            ended_at: Some(started + ChronoDuration::milliseconds(1_000)),
        }],
        ..Default::default()
    }
}

#[test]
fn token_counts_render_compactly() {
    assert_eq!(format_token_count(0), "0");
    assert_eq!(format_token_count(485), "485");
    assert_eq!(format_token_count(1_000), "1k");
    assert_eq!(format_token_count(1_200), "1.2k");
    assert_eq!(format_token_count(12_345), "12.3k");
    assert_eq!(format_token_count(1_900_000), "1.9M");
}

#[test]
fn ledger_record_becomes_a_footer_line() {
    let entry = StandaloneUsageEntry::from_ledger_record(&ledger_record());

    assert_eq!(entry.input_tokens, 1_200);
    assert_eq!(entry.output_tokens, 485);
    assert_eq!(entry.cache_read_tokens, 1_400);
    assert_eq!(entry.reasoning_tokens, Some(20));
    assert_eq!(entry.total_tokens, Some(1_900));
    assert_eq!(entry.generation_ms, Some(5_100));
    assert_eq!(entry.cost_usd, Some(0.0004));

    let line = format_usage_line(&entry).expect("a used call renders a line");
    assert_eq!(
        line,
        "1.2k in · 485 out (20 reasoning) · 1.4k cached · 1.9k total · 95.1 tok/s · 5.1s · est. $0.0004"
    );
}

#[test]
fn zero_usage_renders_no_line() {
    let entry = StandaloneUsageEntry::from_ledger_record(&UsageRecord {
        usage: UsageCounters::default(),
        timing: None,
        cost: None,
        ..ledger_record()
    });
    assert_eq!(entry.total_tokens, Some(0));
    assert_eq!(format_usage_line(&entry), None);
    assert!(usage_line_segments(&entry).is_empty());
}

#[test]
fn missing_pricing_row_leaves_cost_unlabelled() {
    let entry = StandaloneUsageEntry::from_ledger_record(&UsageRecord {
        cost: None,
        ..ledger_record()
    });
    let line = format_usage_line(&entry).expect("usage without cost still renders");
    assert!(
        !line.contains("est."),
        "an unknown cost must not render a fabricated $0: {line}"
    );
}

#[test]
fn throughput_is_unknown_without_output_or_duration() {
    let mut entry = StandaloneUsageEntry {
        output_tokens: 100,
        generation_ms: Some(1_000),
        ..Default::default()
    };
    assert_eq!(entry.tokens_per_second(), Some(100.));

    entry.generation_ms = None;
    assert_eq!(entry.tokens_per_second(), None);

    entry.generation_ms = Some(0);
    assert_eq!(entry.tokens_per_second(), None);

    entry.generation_ms = Some(1_000);
    entry.output_tokens = 0;
    assert_eq!(entry.tokens_per_second(), None);
}

#[test]
fn request_metadata_fallback_aggregates_tokens_and_timing() {
    let entry = StandaloneUsageEntry::from_request_metadata(&[metadata_record()])
        .expect("charges produce an entry");
    assert_eq!(entry.input_tokens, 100);
    assert_eq!(entry.output_tokens, 50);
    assert_eq!(entry.cache_read_tokens, 20);
    assert_eq!(entry.reasoning_tokens, None);
    assert_eq!(entry.total_tokens, Some(170));
    assert_eq!(entry.generation_ms, Some(1_000));
    assert_eq!(entry.cost_usd, Some(0.01));

    assert_eq!(
        StandaloneUsageEntry::from_request_metadata(&[]),
        None,
        "no records means no footer"
    );
}

#[test]
fn context_percent_prefers_reported_percent_and_falls_back_to_ratio() {
    let reported = StandaloneContextReading {
        percent: Some(82.4),
        ..Default::default()
    };
    assert_eq!(reported.percent_used(), Some(82.4));
    assert!((reported.fraction_used().unwrap() - 0.824).abs() < 1e-6);

    let ratio = StandaloneContextReading {
        tokens: Some(90_000),
        context_window: Some(100_000),
        ..Default::default()
    };
    assert_eq!(ratio.percent_used(), Some(90.0));
}

#[test]
fn unknown_context_never_renders_zero_percent() {
    let unknown = StandaloneContextReading::default();
    assert_eq!(unknown.percent_used(), None);
    assert_eq!(context_meter_label(Some(&unknown)), None);
    assert_eq!(context_meter_label(None), None);
    assert_eq!(
        context_meter_level(None),
        ContextMeterLevel::Unavailable,
        "missing data is unavailable, never a normal 0% reading"
    );
    assert!(context_meter_tooltip(None).contains("unavailable"));

    let non_finite = StandaloneContextReading {
        percent: Some(f64::NAN),
        ..Default::default()
    };
    assert_eq!(non_finite.percent_used(), None);
    assert_eq!(
        context_meter_level(Some(&non_finite)),
        ContextMeterLevel::Unavailable
    );
}

#[test]
fn context_meter_levels_and_tooltip_warnings() {
    let reading = |percent: f64| StandaloneContextReading {
        percent: Some(percent),
        context_window: Some(131_072),
        ..Default::default()
    };

    assert_eq!(
        context_meter_level(Some(&reading(10.0))),
        ContextMeterLevel::Normal
    );
    assert_eq!(
        context_meter_level(Some(&reading(75.0))),
        ContextMeterLevel::Warning
    );
    assert_eq!(
        context_meter_level(Some(&reading(90.0))),
        ContextMeterLevel::Critical
    );

    assert_eq!(
        context_meter_label(Some(&reading(75.4))),
        Some("75%".to_string())
    );
    let tooltip = context_meter_tooltip(Some(&reading(96.0)));
    assert!(tooltip.contains("96% of 131.1k tokens used"), "{tooltip}");
    assert!(tooltip.contains("compaction"), "{tooltip}");
}

#[test]
fn the_live_store_accumulates_per_request() {
    record_message_usage(
        "request-accumulate",
        StandaloneUsageEntry {
            output_tokens: 10,
            generation_ms: Some(100),
            ..Default::default()
        },
    );
    record_message_usage(
        "request-accumulate",
        StandaloneUsageEntry {
            output_tokens: 5,
            generation_ms: Some(100),
            reasoning_tokens: Some(7),
            ..Default::default()
        },
    );

    let entry = entry_for_request("request-accumulate").expect("recorded");
    assert_eq!(entry.output_tokens, 15);
    assert_eq!(entry.generation_ms, Some(200));
    assert_eq!(entry.reasoning_tokens, Some(7));
    assert!(entry_for_request("request-missing").is_none());

    record_context(
        "conversation-store",
        StandaloneContextReading {
            percent: Some(42.0),
            ..Default::default()
        },
    );
    assert_eq!(
        context_for_conversation("conversation-store").and_then(|reading| reading.percent_used()),
        Some(42.0)
    );
    assert!(context_for_conversation("conversation-missing").is_none());
}
