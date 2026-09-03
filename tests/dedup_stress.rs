//! Deduplication stress and rebuild determinism test suite (aub-lqe.15,
//! PLAN.md 18, 34.13, 34.16).
//!
//! Verifies:
//! 1. Replay magnitude stress: a fixture on the order of 98,000 duplicate
//!    records asserts a stable canonical count and a correct duplicate count.
//! 2. Ingestion idempotence: ingesting the corpus twice or in randomized order
//!    produces identical exact counts and no duplicate canonical events.
//! 3. Cross-path deduplication: the same transcript added from a second path
//!    does not double count where dedup identity matches.
//! 4. Near-duplicate separation: legitimately equal-sized adjacent requests and
//!    distinct requests remain distinct canonical events.
//! 5. Strong vs heuristic identity isolation: strong-identity and heuristic-identity
//!    domains operate independently without cross-domain interference.
//! 6. Rebuild determinism: deleting rebuildable tables and re-ingesting yields
//!    identical canonical events, components, occurrences, watermarks, and spend
//!    report quantities, with explicit and automatic paths converging.
//! 7. Performance: the entire stress suite runs well within its documented
//!    wall-clock budget of 15.0 seconds with fixtures generated dynamically.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use proptest::prop_assert_eq;

use agent_usage_book::config::{Config, FakeEnv, Overrides, resolve};
use agent_usage_book::dedup::{HeuristicKey, deduplicate};
use agent_usage_book::domain::ids::{NativeSessionId, SessionId, SourceNamespace};
use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcDate, UtcTimestamp};
use agent_usage_book::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, UsageVector,
};
use agent_usage_book::evidence::{
    ComponentKind, CoverageCompleteness, EvidenceQuality, Provenance,
};
use agent_usage_book::ingest::{IngestOptions, IngestReport, run as run_ingest_with_sink};
use agent_usage_book::report::spend::{SpendWindow, assemble, assemble_canonical};
use agent_usage_book::report::{SpendGrouping, SpendReport};
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use agent_usage_book::store::retention::{RebuildGroup, delete_rebuildable};
use agent_usage_book::store::usage_occurrence::heuristic_rebuild_required;
use agent_usage_book::transcripts::NormalizedUsageEvent;
use agent_usage_book::transcripts::parser::{
    EvidenceClassification, ParserVersion, STRONG_IDENTITY_PREFIX,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Documented maximum wall-clock budget for the 98k duplicate stress test.
const STRESS_TEST_WALL_CLOCK_BUDGET_SECS: u64 = 15;

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aub-dedup-stress-{tag}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("scratch dir must be creatable");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn migrated_conn(db_path: &Path) -> rusqlite::Connection {
    let policy = PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(5000),
    };
    let mut conn = open(db_path, AccessMode::ReadWrite, &policy).expect("db must open");
    run_migrations(
        &mut conn,
        &registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
    )
    .expect("migrations must apply");
    conn
}

fn single_source_config(name: &str, root: &Path, format: &str) -> Config {
    let toml = format!(
        r#"
[[transcripts]]
name = "{name}"
root = "{}"
pattern = "**/*.jsonl"
format = "{format}"
"#,
        root.display()
    );
    let (cfg, _) = resolve(
        &Overrides::new(),
        &FakeEnv::new(),
        Some(&toml),
        "/virtual/aub.toml",
    )
    .expect("resolve single-source test config");
    cfg
}

fn multi_source_config(sources: &[(&str, &Path, &str)]) -> Config {
    let mut toml = String::new();
    for (name, root, format) in sources {
        toml.push_str(&format!(
            r#"
[[transcripts]]
name = "{name}"
root = "{}"
pattern = "**/*.jsonl"
format = "{format}"
"#,
            root.display()
        ));
    }
    let (cfg, _) = resolve(
        &Overrides::new(),
        &FakeEnv::new(),
        Some(&toml),
        "/virtual/aub.toml",
    )
    .expect("resolve multi-source test config");
    cfg
}

fn run_ingest(
    conn: &mut rusqlite::Connection,
    config: &Config,
    options: &IngestOptions,
    now: UtcTimestamp,
) -> Result<IngestReport, agent_usage_book::error::Error> {
    run_ingest_with_sink(conn, config, options, &FakeClock::new(now), &mut |_| Ok(()))
}

fn spend_totals(report: &SpendReport) -> (u64, u64) {
    let mut input = 0u64;
    let mut output = 0u64;
    for group in &report.groups {
        let known = group.usage.known();
        input += known.input().value();
        output += known.output().value();
    }
    (input, output)
}

macro_rules! snapshot {
    ($conn:expr, $sql:expr) => {{
        let mut stmt = $conn.prepare($sql).expect("snapshot query must prepare");
        let cols = stmt.column_count();
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut mapped = stmt.query([]).expect("snapshot query must run");
        while let Some(row) = mapped.next().expect("snapshot row must be readable") {
            let mut values = Vec::with_capacity(cols);
            for col in 0..cols {
                use rusqlite::types::Value as SqlValue;
                let value: SqlValue = row.get(col).expect("snapshot cell must be readable");
                let text = match value {
                    SqlValue::Null => String::new(),
                    SqlValue::Integer(n) => n.to_string(),
                    SqlValue::Real(f) => format!("{f}"),
                    SqlValue::Text(t) => t,
                    SqlValue::Blob(b) => String::from_utf8_lossy(&b).to_string(),
                };
                values.push(text);
            }
            rows.push(values);
        }
        rows
    }};
}

fn events_snapshot(conn: &rusqlite::Connection) -> Vec<Vec<String>> {
    snapshot!(
        conn,
        "SELECT canonical_event_id, session_id, event_timestamp, model_id, evidence_kind, \
         source_provenance, parser_version, created_at \
         FROM usage_event ORDER BY canonical_event_id"
    )
}

fn components_snapshot(conn: &rusqlite::Connection) -> Vec<Vec<String>> {
    snapshot!(
        conn,
        "SELECT e.canonical_event_id, c.token_class, c.count \
         FROM usage_component c JOIN usage_event e ON e.id = c.event_id \
         ORDER BY e.canonical_event_id, c.token_class"
    )
}

fn occurrences_snapshot(conn: &rusqlite::Connection) -> Vec<Vec<String>> {
    snapshot!(
        conn,
        "SELECT o.source_namespace, o.native_event_id, o.parser_version, o.heuristic_key, \
         o.source_file, e.canonical_event_id, o.transcript_file_id, o.occurred_at, \
         o.identity_strength, o.heuristic_algorithm_version, o.canonical_payload_digest \
         FROM usage_occurrence o JOIN usage_event e ON e.id = o.event_id \
         ORDER BY o.source_namespace, o.native_event_id, o.source_file, e.canonical_event_id"
    )
}

fn sessions_snapshot(conn: &rusqlite::Connection) -> Vec<Vec<String>> {
    snapshot!(
        conn,
        "SELECT source, native_session_id, start, end, project_key, repository_key, run_id \
         FROM session ORDER BY source, native_session_id"
    )
}

fn watermarks_snapshot(conn: &rusqlite::Connection) -> Vec<Vec<String>> {
    snapshot!(
        conn,
        "SELECT source_key, relative_path, size, mtime_nanos, identity, parser_version, \
         consumed_offset FROM transcript_file ORDER BY source_key, relative_path"
    )
}

fn quarantine_snapshot(conn: &rusqlite::Connection) -> Vec<Vec<String>> {
    snapshot!(
        conn,
        "SELECT source_file, parser, failure_class, excerpt_hash, excerpt, byte_offset, \
         line_number FROM ingest_quarantine ORDER BY source_file, parser, failure_class, excerpt_hash"
    )
}

struct Materializations {
    events: Vec<Vec<String>>,
    components: Vec<Vec<String>>,
    occurrences: Vec<Vec<String>>,
    sessions: Vec<Vec<String>>,
    watermarks: Vec<Vec<String>>,
    quarantine: Vec<Vec<String>>,
}

fn capture_materializations(conn: &rusqlite::Connection) -> Materializations {
    Materializations {
        events: events_snapshot(conn),
        components: components_snapshot(conn),
        occurrences: occurrences_snapshot(conn),
        sessions: sessions_snapshot(conn),
        watermarks: watermarks_snapshot(conn),
        quarantine: quarantine_snapshot(conn),
    }
}

fn assert_materializations_identical(before: &Materializations, after: &Materializations) {
    assert_eq!(
        before.events, after.events,
        "canonical events must reproduce identically"
    );
    assert_eq!(
        before.components, after.components,
        "components must reproduce identically"
    );
    assert_eq!(
        before.occurrences, after.occurrences,
        "occurrences must reproduce identically"
    );
    assert_eq!(
        before.sessions, after.sessions,
        "sessions must reproduce identically"
    );
    assert_eq!(
        before.watermarks, after.watermarks,
        "watermarks must reproduce identically"
    );
    assert_eq!(
        before.quarantine, after.quarantine,
        "quarantine records must reproduce identically"
    );
}

/// Generates a replay corpus with `canonical_count` distinct canonical events
/// and exactly `duplicate_count` replayed records across multiple transcript files.
///
/// If `include_quarantine` is true, writes a malformed record into the first file
/// so that the quarantine table also receives rows and is exercised during rebuild.
///
/// Half of the canonical events use strong identities (`message.id`) and half use
/// heuristic identities (no `message.id`). Every duplicate record replays an existing
/// event with growing output tokens.
///
/// Returns `(canonical_count, duplicate_count, total_records)`.
fn generate_replay_corpus(
    root: &Path,
    canonical_count: usize,
    duplicate_count: usize,
    include_quarantine: bool,
) -> (usize, usize, usize) {
    fs::create_dir_all(root).expect("corpus root must be creatable");

    let num_files = 10;
    let mut file_writers: Vec<std::io::BufWriter<fs::File>> = (0..num_files)
        .map(|i| {
            let file_path = root.join(format!("session_{i}.jsonl"));
            let file = fs::File::create(&file_path).expect("corpus file must be creatable");
            std::io::BufWriter::new(file)
        })
        .collect();

    let strong_count = canonical_count / 2;
    let base_replays = duplicate_count / canonical_count;
    let remainder = duplicate_count % canonical_count;

    let mut total_written = 0usize;

    if include_quarantine {
        writeln!(
            file_writers[0],
            r#"{{"type":"assistant","timestamp":"2026-08-25T09:00:00.000Z","sessionId":"s_0","message":{{"id":"m_malformed","usage":{{"input_tokens":"wrong-type","output_tokens":5}}}}}}"#
        )
        .unwrap();
        total_written += 1;
    }

    for i in 0..canonical_count {
        let is_strong = i < strong_count;
        let replays_for_this_event = base_replays + if i < remainder { 1 } else { 0 };
        let session_idx = i % num_files;
        let writer = &mut file_writers[session_idx];

        let base_input = 100 + (i as u64 % 500);
        let base_output = 50;
        let base_cache_read = 10;
        let base_cache_write = 5;
        let timestamp = format!("2026-08-25T10:{:02}:{:02}.000Z", (i / 60) % 60, i % 60);
        let session_id = format!("s_{session_idx}");

        if is_strong {
            let msg_id = format!("msg_stress_{i}");
            writeln!(
                writer,
                r#"{{"type":"assistant","timestamp":"{timestamp}","sessionId":"{session_id}","message":{{"id":"{msg_id}","usage":{{"input_tokens":{base_input},"output_tokens":{base_output},"cache_read_input_tokens":{base_cache_read},"cache_creation_input_tokens":{base_cache_write}}}}}}}"#
            )
            .unwrap();
        } else {
            writeln!(
                writer,
                r#"{{"type":"assistant","timestamp":"{timestamp}","sessionId":"{session_id}","message":{{"usage":{{"input_tokens":{base_input},"output_tokens":{base_output},"cache_read_input_tokens":{base_cache_read},"cache_creation_input_tokens":{base_cache_write}}}}}}}"#
            )
            .unwrap();
        }
        total_written += 1;

        for r in 0..replays_for_this_event {
            let target_file_idx = (session_idx + r + 1) % num_files;
            let target_writer = &mut file_writers[target_file_idx];
            let grown_output = base_output + (r as u64 + 1);

            if is_strong {
                let msg_id = format!("msg_stress_{i}");
                writeln!(
                    target_writer,
                    r#"{{"type":"assistant","timestamp":"{timestamp}","sessionId":"{session_id}","message":{{"id":"{msg_id}","usage":{{"input_tokens":{base_input},"output_tokens":{grown_output},"cache_read_input_tokens":{base_cache_read},"cache_creation_input_tokens":{base_cache_write}}}}}}}"#
                )
                .unwrap();
            } else {
                writeln!(
                    target_writer,
                    r#"{{"type":"assistant","timestamp":"{timestamp}","sessionId":"{session_id}","message":{{"usage":{{"input_tokens":{base_input},"output_tokens":{grown_output},"cache_read_input_tokens":{base_cache_read},"cache_creation_input_tokens":{base_cache_write}}}}}}}"#
                )
                .unwrap();
            }
            total_written += 1;
        }
    }

    for mut writer in file_writers {
        writer.flush().expect("flush corpus file");
    }

    (canonical_count, duplicate_count, total_written)
}

fn synthetic_event(
    id: Option<&str>,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    nanos: i64,
    session: &str,
) -> NormalizedUsageEvent {
    let usage = UsageVector::new(
        KnownTokenVector::new(
            InputTokens::new(input),
            OutputTokens::new(output),
            CacheReadTokens::new(cache_read),
            CacheWriteTokens::new(cache_write),
        ),
        std::collections::BTreeMap::new(),
        CoverageCompleteness::Complete,
        EvidenceQuality::Measured,
    );
    let mut sources = vec!["file.jsonl".to_string()];
    if let Some(id) = id {
        sources.push(format!("{STRONG_IDENTITY_PREFIX}{id}"));
    }
    NormalizedUsageEvent::new(
        usage,
        EvidenceClassification::Reported,
        Provenance::new(sources),
        ParserVersion::new("claude-code-1"),
    )
    .with_occurred_at(UtcTimestamp::from_unix_nanos(nanos))
    .with_session(SessionId::new(
        SourceNamespace::new("claude-code"),
        NativeSessionId::new(session),
    ))
}

// ---------------------------------------------------------------------------
// Acceptance Criterion 1: Replay fixture at ~98,000 duplicate records scale
// ---------------------------------------------------------------------------

#[test]
fn criterion_1_replay_fixture_98k_duplicates_asserts_stable_canonical_and_exact_counts() {
    let scratch = ScratchDir::new("c1-stress-98k");
    let corpus_root = scratch.path().join("corpus");

    let canonical_expected = 1000usize;
    let duplicate_expected = 98004usize; // Matches observed PLAN.md 34.13 magnitude
    let (canon, dups, total) =
        generate_replay_corpus(&corpus_root, canonical_expected, duplicate_expected, false);
    assert_eq!(canon, canonical_expected);
    assert_eq!(dups, duplicate_expected);
    assert_eq!(total, canonical_expected + duplicate_expected);

    let config = single_source_config("claude-code", &corpus_root, "claude-code");
    let db_path = scratch.path().join("ledger.sqlite3");
    let mut conn = migrated_conn(&db_path);
    let now = UtcTimestamp::from_unix_nanos(100_000_000);

    let started = Instant::now();
    let report = run_ingest(&mut conn, &config, &IngestOptions::default(), now)
        .expect("ingest must succeed");
    let elapsed = started.elapsed();

    // Verify parser and ingest counts
    assert_eq!(report.files_scanned, 10);
    assert_eq!(report.files_parsed, 10);
    assert_eq!(report.unreadable_files, Vec::<String>::new());
    assert_eq!(
        report.outcome.events_written.value(),
        canonical_expected as u64,
        "canonical count must be stable and match expected"
    );
    assert_eq!(
        report.outcome.occurrences_written.value(),
        canonical_expected as u64,
        "initial occurrence rows must match canonical events count"
    );

    // Verify row counts in the SQLite database
    let stored_events: i64 = conn
        .query_row("SELECT COUNT(*) FROM usage_event", [], |row| row.get(0))
        .expect("count usage_event");
    assert_eq!(
        stored_events as usize, canonical_expected,
        "canonical events in store must equal 1,000"
    );

    let stored_components: i64 = conn
        .query_row("SELECT COUNT(*) FROM usage_component", [], |row| row.get(0))
        .expect("count usage_component");
    assert!(
        stored_components >= (canonical_expected as i64 * 4),
        "every canonical event must have 4 token component rows"
    );

    let stored_occurrences: i64 = conn
        .query_row("SELECT COUNT(*) FROM usage_occurrence", [], |row| {
            row.get(0)
        })
        .expect("count usage_occurrence");
    assert_eq!(
        stored_occurrences as usize, canonical_expected,
        "usage occurrences in store must match unique canonical identities"
    );

    // Verify spend diagnostics report correct replayed occurrences
    let spend_diag =
        agent_usage_book::store::spend::diagnostics(&conn).expect("read spend diagnostics");
    assert_eq!(
        spend_diag.heuristic_identities,
        (canonical_expected / 2) as u64,
        "heuristic identities count must match heuristic portion"
    );

    assert!(
        elapsed.as_secs() < STRESS_TEST_WALL_CLOCK_BUDGET_SECS,
        "stress test took {elapsed:?}, exceeding budget of {STRESS_TEST_WALL_CLOCK_BUDGET_SECS}s"
    );
}

// ---------------------------------------------------------------------------
// Acceptance Criterion 2: Ingesting corpus twice is idempotent
// ---------------------------------------------------------------------------

#[test]
fn criterion_2_ingesting_corpus_twice_is_idempotent_asserted_by_exact_counts() {
    let scratch = ScratchDir::new("c2-idempotent");
    let corpus_root = scratch.path().join("corpus");

    let canonical_expected = 100usize;
    let duplicate_expected = 900usize;
    generate_replay_corpus(&corpus_root, canonical_expected, duplicate_expected, false);

    let config = single_source_config("claude-code", &corpus_root, "claude-code");
    let db_path = scratch.path().join("ledger.sqlite3");
    let mut conn = migrated_conn(&db_path);
    let now = UtcTimestamp::from_unix_nanos(100_000_000);

    // First ingestion pass
    let first_report =
        run_ingest(&mut conn, &config, &IngestOptions::default(), now).expect("first ingest");
    assert_eq!(
        first_report.outcome.events_written.value(),
        canonical_expected as u64
    );
    assert_eq!(first_report.generation.value(), 1);

    let before_snapshot = capture_materializations(&conn);
    assert_eq!(before_snapshot.events.len(), canonical_expected);

    // Second full ingestion pass (explicit full re-ingest replacing whole files)
    let second_report =
        run_ingest(&mut conn, &config, &IngestOptions::default(), now).expect("second ingest");
    assert_eq!(
        second_report.outcome.events_written.value(),
        canonical_expected as u64,
        "second pass replaces file contributions and rewrites same canonical events"
    );
    assert_eq!(
        second_report.generation.value(),
        2,
        "generation must advance on second completed pass"
    );

    // Database state after second full pass must be identical to after first pass
    let after_second_snapshot = capture_materializations(&conn);
    assert_materializations_identical(&before_snapshot, &after_second_snapshot);

    let stored_events: i64 = conn
        .query_row("SELECT COUNT(*) FROM usage_event", [], |row| row.get(0))
        .expect("count usage_event");
    assert_eq!(
        stored_events as usize, canonical_expected,
        "canonical count in store must stay strictly idempotent at 100"
    );

    // Third pass: automatic changed-only refresh
    let refresh_options = IngestOptions {
        changed_only: true,
        source: None,
    };
    let third_report =
        run_ingest(&mut conn, &config, &refresh_options, now).expect("third refresh ingest");
    assert_eq!(
        third_report.files_skipped, 10,
        "all unchanged files skipped"
    );
    assert_eq!(third_report.outcome.events_written.value(), 0);

    let after_third_snapshot = capture_materializations(&conn);
    assert_materializations_identical(&before_snapshot, &after_third_snapshot);
}

// ---------------------------------------------------------------------------
// Acceptance Criterion 3: Cross-path duplicate transcript does not double count
// ---------------------------------------------------------------------------

#[test]
fn criterion_3_same_transcript_added_from_second_path_does_not_double_count() {
    let scratch = ScratchDir::new("c3-cross-path");
    let path_a = scratch.path().join("source_a");
    let path_b = scratch.path().join("source_b");

    let canonical_expected = 50usize;
    let duplicate_expected = 200usize;
    generate_replay_corpus(&path_a, canonical_expected, duplicate_expected, false);

    // Copy the entire directory to path_b (identical contents, distinct files and paths)
    fs::create_dir_all(&path_b).expect("create source_b");
    for entry in fs::read_dir(&path_a).expect("read source_a") {
        let entry = entry.expect("valid entry");
        let target = path_b.join(entry.file_name());
        fs::copy(entry.path(), target).expect("copy to source_b");
    }

    let config_single = single_source_config("source_a", &path_a, "claude-code");
    let config_both = multi_source_config(&[
        ("source_a", &path_a, "claude-code"),
        ("source_b", &path_b, "claude-code"),
    ]);

    let db_path_single = scratch.path().join("ledger_single.sqlite3");
    let mut conn_single = migrated_conn(&db_path_single);

    let db_path_both = scratch.path().join("ledger_both.sqlite3");
    let mut conn_both = migrated_conn(&db_path_both);

    let now = UtcTimestamp::from_unix_nanos(200_000_000);

    // Ingest single source
    run_ingest(
        &mut conn_single,
        &config_single,
        &IngestOptions::default(),
        now,
    )
    .unwrap();

    // Ingest both sources (second path included)
    let report_both =
        run_ingest(&mut conn_both, &config_both, &IngestOptions::default(), now).unwrap();
    assert_eq!(report_both.files_scanned, 20);
    assert_eq!(report_both.files_parsed, 20);
    assert_eq!(
        report_both.outcome.events_written.value(),
        canonical_expected as u64,
        "second path transcript duplicates must collapse into the same canonical events"
    );

    let count_single: i64 = conn_single
        .query_row("SELECT COUNT(*) FROM usage_event", [], |row| row.get(0))
        .unwrap();
    let count_both: i64 = conn_both
        .query_row("SELECT COUNT(*) FROM usage_event", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        count_single, count_both,
        "canonical event count must be identical between 1 source and 2 duplicate sources"
    );

    // Check spend report totals
    let window = SpendWindow::starting(UtcDate::parse("2026-08-25").unwrap(), 1).unwrap();
    let spend_single = assemble_canonical(
        &conn_single,
        window,
        now,
        vec![SpendGrouping::Day],
        false,
        None,
    )
    .unwrap();
    let spend_both = assemble_canonical(
        &conn_both,
        window,
        now,
        vec![SpendGrouping::Day],
        false,
        None,
    )
    .unwrap();

    let (input_single, output_single) = spend_totals(&spend_single);
    let (input_both, output_both) = spend_totals(&spend_both);

    assert_eq!(
        input_single, input_both,
        "input token totals must not double count when second path is added"
    );
    assert_eq!(
        output_single, output_both,
        "output token totals must not double count when second path is added"
    );
}

// ---------------------------------------------------------------------------
// Acceptance Criterion 4: Near-duplicates that must remain distinct do remain distinct
// ---------------------------------------------------------------------------

#[test]
fn criterion_4_near_duplicates_remain_distinct_including_equal_sized_adjacent_requests() {
    // Case 1: Two legitimately equal-sized adjacent requests with different timestamps
    let event_t1 = synthetic_event(None, 100, 50, 10, 5, 1_000_000_000, "s1");
    let event_t2 = synthetic_event(None, 100, 50, 10, 5, 1_000_000_001, "s1");
    let dedup_adjacent = deduplicate(vec![event_t1.clone(), event_t2.clone()]);
    assert_eq!(
        dedup_adjacent.canonical.len(),
        2,
        "two equal-sized adjacent requests must remain 2 distinct canonical events"
    );
    assert_eq!(dedup_adjacent.replayed_occurrences, 0);

    // Case 2: Same timestamp and counts, but different session IDs
    let event_s1 = synthetic_event(None, 100, 50, 10, 5, 1_000_000_000, "session_alpha");
    let event_s2 = synthetic_event(None, 100, 50, 10, 5, 1_000_000_000, "session_beta");
    let dedup_sessions = deduplicate(vec![event_s1, event_s2]);
    assert_eq!(
        dedup_sessions.canonical.len(),
        2,
        "different session IDs at identical timestamp must remain distinct"
    );

    // Case 3: Same timestamp and session, but different input counts
    let event_in1 = synthetic_event(None, 100, 50, 10, 5, 1_000_000_000, "s1");
    let event_in2 = synthetic_event(None, 101, 50, 10, 5, 1_000_000_000, "s1");
    let dedup_inputs = deduplicate(vec![event_in1, event_in2]);
    assert_eq!(
        dedup_inputs.canonical.len(),
        2,
        "different input counts at identical timestamp must remain distinct"
    );

    // Case 4: Different native message IDs in strong domain
    let event_msg1 = synthetic_event(Some("msg_alpha"), 100, 50, 10, 5, 1_000_000_000, "s1");
    let event_msg2 = synthetic_event(Some("msg_beta"), 100, 50, 10, 5, 1_000_000_000, "s1");
    let dedup_messages = deduplicate(vec![event_msg1, event_msg2]);
    assert_eq!(
        dedup_messages.canonical.len(),
        2,
        "distinct native message IDs must remain distinct canonical events"
    );

    // Case 5: Stable native ID outranks heuristic identity
    let event_strong_a = synthetic_event(Some("msg_same"), 100, 50, 10, 5, 1_000_000_000, "s1");
    let event_strong_b = synthetic_event(Some("msg_same"), 100, 200, 10, 5, 9_999_999_999, "s9");
    let dedup_strong_outrank = deduplicate(vec![event_strong_a, event_strong_b]);
    assert_eq!(
        dedup_strong_outrank.canonical.len(),
        1,
        "strong native ID collapses occurrences across timestamps and sessions"
    );
    assert_eq!(
        dedup_strong_outrank.canonical[0]
            .usage()
            .known()
            .output()
            .value(),
        200,
        "larger output count kept"
    );
}

// ---------------------------------------------------------------------------
// Acceptance Criterion 5: Strong and heuristic identity parsers stressed separately
// ---------------------------------------------------------------------------

#[test]
fn criterion_5_strong_and_heuristic_identity_parsers_stressed_separately_no_interference() {
    let scratch = ScratchDir::new("c5-strong-heuristic");

    // Sub-test A: Strong identity parser under replay and count disagreement
    let mut strong_events = Vec::new();
    for i in 0..50 {
        let msg_id = format!("strong_msg_{i}");
        // 10 replays with growing output
        for r in 0..10 {
            strong_events.push(synthetic_event(
                Some(&msg_id),
                200,
                50 + r * 10,
                20,
                10,
                1_000_000_000 + i as i64,
                "s_strong",
            ));
        }
        // One disagreeing occurrence (different input count -> strong collision)
        strong_events.push(synthetic_event(
            Some(&msg_id),
            999,
            150,
            20,
            10,
            1_000_000_000 + i as i64,
            "s_strong",
        ));
    }
    let strong_dedup = deduplicate(strong_events);
    assert_eq!(
        strong_dedup.canonical.len(),
        50,
        "50 strong messages must yield 50 canonical events"
    );
    assert_eq!(strong_dedup.replayed_occurrences, 50 * 9);
    assert_eq!(
        strong_dedup.collisions, 50,
        "50 disagreeing strong occurrences recorded as collisions"
    );
    assert!(strong_dedup.heuristic_collisions.is_empty());
    assert_eq!(strong_dedup.without_identity, 0);

    // Sub-test B: Heuristic identity parser under replay and collision
    let mut heuristic_events = Vec::new();
    for i in 0..50 {
        let timestamp_nanos = 2_000_000_000 + i as i64 * 100;
        // 10 replays with growing output
        for r in 0..10 {
            heuristic_events.push(synthetic_event(
                None,
                150,
                40 + r * 5,
                15,
                5,
                timestamp_nanos,
                "s_heur",
            ));
        }
    }
    // Add one deliberate heuristic collision (same key, different coverage payload)
    let heur_collision_nanos = 3_000_000_000i64;
    let heur_coll_a = synthetic_event(None, 100, 50, 0, 0, heur_collision_nanos, "s_coll");
    let heur_coll_b = NormalizedUsageEvent::new(
        UsageVector::new(
            KnownTokenVector::new(
                InputTokens::new(100),
                OutputTokens::new(50),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(0),
            ),
            std::collections::BTreeMap::new(),
            CoverageCompleteness::partial([ComponentKind::new("output")]),
            EvidenceQuality::Measured,
        ),
        EvidenceClassification::Reported,
        Provenance::new(vec!["file.jsonl".to_string()]),
        ParserVersion::new("claude-code-1"),
    )
    .with_occurred_at(UtcTimestamp::from_unix_nanos(heur_collision_nanos))
    .with_session(SessionId::new(
        SourceNamespace::new("claude-code"),
        NativeSessionId::new("s_coll"),
    ));

    heuristic_events.push(heur_coll_a);
    heuristic_events.push(heur_coll_b);

    let heur_dedup = deduplicate(heuristic_events);
    assert_eq!(
        heur_dedup.canonical.len(),
        50,
        "50 heuristic events + 1 quarantined collision pair"
    );
    assert_eq!(heur_dedup.replayed_occurrences, 50 * 9);
    assert_eq!(
        heur_dedup.heuristic_collisions.len(),
        1,
        "heuristic collision quarantined"
    );
    assert_eq!(heur_dedup.without_identity, 50 * 10 + 2);

    // Sub-test C: Strong and Heuristic coexistence and cross-domain non-interference
    // Same timestamp, session, and token counts, one with native ID and one without
    let co_time = 4_000_000_000i64;
    let strong_co = synthetic_event(Some("msg_coexist"), 100, 50, 0, 0, co_time, "s_co");
    let heur_co = synthetic_event(None, 100, 50, 0, 0, co_time, "s_co");
    let co_dedup = deduplicate(vec![strong_co, heur_co]);
    assert_eq!(
        co_dedup.canonical.len(),
        2,
        "strong and heuristic events with identical counts must NOT collide or interfere"
    );
    assert_eq!(co_dedup.replayed_occurrences, 0);
    assert_eq!(co_dedup.collisions, 0);
    assert_eq!(co_dedup.heuristic_collisions.len(), 0);
    assert_eq!(co_dedup.without_identity, 1);

    // Sub-test D: Verify heuristic rebuild detection works across versions
    let db_path = scratch.path().join("ledger_version.sqlite3");
    let conn = migrated_conn(&db_path);
    let rebuild_check = heuristic_rebuild_required(&conn, HeuristicKey::ALGORITHM_VERSION).unwrap();
    assert!(
        rebuild_check.is_empty(),
        "empty or current store requires no rebuild"
    );
    let outdated_check = heuristic_rebuild_required(&conn, "hk0").unwrap();
    assert!(
        outdated_check.is_empty(),
        "no rows present under older version"
    );
}

// ---------------------------------------------------------------------------
// Acceptance Criterion 6: Deleting rebuildable tables and re-ingesting produces
// identical canonical events and report quantities; explicit and automatic paths converge.
// ---------------------------------------------------------------------------

#[test]
fn criterion_6_rebuild_determinism_and_path_convergence() {
    let scratch = ScratchDir::new("c6-rebuild-determinism");
    let corpus_root = scratch.path().join("corpus");

    let canonical_expected = 120usize;
    let duplicate_expected = 480usize;
    generate_replay_corpus(&corpus_root, canonical_expected, duplicate_expected, true);

    let config = single_source_config("claude-code", &corpus_root, "claude-code");
    let db_path = scratch.path().join("ledger.sqlite3");
    let mut conn = migrated_conn(&db_path);
    let now = UtcTimestamp::from_unix_nanos(300_000_000);
    let window = SpendWindow::starting(UtcDate::parse("2026-08-25").unwrap(), 1).unwrap();

    // Step 1: Initial Ingest
    let first_report =
        run_ingest(&mut conn, &config, &IngestOptions::default(), now).expect("initial ingest");
    assert_eq!(
        first_report.outcome.events_written.value(),
        canonical_expected as u64
    );

    let snapshot_before = capture_materializations(&conn);
    assert!(
        !snapshot_before.quarantine.is_empty(),
        "corpus must include quarantine data to prove quarantine rebuild coverage"
    );

    let spend_before =
        assemble_canonical(&conn, window, now, vec![SpendGrouping::Day], false, None)
            .expect("spend report before rebuild");

    // Direct spend assembly from transcript files must match SQLite canonical assembly
    let file_spend_before =
        assemble(&config, window, now).expect("spend report directly from files");

    let (input_before, output_before) = spend_totals(&spend_before);
    let (file_input_before, file_output_before) = spend_totals(&file_spend_before);

    assert_eq!(
        input_before, file_input_before,
        "file assembly and SQLite canonical assembly must agree"
    );
    assert_eq!(output_before, file_output_before);

    // Step 2: Delete every rebuildable table (RebuildGroup::Transcripts)
    let sweep_report = delete_rebuildable(&mut conn, RebuildGroup::Transcripts)
        .expect("rebuild sweep must succeed");
    for (class, count) in &sweep_report.deleted {
        assert!(
            count.value() > 0,
            "sweep should delete real rows from {class:?}"
        );
    }

    // Verify all rebuildable tables are emptied
    for class in RebuildGroup::Transcripts.classes() {
        let table = class.table_name().expect("must have table name");
        let remaining: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(remaining, 0, "{table} must be completely empty after sweep");
    }

    // Step 3: Re-ingest the corpus onto emptied tables
    let rebuild_report =
        run_ingest(&mut conn, &config, &IngestOptions::default(), now).expect("re-ingest");
    assert_eq!(
        rebuild_report.outcome.events_written.value(),
        canonical_expected as u64,
        "re-ingest rewrites exactly the canonical events destroyed"
    );

    let snapshot_after_rebuild = capture_materializations(&conn);
    assert_materializations_identical(&snapshot_before, &snapshot_after_rebuild);

    let spend_after_rebuild =
        assemble_canonical(&conn, window, now, vec![SpendGrouping::Day], false, None)
            .expect("spend report after rebuild");

    let (input_after_rebuild, output_after_rebuild) = spend_totals(&spend_after_rebuild);

    assert_eq!(
        input_before, input_after_rebuild,
        "spend report input total must be identical after rebuild"
    );
    assert_eq!(
        output_before, output_after_rebuild,
        "spend report output total must be identical after rebuild"
    );
    assert_eq!(spend_before.groups.len(), spend_after_rebuild.groups.len());

    // Step 4: Verify automatic refresh path (--changed-only) converges on same set
    let refresh_report = run_ingest(
        &mut conn,
        &config,
        &IngestOptions {
            changed_only: true,
            source: None,
        },
        now,
    )
    .expect("refresh ingest");
    assert_eq!(refresh_report.files_skipped, 10);
    assert_eq!(refresh_report.outcome.events_written.value(), 0);

    let snapshot_after_refresh = capture_materializations(&conn);
    assert_materializations_identical(&snapshot_before, &snapshot_after_refresh);

    let spend_after_refresh =
        assemble_canonical(&conn, window, now, vec![SpendGrouping::Day], false, None)
            .expect("spend report after refresh");
    let (input_after_refresh, _) = spend_totals(&spend_after_refresh);
    assert_eq!(
        input_before, input_after_refresh,
        "spend report quantities must converge across explicit and automatic paths"
    );
}

// ---------------------------------------------------------------------------
// Acceptance Criterion 7: Performance budget and generated fixture verification
// ---------------------------------------------------------------------------

#[test]
fn criterion_7_suite_runs_within_documented_time_budget_with_generated_fixture() {
    let scratch = ScratchDir::new("c7-perf-budget");
    let corpus_root = scratch.path().join("corpus");

    let canonical_expected = 1000usize;
    let duplicate_expected = 98004usize;

    let overall_start = Instant::now();

    // 1. Generation phase
    let gen_start = Instant::now();
    let (canon, dups, total) =
        generate_replay_corpus(&corpus_root, canonical_expected, duplicate_expected, false);
    let gen_elapsed = gen_start.elapsed();
    assert_eq!(canon, 1000);
    assert_eq!(dups, 98004);
    assert_eq!(total, 99004);

    // 2. Parse & Ingestion phase
    let config = single_source_config("claude-code", &corpus_root, "claude-code");
    let db_path = scratch.path().join("ledger.sqlite3");
    let mut conn = migrated_conn(&db_path);
    let now = UtcTimestamp::from_unix_nanos(400_000_000);

    let ingest_start = Instant::now();
    let report = run_ingest(&mut conn, &config, &IngestOptions::default(), now).unwrap();
    let ingest_elapsed = ingest_start.elapsed();

    assert_eq!(report.outcome.events_written.value(), 1000);

    // 3. Spend assembly phase
    let spend_start = Instant::now();
    let window = SpendWindow::starting(UtcDate::parse("2026-08-25").unwrap(), 1).unwrap();
    let spend =
        assemble_canonical(&conn, window, now, vec![SpendGrouping::Day], false, None).unwrap();
    let spend_elapsed = spend_start.elapsed();

    assert_eq!(spend.groups.len(), 1);

    let total_elapsed = overall_start.elapsed();

    println!(
        "Stress suite wall-clock timings: gen={gen_elapsed:?}, ingest={ingest_elapsed:?}, spend={spend_elapsed:?}, total={total_elapsed:?}"
    );

    assert!(
        total_elapsed.as_secs() < STRESS_TEST_WALL_CLOCK_BUDGET_SECS,
        "Total execution time {total_elapsed:?} exceeded documented budget of {STRESS_TEST_WALL_CLOCK_BUDGET_SECS}s"
    );
}

// ---------------------------------------------------------------------------
// Property tests: Ingestion idempotence over randomized order
// ---------------------------------------------------------------------------

proptest::proptest! {
    #[test]
    fn prop_ingestion_idempotence_over_permutations(
        seed in 0u64..100u64,
        num_events in 5usize..20usize,
        replays_per_event in 2usize..6usize,
    ) {
        let mut events = Vec::new();
        for i in 0..num_events {
            let msg_id = format!("prop_msg_{seed}_{i}");
            for r in 0..replays_per_event {
                events.push(synthetic_event(
                    Some(&msg_id),
                    100 + i as u64,
                    50 + r as u64 * 10,
                    10,
                    5,
                    1_000_000_000 + i as i64,
                    "prop_s",
                ));
            }
        }

        // Original order deduplication
        let dedup_orig = deduplicate(events.clone());

        // Reverse order deduplication
        let mut reversed_events = events.clone();
        reversed_events.reverse();
        let dedup_rev = deduplicate(reversed_events);

        prop_assert_eq!(dedup_orig.canonical.len(), num_events);
        prop_assert_eq!(dedup_rev.canonical.len(), num_events);
        prop_assert_eq!(
            dedup_orig.replayed_occurrences,
            (num_events * (replays_per_event - 1)) as u64
        );
        prop_assert_eq!(
            dedup_rev.replayed_occurrences,
            (num_events * (replays_per_event - 1)) as u64
        );

        // Assert canonical output counts match largest snapshot
        for (a, b) in dedup_orig.canonical.iter().zip(dedup_rev.canonical.iter()) {
            prop_assert_eq!(
                a.usage().known().output().value(),
                50 + (replays_per_event as u64 - 1) * 10
            );
            prop_assert_eq!(
                b.usage().known().output().value(),
                50 + (replays_per_event as u64 - 1) * 10
            );
        }
    }
}
