//! Integration tests for session timelines and alias resolution (`aub-lqe.12`,
//! PLAN.md 12.8, 19.1, 19.3).
//!
//! May not depend on:
//! - presentation
//! - provider adapters
//! - HTTP or terminal formatting

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agent_usage_book::config::AliasTable;
use agent_usage_book::config::{FakeEnv, Overrides, resolve as resolve_config};
use agent_usage_book::domain::ids::{NativeSessionId, SessionId, SourceNamespace};
use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::ingest::{IngestOptions, run as run_ingest};
use agent_usage_book::sessions::resolver::{
    ProjectKey, RepositoryKey, UNKNOWN_PROJECT, UNKNOWN_REPOSITORY, first_working_directories,
    resolve_project, resolve_repository,
};
use agent_usage_book::sessions::timeline::{
    count_events_by_project, derive_session_bounds, rebuild_sessions,
};
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::ledger_generation;
use agent_usage_book::store::session::{
    NewSession, insert_session, load_all_sessions, reresolve_keys,
};
use agent_usage_book::store::usage_component::{NewUsageComponent, insert_component};
use agent_usage_book::store::usage_event::{NewUsageEvent, insert_event};
use agent_usage_book::store::usage_occurrence::{NewUsageOccurrence, insert_occurrence};
use agent_usage_book::transcripts::{
    ClaudeCodeParser, ParserAdapter, ParserVersion, SourceLocation,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Self {
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aub-sessions-integration-test-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir(&path).expect("scratch dir must be creatable");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture_conn() -> (ScratchDir, rusqlite::Connection) {
    let scratch = ScratchDir::new();
    let db_path = scratch.path().join("session_integration.db");
    let policy = PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(1000),
    };
    let mut conn = open(&db_path, AccessMode::ReadWrite, &policy).unwrap();
    agent_usage_book::store::migrate::run_migrations(
        &mut conn,
        &agent_usage_book::store::migrations::registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
    )
    .unwrap();
    (scratch, conn)
}

fn ts(nanos: i64) -> UtcTimestamp {
    UtcTimestamp::from_unix_nanos(nanos)
}

fn session(source: &str, native: &str) -> SessionId {
    SessionId::new(SourceNamespace::new(source), NativeSessionId::new(native))
}

fn aliases(pairs: &[(&str, &str)]) -> AliasTable {
    AliasTable::new(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
    .unwrap()
}

#[test]
fn two_identical_native_session_ids_from_different_sources_remain_distinct() {
    let (_scratch, mut conn) = fixture_conn();
    let events = vec![
        (session("claude-code", "session-alpha"), ts(10_000)),
        (session("codex", "session-alpha"), ts(20_000)),
        (session("pi", "session-alpha"), ts(30_000)),
    ];
    let dirs = HashMap::new();
    let projects = aliases(&[]);
    let repositories = aliases(&[]);

    let count = rebuild_sessions(&mut conn, &events, &dirs, &projects, &repositories).unwrap();
    assert_eq!(count, 3, "three distinct source namespaces");

    let stored = load_all_sessions(&conn).unwrap();
    assert_eq!(stored.len(), 3);
    assert_eq!(stored[0].source().as_str(), "claude-code");
    assert_eq!(stored[1].source().as_str(), "codex");
    assert_eq!(stored[2].source().as_str(), "pi");

    for s in &stored {
        assert_eq!(s.native_session_id().as_str(), "session-alpha");
    }
}

#[test]
fn alias_resolution_for_project_and_repository_with_unknown_buckets() {
    let table = aliases(&[
        ("/home/dev/work/aub", "agent-usage-book"),
        ("/home/dev/work/other", "other-tool"),
    ]);

    assert_eq!(
        resolve_project(&table, Some("/home/dev/work/aub")).as_str(),
        "agent-usage-book"
    );
    assert_eq!(
        resolve_repository(&table, Some("/home/dev/work/aub")).as_str(),
        "agent-usage-book"
    );
    assert_eq!(
        resolve_project(&table, Some("/home/dev/work/unmapped")).as_str(),
        UNKNOWN_PROJECT
    );
    assert_eq!(
        resolve_repository(&table, Some("/home/dev/work/unmapped")).as_str(),
        UNKNOWN_REPOSITORY
    );
    assert_eq!(resolve_project(&table, None).as_str(), UNKNOWN_PROJECT);
    assert_eq!(
        resolve_repository(&table, None).as_str(),
        UNKNOWN_REPOSITORY
    );
}

#[test]
fn no_absolute_machine_path_stored_as_report_identity() {
    let (_scratch, mut conn) = fixture_conn();
    let mapped = session("claude-code", "sess-mapped");
    let unmapped = session("claude-code", "sess-unmapped");
    let events = vec![(mapped.clone(), ts(1000)), (unmapped.clone(), ts(2000))];
    let dirs = HashMap::from([
        (mapped.clone(), Some("/home/developer/code/aub".to_string())),
        (unmapped.clone(), Some("/var/tmp/scratch".to_string())),
    ]);
    let projects = aliases(&[("/home/developer/code/aub", "agent-usage-book")]);
    let repositories = aliases(&[("/home/developer/code/aub", "agent-usage-book")]);

    rebuild_sessions(&mut conn, &events, &dirs, &projects, &repositories).unwrap();
    let stored = load_all_sessions(&conn).unwrap();

    for row in &stored {
        assert!(
            !row.project_key().as_str().starts_with('/'),
            "project key must not be a machine path: {}",
            row.project_key().as_str()
        );
        assert!(
            !row.repository_key().as_str().starts_with('/'),
            "repository key must not be a machine path: {}",
            row.repository_key().as_str()
        );
    }
}

#[test]
fn report_grouped_by_project_accounts_for_every_canonical_event_with_unknown_bucket_visible() {
    let s1 = session("claude-code", "s1");
    let s2 = session("claude-code", "s2");
    let s3 = session("codex", "s3");
    let events = vec![
        (s1.clone(), ts(100)),
        (s1.clone(), ts(200)),
        (s2.clone(), ts(300)),
        (s3.clone(), ts(400)),
    ];
    let dirs = HashMap::from([
        (s1.clone(), Some("/work/aub".to_string())),
        (s2.clone(), Some("/work/other".to_string())),
        (s3.clone(), None),
    ]);
    let projects = aliases(&[("/work/aub", "agent-usage-book")]);

    let counts = count_events_by_project(&events, &dirs, &projects);
    let sum: usize = counts.values().sum();
    assert_eq!(sum, 4, "every event must be accounted for");
    assert_eq!(
        counts.get(&ProjectKey::new("agent-usage-book")),
        Some(&2),
        "s1 has 2 events mapped to agent-usage-book"
    );
    assert_eq!(
        counts.get(&ProjectKey::new(UNKNOWN_PROJECT)),
        Some(&2),
        "s2 (/work/other) and s3 (None) land in unknown-project"
    );
}

#[test]
fn session_start_and_end_derive_from_event_timestamps() {
    let timestamps = vec![ts(5000), ts(1000), ts(3000), ts(7000)];
    let (start, end) = derive_session_bounds(&timestamps).unwrap();
    assert_eq!(start, ts(1000));
    assert_eq!(end, Some(ts(7000)));

    let single = vec![ts(42)];
    let (s_start, s_end) = derive_session_bounds(&single).unwrap();
    assert_eq!(s_start, ts(42));
    assert_eq!(s_end, Some(ts(42)));

    assert_eq!(derive_session_bounds(&[]), None);
}

#[test]
fn rebuild_determinism_property_over_sessions() {
    let (_scratch, mut conn) = fixture_conn();
    let events = vec![
        (session("claude-code", "a"), ts(100)),
        (session("claude-code", "a"), ts(500)),
        (session("codex", "b"), ts(200)),
        (session("pi", "c"), ts(300)),
    ];
    let dirs = HashMap::from([
        (session("claude-code", "a"), Some("/aub".to_string())),
        (session("codex", "b"), Some("/other".to_string())),
    ]);
    let projects = aliases(&[("/aub", "proj-aub"), ("/other", "proj-other")]);
    let repos = aliases(&[("/aub", "repo-aub"), ("/other", "repo-other")]);

    rebuild_sessions(&mut conn, &events, &dirs, &projects, &repos).unwrap();
    let first = load_all_sessions(&conn).unwrap();

    // Rebuild again on fresh table
    agent_usage_book::store::session::clear_all_sessions(&conn).unwrap();
    rebuild_sessions(&mut conn, &events, &dirs, &projects, &repos).unwrap();
    let second = load_all_sessions(&conn).unwrap();

    assert_eq!(first, second, "rebuilt sessions must be strictly identical");
    assert_eq!(first.len(), 3);
}

/// The working-directory change fixture through the real parser: one session
/// whose two lines state different directories keeps the first, and the
/// session counts exactly once as changed.
#[test]
fn fixture_session_with_two_directories_keeps_the_first_and_counts_one_change() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/transcripts/working-directory/claude-code-changes.jsonl");
    let input = std::fs::read_to_string(&path).expect("change fixture must be readable");
    let output =
        ClaudeCodeParser.parse(&input, &SourceLocation::new("claude-code-changes.jsonl", 1));
    assert_eq!(output.events().len(), 2);

    let (first, changes) = first_working_directories(output.events().iter().filter_map(|event| {
        event.session().map(|session| {
            (
                (
                    session.source().as_str().to_string(),
                    session.native().as_str().to_string(),
                ),
                event.working_directory().map(str::to_string),
            )
        })
    }));
    assert_eq!(
        first
            .get(&("claude-code".to_string(), "s1".to_string()))
            .cloned()
            .flatten()
            .as_deref(),
        Some("/tmp/aub-fixture-project"),
        "the session keeps the first stated directory"
    );
    assert_eq!(changes, 1, "one session disagreed, counted once");

    // The planted negative: an aggregation that kept the last value instead
    // of the first would report the moved directory here.
    assert_ne!(
        first
            .get(&("claude-code".to_string(), "s1".to_string()))
            .cloned()
            .flatten()
            .as_deref(),
        Some("/tmp/aub-fixture-project-moved")
    );
}

fn seed_session_with_directory(
    conn: &rusqlite::Connection,
    source: &str,
    native: &str,
    directory: Option<&str>,
) {
    insert_session(
        conn,
        &NewSession {
            source: SourceNamespace::new(source),
            native_session_id: NativeSessionId::new(native),
            start: ts(1000),
            end: Some(ts(2000)),
            project_key: ProjectKey::new(UNKNOWN_PROJECT),
            repository_key: RepositoryKey::new(UNKNOWN_REPOSITORY),
            working_directory: directory.map(str::to_string),
            run_id: None,
        },
    )
    .unwrap();
}

fn seed_evidence(conn: &rusqlite::Connection) {
    let event = insert_event(
        conn,
        &NewUsageEvent {
            canonical_event_id: "ce-reresolve-1",
            session_id: Some("sess-mapped"),
            event_timestamp: Some(ts(1000)),
            model_id: None,
            evidence_kind: "transcript",
            source_provenance: "corpus/a.jsonl",
            parser_version: "claude-code-1",
            created_at: ts(1000),
        },
    )
    .unwrap();
    insert_component(
        conn,
        &NewUsageComponent {
            event_id: event,
            token_class: "input",
            count: 100,
        },
    )
    .unwrap();
    let namespace = SourceNamespace::new("claude-code");
    let parser_version = ParserVersion::new("claude-code-1");
    insert_occurrence(
        conn,
        &NewUsageOccurrence {
            source_namespace: &namespace,
            native_event_id: Some("m1"),
            parser_version: &parser_version,
            heuristic_key: None,
            source_file: "corpus/a.jsonl",
            occurred_at_nanos: Some(1000),
            event_id: Some(event),
            transcript_file_id: None,
            source_location: None,
            canonical_fingerprint: None,
            identity_strength: None,
            heuristic_algorithm_version: None,
            canonical_payload_digest: None,
        },
    )
    .unwrap();
}

/// One ordered dump per evidence table: row counts plus full content, so the
/// test below asserts byte-identical evidence rather than merely equal
/// counts.
fn evidence_snapshot(conn: &rusqlite::Connection) -> Vec<(String, Vec<String>)> {
    let mut snapshot = Vec::new();
    for (table, columns) in [
        (
            "usage_event",
            "id, canonical_event_id, session_id, event_timestamp, model_id, evidence_kind, \
             source_provenance, parser_version, created_at",
        ),
        ("usage_component", "id, event_id, token_class, count"),
        (
            "usage_occurrence",
            "id, source_namespace, native_event_id, parser_version, heuristic_key, \
             source_file, occurred_at, event_id, transcript_file_id, source_location, \
             canonical_fingerprint, identity_strength, heuristic_algorithm_version, \
             canonical_payload_digest",
        ),
    ] {
        let width = columns.split(',').count();
        let mut stmt = conn
            .prepare(&format!("SELECT {columns} FROM {table} ORDER BY id"))
            .expect("evidence scan must prepare");
        let rows: Vec<String> = stmt
            .query_map([], |row| {
                let mut values = Vec::with_capacity(width);
                for index in 0..width {
                    let value: rusqlite::types::Value = row.get(index)?;
                    values.push(format!("{value:?}"));
                }
                Ok(values.join("|"))
            })
            .expect("evidence scan must query")
            .map(|row| row.expect("evidence row must read"))
            .collect();
        snapshot.push((table.to_string(), rows));
    }
    snapshot
}

/// `rebuild sessions` on a seeded ledger: the keys follow the alias table
/// across two runs with different tables, the evidence tables are
/// byte-identical throughout, and the ledger generation advances each run.
#[test]
fn rebuild_sessions_reresolves_keys_and_leaves_evidence_untouched() {
    let (_scratch, mut conn) = fixture_conn();
    seed_session_with_directory(
        &conn,
        "claude-code",
        "sess-mapped",
        Some("/tmp/aub-fixture-project"),
    );
    seed_session_with_directory(&conn, "claude-code", "sess-bare", None);
    seed_evidence(&conn);
    let before = evidence_snapshot(&conn);
    assert_eq!(before[0].1.len(), 1, "one seeded event");
    assert_eq!(before[1].1.len(), 1, "one seeded component");
    assert_eq!(before[2].1.len(), 1, "one seeded occurrence");
    let generation_before = ledger_generation::current(&conn).unwrap();

    let first_table = aliases(&[("/tmp/aub-fixture-project", "fixture")]);
    let outcome = reresolve_keys(&mut conn, &first_table, &first_table).unwrap();
    assert_eq!(outcome.sessions, 2);
    assert_eq!(
        outcome.generation,
        ledger_generation::Generation::new(generation_before.value() + 1),
        "the rewrite advances the ledger generation"
    );
    let stored = load_all_sessions(&conn).unwrap();
    let mapped = stored
        .iter()
        .find(|row| row.native_session_id().as_str() == "sess-mapped")
        .unwrap();
    assert_eq!(mapped.project_key().as_str(), "fixture");
    assert_eq!(mapped.repository_key().as_str(), "fixture");
    assert_eq!(
        mapped.working_directory(),
        Some("/tmp/aub-fixture-project"),
        "the stored directory survives the re-resolve"
    );
    let bare = stored
        .iter()
        .find(|row| row.native_session_id().as_str() == "sess-bare")
        .unwrap();
    assert_eq!(bare.project_key().as_str(), UNKNOWN_PROJECT);
    assert_eq!(
        bare.working_directory(),
        None,
        "a session with no directory stays unknown"
    );
    assert_eq!(
        evidence_snapshot(&conn),
        before,
        "evidence tables are byte-identical after the re-resolve"
    );

    // A changed alias table moves the keys on the next run: the new alias
    // applies to history, not only to sessions ingested after it.
    let second_table = aliases(&[("/tmp/aub-fixture-project", "renamed")]);
    let outcome = reresolve_keys(&mut conn, &second_table, &second_table).unwrap();
    assert_eq!(outcome.sessions, 2);
    let stored = load_all_sessions(&conn).unwrap();
    let mapped = stored
        .iter()
        .find(|row| row.native_session_id().as_str() == "sess-mapped")
        .unwrap();
    assert_eq!(
        mapped.project_key().as_str(),
        "renamed",
        "the keys follow the current table"
    );
    assert_eq!(
        evidence_snapshot(&conn),
        before,
        "evidence tables are still byte-identical after the second run"
    );
    assert_eq!(
        ledger_generation::current(&conn).unwrap().value(),
        generation_before.value() + 2
    );

    // Idempotent: a third run with the same table changes nothing but still
    // advances the generation with the rewrite.
    let stored_before = load_all_sessions(&conn).unwrap();
    reresolve_keys(&mut conn, &second_table, &second_table).unwrap();
    assert_eq!(load_all_sessions(&conn).unwrap(), stored_before);
}

/// With `[projects] "/tmp/aub-fixture-project" = "fixture"` in the config,
/// ingesting the Claude Code fixture whose `cwd` is that path yields
/// `project_key = fixture`; without the alias the same ingest yields
/// `unknown-project` while the directory is still stored.
#[test]
fn ingest_resolves_project_from_the_transcript_directory_through_aliases() {
    let scratch = ScratchDir::new();
    let corpus = scratch.path().join("corpus");
    std::fs::create_dir(&corpus).expect("corpus dir must be creatable");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/transcripts/working-directory/claude-code.jsonl");
    std::fs::copy(&fixture, corpus.join("session.jsonl")).expect("fixture must copy");

    let config_with_alias = format!(
        "[projects]\n\"/tmp/aub-fixture-project\" = \"fixture\"\n\n\
         [[transcripts]]\nname = \"claude-code\"\nroot = \"{}\"\n\
         pattern = \"**/*.jsonl\"\nformat = \"claude-code\"\n",
        corpus.display()
    );
    let (config, _) = resolve_config(
        &Overrides::new(),
        &FakeEnv::new(),
        Some(&config_with_alias),
        "/virtual/aub.toml",
    )
    .expect("config with alias must resolve");
    let (_ledger_scratch, mut conn) = fixture_conn();
    let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(2_000_000));
    let report = run_ingest(
        &mut conn,
        &config,
        &IngestOptions::default(),
        &clock,
        &mut |_| Ok(()),
        &mut |_| Ok(()),
    )
    .expect("ingest with alias must succeed");
    assert_eq!(report.working_directory_changes, 0);
    let stored = load_all_sessions(&conn).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].project_key().as_str(), "fixture");
    assert_eq!(
        stored[0].working_directory(),
        Some("/tmp/aub-fixture-project")
    );

    // The planted negative: the same corpus without the alias lands in the
    // unknown bucket, while the directory is still stored. An implementation
    // that resolved the key but dropped the evidence, or stored nothing at
    // all, fails one of the two assertions.
    let config_without_alias = format!(
        "[[transcripts]]\nname = \"claude-code\"\nroot = \"{}\"\n\
         pattern = \"**/*.jsonl\"\nformat = \"claude-code\"\n",
        corpus.display()
    );
    let (config, _) = resolve_config(
        &Overrides::new(),
        &FakeEnv::new(),
        Some(&config_without_alias),
        "/virtual/aub.toml",
    )
    .expect("config without alias must resolve");
    let (_ledger_scratch, mut conn) = fixture_conn();
    run_ingest(
        &mut conn,
        &config,
        &IngestOptions::default(),
        &clock,
        &mut |_| Ok(()),
        &mut |_| Ok(()),
    )
    .expect("ingest without alias must succeed");
    let stored = load_all_sessions(&conn).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].project_key().as_str(), UNKNOWN_PROJECT);
    assert_eq!(
        stored[0].working_directory(),
        Some("/tmp/aub-fixture-project"),
        "the directory is stored even when no alias maps it"
    );
}
