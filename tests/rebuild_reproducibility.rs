//! Rebuild determinism: `rebuild transcripts` followed by re-ingest reproduces
//! identical canonical normalized events and rebuildable materializations for a
//! fixed corpus (`aub-lqe.11`, PLAN.md 34.16, invariant 2 of docs/INVARIANTS.md).
//!
//! Transcripts stay authoritative on disk, so the store's transcript-derived
//! tables are a cache. This is the proof: destroy every table the rebuild
//! group sweeps, ingest the same corpus again, and the store holds the same
//! events, components, occurrences, sessions, watermarks and quarantine rows
//! as before the sweep, with the irreplaceable tables untouched throughout.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agent_usage_book::config::{FakeEnv, Overrides, resolve};
use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::ingest::IngestOptions;
use agent_usage_book::ingest::run as run_ingest_with_sink;

/// Runs one ingest pass under a fixture clock with a sink that asserts nothing,
/// so every call site below names the behaviour instead of the new plumbing.
fn run_ingest(
    conn: &mut rusqlite::Connection,
    config: &agent_usage_book::config::Config,
    options: &IngestOptions,
    now: UtcTimestamp,
) -> Result<agent_usage_book::ingest::IngestReport, agent_usage_book::error::Error> {
    run_ingest_with_sink(
        conn,
        config,
        options,
        &FakeClock::new(now),
        &mut |_| Ok(()),
        &mut |_| Ok(()),
    )
}
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use agent_usage_book::store::retention::{RebuildGroup, delete_rebuildable};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Self {
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aub-rebuild-reproducibility-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("scratch dir must be creatable");
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

/// The corpus: two sessions across two files, with a replayed message across
/// files, a replayed line inside one file, and one malformed record. Every
/// rebuildable materialization the transcripts group sweeps gets a row from
/// this corpus, so the reproduction claim covers them all.
fn write_corpus(root: &Path) {
    let claude = root.join("claude-code");
    fs::create_dir_all(&claude).expect("corpus dir must be creatable");

    fs::write(
        claude.join("session1.jsonl"),
        concat!(
            r#"{"type":"assistant","timestamp":"2026-08-25T10:00:00.000Z","sessionId":"s1","message":{"id":"m1","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":20,"cache_creation_input_tokens":10}}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-08-25T10:05:00.000Z","sessionId":"s1","message":{"id":"m2","usage":{"input_tokens":30,"output_tokens":12,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
            "\n",
            // A replayed line of m1 with grown output: deduplication collapses
            // it onto the canonical event, and the component merge keeps the
            // larger count.
            r#"{"type":"assistant","timestamp":"2026-08-25T10:00:00.000Z","sessionId":"s1","message":{"id":"m1","usage":{"input_tokens":100,"output_tokens":60,"cache_read_input_tokens":20,"cache_creation_input_tokens":10}}}"#,
            "\n",
            // A malformed record: quarantined, and quarantined again after the
            // rebuild, which is why the quarantine table is inside the group.
            r#"{"type":"assistant","timestamp":"2026-08-25T10:06:00.000Z","sessionId":"s1","message":{"id":"m9","usage":{"input_tokens":"wrong-type","output_tokens":5}}}"#,
            "\n",
        ),
    )
    .expect("corpus file must be writable");

    fs::write(
        claude.join("session2.jsonl"),
        concat!(
            // m1 replayed from the second file: one canonical event, two files
            // of occurrence evidence.
            r#"{"type":"assistant","timestamp":"2026-08-25T10:00:00.000Z","sessionId":"s1","message":{"id":"m1","usage":{"input_tokens":100,"output_tokens":60,"cache_read_input_tokens":20,"cache_creation_input_tokens":10}}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-08-25T11:00:00.000Z","sessionId":"s2","message":{"id":"m3","usage":{"input_tokens":7,"output_tokens":2,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
            "\n",
        ),
    )
    .expect("corpus file must be writable");
}

fn config_for(root: &Path) -> agent_usage_book::config::Config {
    let toml = format!(
        r#"
[[transcripts]]
name = "claude-code"
root = "{}"
pattern = "**/*.jsonl"
format = "claude-code"
"#,
        root.display()
    );
    let (cfg, _) = resolve(
        &Overrides::new(),
        &FakeEnv::new(),
        Some(&toml),
        "/virtual/aub.toml",
    )
    .expect("resolve test config");
    cfg
}

fn migrated_conn(db_path: &Path) -> rusqlite::Connection {
    let policy = PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(1000),
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

/// One table's rows as ordered string tuples, read raw: a snapshot is a test
/// observation, not persistence logic, so it lives here rather than in the
/// store, and the queries are spelled where they are used.
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

/// The one snapshot of every rebuildable materialization the transcripts sweep
/// owns, compared whole: events, components, occurrences, sessions, watermarks
/// and quarantine, in that order, each stable-ordered.
struct Materializations {
    events: Vec<Vec<String>>,
    components: Vec<Vec<String>>,
    occurrences: Vec<Vec<String>>,
    sessions: Vec<Vec<String>>,
    watermarks: Vec<Vec<String>>,
    quarantine: Vec<Vec<String>>,
}

fn capture(conn: &rusqlite::Connection) -> Materializations {
    Materializations {
        events: events_snapshot(conn),
        components: components_snapshot(conn),
        occurrences: occurrences_snapshot(conn),
        sessions: sessions_snapshot(conn),
        watermarks: watermarks_snapshot(conn),
        quarantine: quarantine_snapshot(conn),
    }
}

/// A snapshot that is empty proves nothing about reproduction: every table the
/// sweep touches must hold real rows before the rebuild, or the equality after
/// it is vacuous.
fn assert_snapshot_holds_data(snapshot: &Materializations) {
    assert!(
        !snapshot.events.is_empty(),
        "the corpus must produce events"
    );
    assert!(
        !snapshot.components.is_empty(),
        "the corpus must produce components"
    );
    assert!(
        !snapshot.occurrences.is_empty(),
        "the corpus must produce occurrences"
    );
    assert!(
        !snapshot.sessions.is_empty(),
        "the corpus must produce sessions"
    );
    assert!(
        !snapshot.watermarks.is_empty(),
        "the corpus must produce watermarks"
    );
    assert!(
        !snapshot.quarantine.is_empty(),
        "the corpus must quarantine a record, or the sweep's quarantine coverage proves nothing"
    );
}

fn assert_identical(before: &Materializations, after: &Materializations) {
    assert_eq!(
        before.events, after.events,
        "canonical events must reproduce identically after rebuild and re-ingest"
    );
    assert_eq!(
        before.components, after.components,
        "token components must reproduce identically after rebuild and re-ingest"
    );
    assert_eq!(
        before.occurrences, after.occurrences,
        "usage occurrences must reproduce identically after rebuild and re-ingest"
    );
    assert_eq!(
        before.sessions, after.sessions,
        "session materializations must reproduce identically after rebuild and re-ingest"
    );
    assert_eq!(
        before.watermarks, after.watermarks,
        "file-index watermarks must reproduce identically after rebuild and re-ingest"
    );
    assert_eq!(
        before.quarantine, after.quarantine,
        "quarantine records must reproduce identically after rebuild and re-ingest"
    );
}

/// The bead's done-when: rebuild the transcripts group, ingest the same fixed
/// corpus again, and every rebuildable materialization is byte-identical to
/// what the first ingest landed, while the ingestion generation advanced
/// rather than reset (the counter is irreplaceable and survives the sweep).
#[test]
fn rebuild_then_reingest_reproduces_the_canonical_events_and_materializations() {
    let scratch = ScratchDir::new();
    let corpus_root = scratch.path().join("corpus");
    write_corpus(corpus_root.as_path());
    let config = config_for(corpus_root.as_path());

    let db_path = scratch.path().join("ledger.sqlite3");
    let mut conn = migrated_conn(&db_path);
    let now = UtcTimestamp::from_unix_nanos(50_000_000);

    // Pass 1: land the corpus.
    let first = run_ingest(&mut conn, &config, &IngestOptions::default(), now)
        .expect("the first ingest must succeed");
    assert_eq!(
        first.generation.value(),
        1,
        "the first pass lands as generation 1"
    );
    assert_eq!(first.files_parsed, 2, "both corpus files parse");
    assert_eq!(first.files_scanned, 2);
    assert_eq!(first.unreadable_files, Vec::<String>::new());
    let before = capture(&conn);
    assert_snapshot_holds_data(&before);

    // One canonical event for m1 despite three lines across two files.
    assert_eq!(
        before.events.len(),
        3,
        "m1, m2 and m3 collapse to three canonical events, with every m1 replay merged"
    );

    // The sweep: destroy exactly the transcripts group's materializations.
    let report = delete_rebuildable(&mut conn, RebuildGroup::Transcripts)
        .expect("the rebuild sweep must succeed");
    let swept: Vec<_> = report.deleted.iter().map(|(class, _)| *class).collect();
    assert_eq!(
        swept,
        RebuildGroup::Transcripts.classes(),
        "the sweep addresses exactly the taxonomy-derived class set"
    );
    for (class, count) in &report.deleted {
        assert!(
            count.value() > 0,
            "the sweep deletes real rows: {class:?} removed {}",
            count.value()
        );
    }
    for class in RebuildGroup::Transcripts.classes() {
        let table = class.table_name().expect("a sweep class is a table class");
        let remaining: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count must be readable");
        assert_eq!(remaining, 0, "{table} must be empty after the sweep");
    }

    // Pass 2: re-ingest the same corpus onto the emptied tables.
    let second = run_ingest(&mut conn, &config, &IngestOptions::default(), now)
        .expect("the re-ingest must succeed");
    assert_eq!(
        second.generation.value(),
        2,
        "the counter advanced through the sweep: rebuild destroys materializations, never the generation"
    );
    assert_eq!(
        second.outcome.events_written.value(),
        before.events.len() as u64,
        "the re-ingest rewrites every canonical event the sweep destroyed"
    );
    let after = capture(&conn);

    assert_identical(&before, &after);
}

/// The `--source` filter restricts the pass to one configured source, and a
/// filter naming nothing configured is a usage error naming what exists, never
/// a silently empty pass. The report's scanned and parsed counts name the
/// files of the covered source only.
#[test]
fn the_source_filter_restricts_the_pass_and_an_unknown_name_is_a_usage_error() {
    let scratch = ScratchDir::new();
    let first_root = scratch.path().join("first");
    let second_root = scratch.path().join("second");
    write_corpus(first_root.as_path());
    fs::create_dir_all(second_root.join("claude-code")).expect("second corpus dir");
    fs::write(
        second_root.join("claude-code/other.jsonl"),
        concat!(
            r#"{"type":"assistant","timestamp":"2026-08-25T12:00:00.000Z","sessionId":"s9","message":{"id":"m4","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
            "\n",
        ),
    )
    .expect("second corpus file must be writable");

    let toml = format!(
        r#"
[[transcripts]]
name = "first"
root = "{}"
pattern = "**/*.jsonl"
format = "claude-code"

[[transcripts]]
name = "second"
root = "{}"
pattern = "**/*.jsonl"
format = "claude-code"
"#,
        first_root.display(),
        second_root.display()
    );
    let config = {
        let (cfg, _) = resolve(
            &Overrides::new(),
            &FakeEnv::new(),
            Some(&toml),
            "/virtual/aub.toml",
        )
        .expect("resolve test config");
        cfg
    };
    let db_path = scratch.path().join("ledger.sqlite3");
    let mut conn = migrated_conn(&db_path);
    let now = UtcTimestamp::from_unix_nanos(50_000_000);

    // One source only: the second corpus's file is never opened.
    let options = IngestOptions {
        source: Some("first".to_string()),
        changed_only: false,
    };
    let report = run_ingest(&mut conn, &config, &options, now).expect("the filtered pass must run");
    assert_eq!(report.sources, vec!["first".to_string()]);
    assert_eq!(report.files_scanned, 2);
    assert_eq!(report.outcome.events_written.value(), 3);

    // The other source only.
    let options = IngestOptions {
        source: Some("second".to_string()),
        changed_only: false,
    };
    let report = run_ingest(&mut conn, &config, &options, now).expect("the second pass must run");
    assert_eq!(report.sources, vec!["second".to_string()]);
    assert_eq!(report.files_scanned, 1);
    assert_eq!(report.outcome.events_written.value(), 1);

    // An unknown name is a usage error naming the sources that exist.
    let options = IngestOptions {
        source: Some("no-such-source".to_string()),
        changed_only: false,
    };
    let err = run_ingest(&mut conn, &config, &options, now).unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("unknown transcript source no-such-source"),
        "the refusal must name the unknown source: {message}"
    );
    assert!(
        message.contains("first") && message.contains("second"),
        "the refusal must name the sources that exist: {message}"
    );
}

/// The automatic refresh path (`--changed-only`) and the explicit full ingest
/// path converge on the same canonical event set: an unchanged file skipped by
/// the index leaves the store exactly where the full pass put it. PLAN.md
/// 34.16 asks for this convergence explicitly.
#[test]
fn the_changed_only_refresh_converges_on_the_full_ingests_event_set() {
    let scratch = ScratchDir::new();
    let corpus_root = scratch.path().join("corpus");
    write_corpus(corpus_root.as_path());
    let config = config_for(corpus_root.as_path());
    let db_path = scratch.path().join("ledger.sqlite3");
    let mut conn = migrated_conn(&db_path);
    let now = UtcTimestamp::from_unix_nanos(50_000_000);

    run_ingest(&mut conn, &config, &IngestOptions::default(), now)
        .expect("the full ingest must succeed");
    let full = capture(&conn);
    let generation_after_full = agent_usage_book::store::ingestion_generation::current(&conn)
        .expect("the counter must read")
        .value();

    // A refresh pass over the unchanged corpus skips both files and lands
    // nothing: an empty pass is a completed pass, so the generation still
    // advances, and the materializations are unchanged.
    let options = IngestOptions {
        changed_only: true,
        source: None,
    };
    let report = run_ingest(&mut conn, &config, &options, now).expect("the refresh must succeed");
    assert_eq!(
        report.files_skipped, 2,
        "both unchanged files are skipped by the index"
    );
    assert_eq!(report.files_parsed, 0, "an unchanged corpus parses nothing");
    assert_eq!(
        report.outcome.events_written.value(),
        0,
        "a refresh over unchanged files writes no new events"
    );
    assert_eq!(
        agent_usage_book::store::ingestion_generation::current(&conn)
            .expect("the counter must read")
            .value(),
        generation_after_full + 1,
        "a completed refresh pass advances the generation even when it lands nothing"
    );

    let after = capture(&conn);
    assert_identical(&full, &after);
}
