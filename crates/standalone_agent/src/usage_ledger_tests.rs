//! Unit tests for the local usage ledger: schema, rotation, pruning, backfill
//! idempotence, rollups, and pricing edges.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tempfile::TempDir;

use crate::usage_ledger::{
    ContextSource, ContextUsage, CostEstimate, ModelPricing, RecordCategory, RecordSource,
    UsageCounters, UsageFilter, UsageLedger, UsageRecord, UsageTiming, estimate_cost,
    format_rfc3339_millis, parse_rfc3339_millis, parse_session_jsonl, summarize,
    summarize_filtered, unix_millis, usage_dir, utc_day_of,
};

const SESSION_FIXTURE: &str = r#"{"type":"session","version":3,"id":"sess-1","timestamp":"2026-09-23T16:06:29.297Z","cwd":"/tmp"}
{"type":"model_change","id":"m1","parentId":null,"timestamp":"2026-09-23T16:06:29.304Z","provider":"warpi-profile-a","modelId":"model-fallback"}
{"type":"message","id":"u1","parentId":null,"timestamp":"2026-09-23T16:06:30.000Z","message":{"role":"user","content":[]}}
{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-09-23T16:06:30.901Z","message":{"role":"assistant","model":"model-a","provider":"warpi-profile-a","stopReason":"stop","usage":{"input":1000,"output":500,"cacheRead":200,"cacheWrite":0,"reasoning":50,"totalTokens":1700,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"content":[]}}
{"type":"message","id":"a2","parentId":"a1","timestamp":"2026-09-23T16:06:48.758Z","message":{"role":"assistant","stopReason":"toolUse","usage":{"input":200,"output":100,"cacheRead":0,"cacheWrite":10,"reasoning":0,"totalTokens":310},"content":[]}}
{"type":"message","id":"a3","parentId":"a2","timestamp":"2026-09-23T16:06:49.000Z","message":{"role":"assistant","model":"model-a","provider":"warpi-profile-a","stopReason":"error","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0,"totalTokens":0},"content":[]}}
{"type":"message","id":"a4","parentId":"a3","timestamp":"2026-09-23T16:06:50.000Z","message":{"role":"assistant","model":"model-b","provider":"other-provider","stopReason":"stop","usage":{"input":10,"output":5,"cacheRead":1,"cacheWrite":0,"reasoning":0,"totalTokens":16},"content":[]}}
"#;

fn at(value: &str) -> SystemTime {
    let millis = parse_rfc3339_millis(value).expect("valid timestamp");
    UNIX_EPOCH + Duration::from_millis(u64::try_from(millis).expect("post-epoch timestamp"))
}

fn ledger(dir: &Path) -> UsageLedger {
    UsageLedger::new(dir).with_limits(8 * 1024 * 1024, 64 * 1024 * 1024)
}

fn pricing() -> BTreeMap<String, ModelPricing> {
    BTreeMap::from([
        (
            "model-a".to_string(),
            ModelPricing {
                input: 1.0,
                output: 2.0,
                cache_read: Some(0.1),
                cache_write: None,
            },
        ),
        (
            "model-fallback".to_string(),
            ModelPricing {
                input: 0.5,
                output: 1.0,
                cache_read: None,
                cache_write: None,
            },
        ),
    ])
}

fn live_record(ts: &str) -> UsageRecord {
    UsageRecord {
        v: 1,
        ts: ts.to_string(),
        source: RecordSource::Live,
        conversation_id: "conv-1".to_string(),
        exchange_id: Some("ex-1".to_string()),
        message_id: Some("msg-1".to_string()),
        profile_id: "profile-a".to_string(),
        provider_id: Some("warpi-profile-a".to_string()),
        model_id: "deepseek-flash".to_string(),
        stop_reason: Some("stop".to_string()),
        category: RecordCategory::PrimaryAgent,
        usage: UsageCounters {
            input: 160,
            output: 56,
            cache_read: 1280,
            cache_write: 0,
            reasoning: 13,
            total: 1496,
        },
        timing: Some(UsageTiming {
            generation_ms: Some(812),
            first_token_ms: Some(210),
            wall_ms: Some(915),
        }),
        cost: Some(CostEstimate::new(0.000123)),
        context: Some(ContextUsage {
            tokens: Some(7937),
            context_window: Some(131072),
            percent: Some(6.06),
            source: ContextSource::Usage,
        }),
        compaction: None,
        session_file: None,
    }
}

fn short_record(ts: &str, conversation: &str) -> UsageRecord {
    UsageRecord {
        v: 1,
        ts: ts.to_string(),
        source: RecordSource::Live,
        conversation_id: conversation.to_string(),
        exchange_id: None,
        message_id: None,
        profile_id: "p".to_string(),
        provider_id: None,
        model_id: "m".to_string(),
        stop_reason: None,
        category: RecordCategory::PrimaryAgent,
        usage: UsageCounters {
            input: 1,
            ..Default::default()
        },
        timing: None,
        cost: None,
        context: None,
        compaction: None,
        session_file: None,
    }
}

fn keyed_record(
    conversation: &str,
    profile: &str,
    model: &str,
    ts: &str,
    category: RecordCategory,
    source: RecordSource,
) -> UsageRecord {
    UsageRecord {
        v: 1,
        ts: ts.to_string(),
        source,
        conversation_id: conversation.to_string(),
        exchange_id: None,
        message_id: None,
        profile_id: profile.to_string(),
        provider_id: None,
        model_id: model.to_string(),
        stop_reason: None,
        category,
        usage: UsageCounters::default(),
        timing: None,
        cost: None,
        context: None,
        compaction: None,
        session_file: None,
    }
}

fn line_count(path: &Path) -> usize {
    fs::read_to_string(path)
        .expect("ledger file")
        .lines()
        .count()
}

#[test]
fn usage_dir_matches_the_standalone_data_dir_convention() {
    assert_eq!(
        usage_dir(Path::new("/data/standalone")),
        PathBuf::from("/data/standalone/usage")
    );
}

#[test]
fn appends_one_json_object_per_line_with_the_spec_schema() {
    let dir = TempDir::new().unwrap();
    let record = live_record("2026-09-23T16:20:11.123Z");
    let path = ledger(dir.path())
        .append_at(&record, at("2026-09-23T16:20:11.123Z"))
        .expect("append");
    assert_eq!(path.file_name().unwrap(), "ledger-2026-09.jsonl");

    let raw = fs::read_to_string(&path).unwrap();
    assert_eq!(raw.lines().count(), 1);
    assert!(raw.ends_with('\n'));
    let decoded: UsageRecord = serde_json::from_str(raw.trim()).unwrap();
    assert_eq!(decoded, record);

    let value: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
    assert_eq!(value["v"], 1);
    assert_eq!(value["source"], "live");
    assert_eq!(value["ts"], "2026-09-23T16:20:11.123Z");
    assert_eq!(value["exchange_id"], "ex-1");
    assert_eq!(value["message_id"], "msg-1");
    assert_eq!(value["provider_id"], "warpi-profile-a");
    assert_eq!(value["stop_reason"], "stop");
    assert_eq!(value["category"], "primary_agent");
    assert_eq!(value["usage"]["input"], 160);
    assert_eq!(value["usage"]["output"], 56);
    assert_eq!(value["usage"]["cache_read"], 1280);
    assert_eq!(value["usage"]["cache_write"], 0);
    assert_eq!(value["usage"]["reasoning"], 13);
    assert_eq!(value["usage"]["total"], 1496);
    assert_eq!(value["timing"]["generation_ms"], 812);
    assert_eq!(value["timing"]["first_token_ms"], 210);
    assert_eq!(value["timing"]["wall_ms"], 915);
    assert_eq!(value["cost"]["total_usd"], 0.000123);
    assert_eq!(value["cost"]["estimated"], true);
    assert_eq!(value["cost"]["pricing_version"], 1);
    assert_eq!(value["context"]["tokens"], 7937);
    assert_eq!(value["context"]["context_window"], 131072);
    assert_eq!(value["context"]["source"], "usage");
}

#[test]
fn minimal_records_omit_absent_optional_fields_and_default_on_read() {
    let record = short_record("2026-09-23T16:20:11.123Z", "conv-1");
    let value = serde_json::to_value(&record).unwrap();
    for key in [
        "exchange_id",
        "message_id",
        "provider_id",
        "stop_reason",
        "timing",
        "cost",
        "context",
        "compaction",
        "session_file",
    ] {
        assert!(value.get(key).is_none(), "{key} should be omitted");
    }
    assert_eq!(value["category"], "primary_agent");

    let decoded: UsageRecord = serde_json::from_str(
        r#"{"ts":"2026-09-23T16:20:11.123Z","source":"live","conversation_id":"c",
            "profile_id":"p","model_id":"m","usage":{"input":1}}"#,
    )
    .expect("partial record deserializes");
    assert_eq!(decoded.v, 1);
    assert_eq!(decoded.category, RecordCategory::PrimaryAgent);
    assert_eq!(decoded.usage.total_tokens(), 1);
    assert!(decoded.cost.is_none());
}

#[test]
fn rotates_to_a_new_file_at_a_month_boundary() {
    let dir = TempDir::new().unwrap();
    let ledger = ledger(dir.path());
    let september = short_record("2026-09-30T23:59:59.000Z", "september");
    let october = short_record("2026-10-01T00:00:01.000Z", "october");
    ledger
        .append_at(&september, at("2026-09-30T23:59:59.000Z"))
        .unwrap();
    ledger
        .append_at(&october, at("2026-10-01T00:00:01.000Z"))
        .unwrap();
    assert!(
        fs::read_to_string(dir.path().join("ledger-2026-09.jsonl"))
            .unwrap()
            .contains("september")
    );
    assert!(
        fs::read_to_string(dir.path().join("ledger-2026-10.jsonl"))
            .unwrap()
            .contains("october")
    );
}

#[test]
fn rotates_to_a_numbered_sibling_when_the_active_file_is_full() {
    let dir = TempDir::new().unwrap();
    let record = short_record("2026-09-10T00:00:00.000Z", "conv-1");
    let line = serde_json::to_string(&record).unwrap().len() as u64 + 1;
    let ledger = ledger(dir.path()).with_limits(line * 2, u64::MAX);
    for _ in 0..4 {
        ledger
            .append_at(&record, at("2026-09-10T00:00:00.000Z"))
            .unwrap();
    }
    assert_eq!(line_count(&dir.path().join("ledger-2026-09.jsonl")), 2);
    assert_eq!(line_count(&dir.path().join("ledger-2026-09-02.jsonl")), 2);
    assert!(!dir.path().join("ledger-2026-09-03.jsonl").exists());
}

#[test]
fn rotation_reuses_the_highest_sibling_that_still_has_room() {
    let dir = TempDir::new().unwrap();
    let record = short_record("2026-09-10T00:00:00.000Z", "conv-1");
    let line = serde_json::to_string(&record).unwrap().len() as u64 + 1;
    fs::write(
        dir.path().join("ledger-2026-09.jsonl"),
        vec![b'x'; (line + 50) as usize],
    )
    .unwrap();
    fs::write(dir.path().join("ledger-2026-09-02.jsonl"), b"{}\n").unwrap();
    let ledger = ledger(dir.path()).with_limits(line + 20, u64::MAX);
    let path = ledger.append_at(&record, at(&record.ts)).unwrap();
    assert_eq!(path.file_name().unwrap(), "ledger-2026-09-02.jsonl");
    assert_eq!(
        fs::read_to_string(dir.path().join("ledger-2026-09.jsonl"))
            .unwrap()
            .len(),
        (line + 50) as usize
    );
    assert!(!dir.path().join("ledger-2026-09-03.jsonl").exists());
}

#[test]
fn pruning_removes_files_outside_the_retention_window() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("ledger-2026-01.jsonl"), b"x").unwrap();
    fs::write(dir.path().join("ledger-2026-05.jsonl"), b"x").unwrap();
    let report = ledger(dir.path())
        .with_retention_months(6)
        .prune_at(at("2026-09-15T00:00:00.000Z"))
        .unwrap();
    assert_eq!(report.files_removed, 1);
    assert!(!dir.path().join("ledger-2026-01.jsonl").exists());
    assert!(dir.path().join("ledger-2026-05.jsonl").exists());
}

#[test]
fn pruning_enforces_the_byte_budget_oldest_first() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("ledger-2026-08.jsonl"), vec![b'x'; 150]).unwrap();
    fs::write(dir.path().join("ledger-2026-09.jsonl"), vec![b'x'; 30]).unwrap();
    let report = ledger(dir.path())
        .with_limits(1024, 100)
        .with_retention_months(240)
        .prune_at(at("2026-09-15T00:00:00.000Z"))
        .unwrap();
    assert_eq!(report.files_removed, 1);
    assert_eq!(report.bytes_removed, 150);
    assert!(!dir.path().join("ledger-2026-08.jsonl").exists());
    assert!(dir.path().join("ledger-2026-09.jsonl").exists());
}

#[test]
fn pruning_never_removes_the_active_file() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("ledger-2026-08.jsonl"), vec![b'x'; 150]).unwrap();
    fs::write(dir.path().join("ledger-2026-09.jsonl"), vec![b'x'; 30]).unwrap();
    let ledger = ledger(dir.path())
        .with_limits(1024, 1)
        .with_retention_months(240);
    let report = ledger.prune_at(at("2026-09-15T00:00:00.000Z")).unwrap();
    assert_eq!(report.files_removed, 1);
    assert!(dir.path().join("ledger-2026-09.jsonl").exists());
    let again = ledger.prune_at(at("2026-09-15T00:00:00.000Z")).unwrap();
    assert_eq!(again.files_removed, 0);
}

#[test]
fn rollover_prunes_expired_files() {
    let dir = TempDir::new().unwrap();
    let record = short_record("2026-09-10T00:00:00.000Z", "conv-1");
    let line = serde_json::to_string(&record).unwrap().len() as u64 + 1;
    fs::write(
        dir.path().join("ledger-2026-09.jsonl"),
        vec![b'x'; (line + 10) as usize],
    )
    .unwrap();
    fs::write(dir.path().join("ledger-2026-01.jsonl"), b"old").unwrap();
    ledger(dir.path())
        .with_limits(line + 20, u64::MAX)
        .with_retention_months(1)
        .append_at(&record, at(&record.ts))
        .unwrap();
    assert!(!dir.path().join("ledger-2026-01.jsonl").exists());
    assert!(dir.path().join("ledger-2026-09-02.jsonl").exists());
}

#[test]
fn read_records_skips_malformed_lines() {
    let dir = TempDir::new().unwrap();
    let first = serde_json::to_string(&short_record("2026-09-01T00:00:00.000Z", "one")).unwrap();
    let second = serde_json::to_string(&short_record("2026-09-02T00:00:00.000Z", "two")).unwrap();
    fs::write(
        dir.path().join("ledger-2026-09.jsonl"),
        format!("{first}\nnot json\n{second}\n"),
    )
    .unwrap();
    let records = ledger(dir.path()).read_records().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].conversation_id, "one");
    assert_eq!(records[1].conversation_id, "two");
}

#[test]
fn best_effort_append_swallows_io_errors() {
    let dir = TempDir::new().unwrap();
    let blocked = dir.path().join("not-a-directory");
    fs::write(&blocked, b"x").unwrap();
    let ledger = UsageLedger::new(&blocked);
    let record = short_record("2026-09-10T00:00:00.000Z", "conv-1");
    assert!(ledger.append(&record).is_err());
    assert_eq!(ledger.append_best_effort(&record), None);
}

#[test]
fn parses_session_jsonl_and_derives_provider_and_model() {
    let dir = TempDir::new().unwrap();
    let session_file = dir.path().join("session-1.jsonl");
    let records = parse_session_jsonl(&session_file, SESSION_FIXTURE, "conv-1", &pricing());
    assert_eq!(records.len(), 3);

    let first = &records[0];
    assert_eq!(first.v, 1);
    assert_eq!(first.source, RecordSource::Backfill);
    assert_eq!(first.ts, "2026-09-23T16:06:30.901Z");
    assert_eq!(first.conversation_id, "conv-1");
    assert_eq!(first.exchange_id, None);
    assert_eq!(first.message_id.as_deref(), Some("a1"));
    assert_eq!(first.profile_id, "profile-a");
    assert_eq!(first.provider_id.as_deref(), Some("warpi-profile-a"));
    assert_eq!(first.model_id, "model-a");
    assert_eq!(first.stop_reason.as_deref(), Some("stop"));
    assert_eq!(first.category, RecordCategory::PrimaryAgent);
    assert_eq!(first.usage.input, 1000);
    assert_eq!(first.usage.output, 500);
    assert_eq!(first.usage.cache_read, 200);
    assert_eq!(first.usage.reasoning, 50);
    assert_eq!(first.usage.total, 1700);
    assert_eq!(
        first.session_file.as_deref(),
        Some(session_file.to_string_lossy().as_ref())
    );
    let cost = first.cost.expect("model-a is priced");
    assert!((cost.total_usd - 0.00202).abs() < 1e-12, "{cost:?}");
    assert!(cost.estimated);
    assert_eq!(cost.pricing_version, 1);

    // The second call inherits provider/model from the preceding model_change.
    let second = &records[1];
    assert_eq!(second.message_id.as_deref(), Some("a2"));
    assert_eq!(second.profile_id, "profile-a");
    assert_eq!(second.provider_id.as_deref(), Some("warpi-profile-a"));
    assert_eq!(second.model_id, "model-fallback");
    assert_eq!(second.stop_reason.as_deref(), Some("toolUse"));
    assert_eq!(second.usage.total, 310);
    let cost = second.cost.expect("model-fallback is priced");
    assert!((cost.total_usd - 0.000205).abs() < 1e-12, "{cost:?}");

    // A provider without the warpi- prefix becomes its own profile, and an
    // unpriced model keeps `cost: None` instead of a false zero.
    let third = &records[2];
    assert_eq!(third.profile_id, "other-provider");
    assert_eq!(third.provider_id.as_deref(), Some("other-provider"));
    assert_eq!(third.model_id, "model-b");
    assert!(third.cost.is_none());
}

#[test]
fn session_parser_skips_zero_usage_errors_and_unknown_lines() {
    let dir = TempDir::new().unwrap();
    let contents = format!(
        "{SESSION_FIXTURE}{}\n{}\n",
        r#"{"type":"thinking_level_change","id":"t1","parentId":"m1","timestamp":"2026-09-23T16:06:29.305Z","thinkingLevel":"off"}"#,
        "this is not json"
    );
    let records = parse_session_jsonl(&dir.path().join("s.jsonl"), &contents, "conv-1", &pricing());
    assert_eq!(
        records.len(),
        3,
        "zero-usage error and junk lines are skipped"
    );
}

fn backfill_setup() -> (TempDir, TempDir, PathBuf, BTreeMap<String, String>) {
    let ledger_dir = TempDir::new().unwrap();
    let sessions = TempDir::new().unwrap();
    let session_file = sessions.path().join("session-1.jsonl");
    fs::write(&session_file, SESSION_FIXTURE).unwrap();
    let conversation_map = BTreeMap::from([(
        "conv-1".to_string(),
        session_file.to_string_lossy().into_owned(),
    )]);
    (ledger_dir, sessions, session_file, conversation_map)
}

#[test]
fn backfill_is_idempotent_when_the_same_file_is_fed_twice() {
    let (ledger_dir, sessions, session_file, conversation_map) = backfill_setup();
    let ledger = ledger(ledger_dir.path());
    let first = ledger
        .backfill_from_sessions(sessions.path(), &conversation_map, &pricing())
        .unwrap();
    assert_eq!(first.files_scanned, 1);
    assert_eq!(first.records_imported, 3);
    assert_eq!(ledger.read_records().unwrap().len(), 3);

    let second = ledger
        .backfill_from_sessions(sessions.path(), &conversation_map, &pricing())
        .unwrap();
    assert_eq!(second.records_imported, 0);
    assert_eq!(second.files_skipped, 1);
    assert_eq!(ledger.read_records().unwrap().len(), 3);

    // Without the size+mtime marker, the record key still dedupes.
    fs::remove_file(ledger_dir.path().join("backfill.json")).unwrap();
    let third = ledger
        .backfill_from_sessions(sessions.path(), &conversation_map, &pricing())
        .unwrap();
    assert_eq!(third.records_imported, 0);
    assert_eq!(third.records_skipped, 3);
    assert_eq!(ledger.read_records().unwrap().len(), 3);

    // The imports are marked as backfill and still point at their source file.
    let records = ledger.read_records().unwrap();
    assert!(
        records
            .iter()
            .all(|record| record.source == RecordSource::Backfill)
    );
    assert!(records.iter().all(|record| {
        record.session_file.as_deref() == Some(session_file.to_string_lossy().as_ref())
    }));
}

#[test]
fn backfill_imports_only_messages_appended_since_the_last_run() {
    let (ledger_dir, sessions, session_file, conversation_map) = backfill_setup();
    let ledger = ledger(ledger_dir.path());
    ledger
        .backfill_from_sessions(sessions.path(), &conversation_map, &pricing())
        .unwrap();

    let appended = r#"{"type":"message","id":"a5","parentId":"a4","timestamp":"2026-09-23T16:07:00.000Z","message":{"role":"assistant","model":"model-a","provider":"warpi-profile-a","stopReason":"stop","usage":{"input":4,"output":2,"cacheRead":0,"cacheWrite":0,"reasoning":0,"totalTokens":6},"content":[]}}"#;
    let mut contents = fs::read_to_string(&session_file).unwrap();
    contents.push_str(appended);
    contents.push('\n');
    fs::write(&session_file, contents).unwrap();

    let report = ledger
        .backfill_from_sessions(sessions.path(), &conversation_map, &pricing())
        .unwrap();
    assert_eq!(report.records_imported, 1);
    assert_eq!(report.records_skipped, 3);
    assert_eq!(ledger.read_records().unwrap().len(), 4);
}

#[test]
fn backfill_ignores_session_files_without_a_conversation() {
    let (ledger_dir, sessions, _session_file, _conversation_map) = backfill_setup();
    let report = ledger(ledger_dir.path())
        .backfill_from_sessions(sessions.path(), &BTreeMap::new(), &pricing())
        .unwrap();
    assert_eq!(report.files_unmapped, 1);
    assert_eq!(report.records_imported, 0);
    assert!(ledger(ledger_dir.path()).read_records().unwrap().is_empty());
}

#[test]
fn backfill_missing_session_dir_is_an_empty_run() {
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("no-sessions");
    let report = ledger(dir.path())
        .backfill_from_sessions(&missing, &BTreeMap::new(), &pricing())
        .unwrap();
    assert_eq!(report, Default::default());
}

fn sample_records() -> Vec<UsageRecord> {
    let mut first = keyed_record(
        "conv-a",
        "profile-p",
        "model-m1",
        "2026-09-23T10:00:00.000Z",
        RecordCategory::PrimaryAgent,
        RecordSource::Live,
    );
    first.usage = UsageCounters {
        input: 100,
        output: 50,
        cache_read: 10,
        cache_write: 5,
        reasoning: 7,
        total: 0,
    };
    first.cost = Some(CostEstimate::new(0.01));

    let mut second = keyed_record(
        "conv-a",
        "profile-p",
        "model-m1",
        "2026-09-23T11:00:00.000Z",
        RecordCategory::PrimaryAgent,
        RecordSource::Live,
    );
    second.usage = UsageCounters {
        input: 200,
        output: 100,
        cache_read: 20,
        cache_write: 10,
        reasoning: 3,
        total: 0,
    };

    let mut compaction = keyed_record(
        "conv-a",
        "profile-p",
        "model-m1",
        "2026-09-24T09:00:00.000Z",
        RecordCategory::Compaction,
        RecordSource::Live,
    );
    compaction.usage = UsageCounters {
        input: 10,
        output: 5,
        cache_read: 1,
        ..Default::default()
    };
    compaction.cost = Some(CostEstimate::new(0.001));

    let mut third = keyed_record(
        "conv-b",
        "profile-q",
        "model-m2",
        "2026-09-24T12:00:00.000Z",
        RecordCategory::PrimaryAgent,
        RecordSource::Backfill,
    );
    third.usage = UsageCounters {
        input: 7,
        output: 3,
        ..Default::default()
    };

    vec![first, second, compaction, third]
}

#[test]
fn summarize_rolls_up_by_conversation_profile_model_and_day() {
    let records = sample_records();
    let summary = summarize(&records);

    assert_eq!(summary.overall.primary.records, 3);
    assert_eq!(summary.overall.compaction.records, 1);
    assert_eq!(summary.overall.primary.usage.input, 307);
    assert_eq!(summary.overall.primary.usage.output, 153);
    assert_eq!(summary.overall.primary.usage.cache_read, 30);
    assert_eq!(summary.overall.primary.usage.cache_write, 15);
    assert_eq!(summary.overall.primary.usage.reasoning, 10);
    assert_eq!(summary.overall.primary.usage.total, 505);
    assert_eq!(summary.overall.primary.cost_known_records, 1);
    assert_eq!(summary.overall.primary.cost_unknown_records, 2);
    assert!((summary.overall.primary.cost_usd - 0.01).abs() < 1e-12);
    assert_eq!(summary.overall.compaction.usage.input, 10);
    assert_eq!(summary.overall.compaction.usage.total, 16);
    assert_eq!(summary.overall.total().records, 4);
    assert!((summary.overall.total().cost_usd - 0.011).abs() < 1e-12);

    assert_eq!(summary.by_conversation.len(), 2);
    assert_eq!(summary.by_conversation["conv-a"].total().records, 3);
    assert_eq!(summary.by_conversation["conv-a"].compaction.usage.total, 16);
    assert_eq!(summary.by_profile["profile-q"].primary.records, 1);
    assert_eq!(summary.by_model["model-m1"].total().usage.total, 511);
    assert_eq!(summary.by_model["model-m2"].primary.usage.total, 10);
    assert_eq!(summary.by_day["2026-09-23"].total().records, 2);
    assert_eq!(summary.by_day["2026-09-24"].total().records, 2);
}

#[test]
fn summarize_filtered_honours_ids_time_bounds_and_source() {
    let records = sample_records();
    let all = || {
        let filter = UsageFilter::default();
        summarize_filtered(&records, &filter)
            .overall
            .total()
            .records
    };
    assert_eq!(all(), 4);

    let by_conversation = UsageFilter {
        conversation_id: Some("conv-b".to_string()),
        ..Default::default()
    };
    assert_eq!(
        summarize_filtered(&records, &by_conversation)
            .overall
            .total()
            .records,
        1
    );

    let by_model = UsageFilter {
        model_id: Some("model-m1".to_string()),
        ..Default::default()
    };
    assert_eq!(
        summarize_filtered(&records, &by_model)
            .overall
            .total()
            .records,
        3
    );

    let by_source = UsageFilter {
        source: Some(RecordSource::Backfill),
        ..Default::default()
    };
    assert_eq!(
        summarize_filtered(&records, &by_source)
            .overall
            .total()
            .records,
        1
    );

    let since = UsageFilter {
        since: Some("2026-09-24T00:00:00.000Z".to_string()),
        ..Default::default()
    };
    assert_eq!(
        summarize_filtered(&records, &since).overall.total().records,
        2
    );

    let until = UsageFilter {
        until: Some("2026-09-24T00:00:00.000Z".to_string()),
        ..Default::default()
    };
    assert_eq!(
        summarize_filtered(&records, &until).overall.total().records,
        2
    );
}

#[test]
fn ledger_summary_reads_written_records() {
    let dir = TempDir::new().unwrap();
    let ledger = ledger(dir.path());
    for record in sample_records() {
        ledger.append_at(&record, at(&record.ts)).unwrap();
    }
    let summary = ledger.summary(&UsageFilter::default()).unwrap();
    assert_eq!(summary.overall.total().records, 4);
    assert_eq!(summary.overall.total().usage.total, 521);

    let filter = UsageFilter {
        conversation_id: Some("conv-b".to_string()),
        ..Default::default()
    };
    let filtered = ledger.summary(&filter).unwrap();
    assert_eq!(filtered.overall.total().records, 1);
    assert_eq!(filtered.by_conversation.len(), 1);
}

#[test]
fn formats_and_parses_utc_timestamps() {
    assert_eq!(format_rfc3339_millis(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(unix_millis(UNIX_EPOCH), 0);
    assert_eq!(unix_millis(UNIX_EPOCH + Duration::from_millis(1234)), 1234);

    let value = "2026-09-23T16:20:11.123Z";
    let millis = parse_rfc3339_millis(value).expect("parses");
    assert_eq!(format_rfc3339_millis(millis), value);
    assert_eq!(utc_day_of(value).as_deref(), Some("2026-09-23"));

    let seconds_only = parse_rfc3339_millis("2026-09-23T16:20:11Z").expect("parses");
    assert_eq!(
        format_rfc3339_millis(seconds_only),
        "2026-09-23T16:20:11.000Z"
    );

    let leap_day = parse_rfc3339_millis("2024-02-29T00:00:00.000Z").expect("parses");
    assert_eq!(
        format_rfc3339_millis(leap_day + 86_400_000),
        "2024-03-01T00:00:00.000Z"
    );

    assert_eq!(parse_rfc3339_millis("2026-09-23T16:20:11.123+02:00"), None);
    assert_eq!(parse_rfc3339_millis("2026-13-23T16:20:11.123Z"), None);
    assert_eq!(parse_rfc3339_millis("not a timestamp"), None);
    assert_eq!(utc_day_of("2026-13-23T00:00:00.000Z"), None);
    assert_eq!(utc_day_of("garbage"), None);
}

#[test]
fn estimates_usd_per_million_with_optional_cache_rates() {
    let usage = UsageCounters {
        input: 1_000_000,
        output: 1_000_000,
        cache_read: 1_000_000,
        cache_write: 1_000_000,
        reasoning: 123,
        total: 0,
    };
    let explicit = ModelPricing {
        input: 1.0,
        output: 2.0,
        cache_read: Some(0.5),
        cache_write: Some(0.25),
    };
    assert_eq!(explicit.estimate(&usage), Some(3.75));

    let fallback = ModelPricing {
        input: 1.0,
        output: 2.0,
        cache_read: None,
        cache_write: None,
    };
    assert_eq!(fallback.estimate(&usage), Some(5.0));

    let empty = UsageCounters::default();
    assert_eq!(explicit.estimate(&empty), Some(0.0));
}

#[test]
fn estimate_cost_is_none_for_unknown_or_invalid_pricing() {
    let usage = UsageCounters {
        input: 1_000_000,
        output: 1_000_000,
        ..Default::default()
    };
    let table = BTreeMap::from([(
        "known".to_string(),
        ModelPricing {
            input: 1.0,
            output: 2.0,
            cache_read: None,
            cache_write: None,
        },
    )]);
    assert_eq!(
        estimate_cost(&table, "known", &usage).map(|cost| cost.total_usd),
        Some(3.0)
    );
    assert!(estimate_cost(&table, "unknown", &usage).is_none());
    assert!(estimate_cost(&BTreeMap::new(), "known", &usage).is_none());

    let invalid = ModelPricing {
        input: f64::NAN,
        output: 1.0,
        cache_read: None,
        cache_write: None,
    };
    assert!(!invalid.is_valid());
    assert_eq!(invalid.estimate(&usage), None);

    let negative = ModelPricing {
        input: 1.0,
        output: -1.0,
        cache_read: None,
        cache_write: None,
    };
    assert_eq!(negative.estimate(&usage), None);

    let infinite_cache = ModelPricing {
        input: 1.0,
        output: 1.0,
        cache_read: Some(f64::INFINITY),
        cache_write: None,
    };
    assert_eq!(infinite_cache.estimate(&usage), None);
}

#[test]
fn apply_pricing_fills_known_models_and_clears_unknown_ones() {
    let mut record = short_record("2026-09-23T16:20:11.123Z", "conv-1");
    record.model_id = "model-a".to_string();
    record.usage = UsageCounters {
        input: 1_000,
        output: 1_000,
        ..Default::default()
    };
    record.cost = Some(CostEstimate::new(999.0));
    record.apply_pricing(&pricing());
    let cost = record.cost.expect("model-a is priced");
    assert!((cost.total_usd - 0.003).abs() < 1e-12);

    record.model_id = "unpriced".to_string();
    record.apply_pricing(&pricing());
    assert!(record.cost.is_none());
}

#[test]
fn subagent_records_are_split_from_parent_and_compaction_spend() {
    let mut parent = keyed_record(
        "conv-a",
        "profile-p",
        "model-m",
        "2026-09-23T10:00:00.000Z",
        RecordCategory::PrimaryAgent,
        RecordSource::Live,
    );
    parent.usage = UsageCounters {
        input: 100,
        output: 20,
        ..Default::default()
    };
    parent.cost = Some(CostEstimate::new(0.10));

    let mut child = keyed_record(
        "conv-a",
        "profile-p",
        "model-m",
        "2026-09-23T10:01:00.000Z",
        RecordCategory::Subagent,
        RecordSource::Live,
    );
    child.usage = UsageCounters {
        input: 40,
        output: 10,
        ..Default::default()
    };
    child.cost = Some(CostEstimate::new(0.04));

    let records = vec![parent, child];
    let summary = summarize(&records);

    assert_eq!(summary.overall.primary.records, 1);
    assert_eq!(summary.overall.subagent.records, 1);
    assert_eq!(summary.overall.primary.usage.input, 100);
    assert_eq!(summary.overall.subagent.usage.input, 40);
    assert_eq!(summary.overall.total().records, 2);
    assert!((summary.overall.total().cost_usd - 0.14).abs() < 1e-12);
    // The child is a distinct bucket: parent-only filtering is unaffected.
    let parent_only = summarize_filtered(
        &records,
        &UsageFilter {
            conversation_id: Some("conv-a".to_string()),
            ..Default::default()
        },
    );
    assert_eq!(parent_only.overall.primary.records, 1);
    assert_eq!(parent_only.overall.subagent.records, 1);
    assert!(
        (parent_only.overall.primary.cost_usd - 0.10).abs() < 1e-12,
        "child cost must not be folded into the parent bucket"
    );
}
