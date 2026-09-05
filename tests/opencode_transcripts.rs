//! opencode as a transcript source (aub-g18q).
//!
//! opencode keeps every session in one SQLite database rather than the
//! line-delimited files the other sources write, so these tests build a
//! scratch `opencode.db` at runtime instead of reading a committed
//! transcript. The committed fixture is the seed the database is built from
//! (`tests/fixtures/transcripts/opencode/seed.json`): invented rows in the
//! real table shape, never copied from a real database.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agent_usage_book::config::{FakeEnv, Overrides, resolve};
use agent_usage_book::dedup::deduplicate;
use agent_usage_book::domain::ids::{NativeSessionId, SessionId, SourceNamespace};
use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::evidence::CoverageCompleteness;
use agent_usage_book::ingest::{IngestOptions, run as run_ingest};
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use agent_usage_book::transcripts::{
    ClaudeCodeParser, CodexParser, EvidenceClassification, ParserAdapter, PiParser,
    QuarantineClass, SourceLocation,
};
use agent_usage_book::transcripts::{
    KNOWN_FORMATS, OpencodeParser, namespace_for_format, parser_for_format,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aub-opencode-{tag}-{}-{suffix}",
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

/// The committed seed: invented session and message rows in the real table
/// shape. The database under test is always built from this file, so the
/// committed fixture is what every assertion below ultimately reads.
fn seed_value() -> serde_json::Value {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/transcripts/opencode/seed.json");
    let text = std::fs::read_to_string(&path).expect("seed fixture must be readable");
    serde_json::from_str(&text).expect("seed fixture must parse as JSON")
}

fn create_tables(conn: &rusqlite::Connection) {
    conn.execute_batch(
        "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, \
         parent_id TEXT, directory TEXT NOT NULL, title TEXT NOT NULL, \
         time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, model TEXT, \
         cost REAL NOT NULL, tokens_input INTEGER NOT NULL, tokens_output INTEGER NOT NULL, \
         tokens_reasoning INTEGER NOT NULL, tokens_cache_read INTEGER NOT NULL, \
         tokens_cache_write INTEGER NOT NULL);
         CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, \
         time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
         CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, \
         session_id TEXT NOT NULL, time_created INTEGER NOT NULL, \
         time_updated INTEGER NOT NULL, data TEXT NOT NULL);",
    )
    .expect("fixture tables must create");
}

fn insert_message(
    conn: &rusqlite::Connection,
    id: &str,
    session_id: &str,
    time_created_ms: i64,
    time_updated_ms: i64,
    data: &str,
) {
    conn.execute(
        "INSERT INTO message (id, session_id, time_created, time_updated, data) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![id, session_id, time_created_ms, time_updated_ms, data],
    )
    .expect("fixture message must insert");
}

/// Builds a scratch opencode database from the committed seed, one row per
/// seed entry, and returns its path. An optional mutation rewrites one
/// message's `data` before insert, so replay and malformed variants share
/// the same seed without a second committed file.
fn build_fixture_db(dir: &Path, name: &str, mutate: Option<(&str, serde_json::Value)>) -> PathBuf {
    let db_path = dir.join(name);
    let conn = rusqlite::Connection::open(&db_path).expect("fixture db must open");
    create_tables(&conn);
    let seed = seed_value();
    for session in seed["sessions"]
        .as_array()
        .expect("seed must hold sessions")
    {
        let model = serde_json::to_string(&session["model"]).expect("model must serialize");
        let parent: Option<&str> = session["parent_id"].as_str();
        conn.execute(
            "INSERT INTO session (id, project_id, parent_id, directory, title, \
             time_created, time_updated, model, cost, tokens_input, tokens_output, \
             tokens_reasoning, tokens_cache_read, tokens_cache_write) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            rusqlite::params![
                session["id"].as_str().expect("session id"),
                session["project_id"].as_str().expect("project id"),
                parent,
                session["directory"].as_str().expect("directory"),
                session["title"].as_str().expect("title"),
                session["time_created_ms"].as_i64().expect("created"),
                session["time_updated_ms"].as_i64().expect("updated"),
                model,
                session["cost"].as_f64().expect("cost"),
                session["tokens_input"].as_i64().expect("tokens"),
                session["tokens_output"].as_i64().expect("tokens"),
                session["tokens_reasoning"].as_i64().expect("tokens"),
                session["tokens_cache_read"].as_i64().expect("tokens"),
                session["tokens_cache_write"].as_i64().expect("tokens"),
            ],
        )
        .expect("fixture session must insert");
    }
    for message in seed["messages"]
        .as_array()
        .expect("seed must hold messages")
    {
        let id = message["id"].as_str().expect("message id");
        let mut data = message["data"].clone();
        if let Some((target, replacement)) = &mutate
            && *target == id
        {
            data = replacement.clone();
        }
        let data_text = serde_json::to_string(&data).expect("data must serialize");
        insert_message(
            &conn,
            id,
            message["session_id"].as_str().expect("session id"),
            message["time_created_ms"].as_i64().expect("created"),
            message["time_updated_ms"].as_i64().expect("updated"),
            &data_text,
        );
    }
    db_path
}

fn parse_db(path: &Path) -> agent_usage_book::transcripts::ParseOutput {
    OpencodeParser.parse_database_file(path, &SourceLocation::new(path.display().to_string(), 1))
}

fn known_of(event: &agent_usage_book::transcripts::NormalizedUsageEvent) -> (u64, u64, u64, u64) {
    let known = event.usage().known();
    (
        known.input().value(),
        known.output().value(),
        known.cache_read().value(),
        known.cache_write().value(),
    )
}

fn session_of(event: &agent_usage_book::transcripts::NormalizedUsageEvent) -> (String, String) {
    let session = event.session().expect("opencode events carry a session");
    (
        session.source().as_str().to_string(),
        session.native().as_str().to_string(),
    )
}

/// The fixture parses into the expected canonical events: input, output and
/// both cache kinds where the source carries them, `reasoning` under unknown
/// components, the message id as the canonical id, and one quarantine for
/// the assistant message without `tokens`.
#[test]
fn parses_the_seed_fixture_into_canonical_events() {
    let scratch = ScratchDir::new("parse");
    let db_path = build_fixture_db(scratch.path(), "opencode.db", None);
    let output = parse_db(&db_path);

    let actual: Vec<(u64, u64, u64, u64)> = output.events().iter().map(known_of).collect();
    assert_eq!(actual, vec![(120, 34, 9, 5), (40, 12, 3, 0), (10, 4, 0, 0)]);

    let reasoning: Vec<Option<u64>> = output
        .events()
        .iter()
        .map(|event| {
            event
                .usage()
                .unknown()
                .get("reasoning")
                .map(|count| count.value())
        })
        .collect();
    assert_eq!(reasoning, vec![Some(7), None, Some(2)]);

    let ids: Vec<&str> = output
        .events()
        .iter()
        .map(|event| event.strong_identity().expect("message id is the identity"))
        .collect();
    assert_eq!(
        ids,
        vec!["msg_fixture_0001", "msg_fixture_0003", "msg_fixture_0004"]
    );

    let sessions: Vec<(String, String)> = output.events().iter().map(session_of).collect();
    assert_eq!(
        sessions,
        vec![
            ("opencode".to_string(), "ses_fixture_parent".to_string()),
            ("opencode".to_string(), "ses_fixture_parent".to_string()),
            ("opencode".to_string(), "ses_fixture_child".to_string()),
        ]
    );

    for event in output.events() {
        assert_eq!(
            event.classification(),
            &EvidenceClassification::Reported,
            "native-usage sources are measured, never reconstructed"
        );
        assert_eq!(
            event.usage().coverage(),
            &CoverageCompleteness::Complete,
            "every event here carries all four known kinds"
        );
    }
    assert!(
        output.events()[0]
            .provenance()
            .sources()
            .contains("model:fixture-provider/fixture-model-a"),
        "provider and model name the model: {:?}",
        output.events()[0].provenance().sources()
    );
    assert_eq!(
        output.events()[0].occurred_at(),
        Some(UtcTimestamp::from_unix_nanos(1788220815000i64 * 1_000_000)),
        "the completion time is the event time"
    );
    assert_eq!(
        output.events()[0].session(),
        Some(&SessionId::new(
            SourceNamespace::new("opencode"),
            NativeSessionId::new("ses_fixture_parent"),
        ))
    );

    assert_eq!(output.quarantined().len(), 1);
    assert_eq!(
        output.quarantined()[0].class(),
        QuarantineClass::MissingRequiredField,
        "the assistant message without tokens is skipped with a count"
    );
}

/// A replayed message read from a second file collapses onto the same
/// canonical event by the strong identity, keeping the larger output: the
/// same dedup rules as every other format, with no parser-specific path.
#[test]
fn replays_across_files_collapse_to_one_canonical_event() {
    let scratch = ScratchDir::new("replay");
    let first = build_fixture_db(scratch.path(), "first.db", None);
    let grown = seed_value()["messages"][0]["data"].clone();
    let mut grown = grown;
    grown["tokens"]["output"] = serde_json::json!(90);
    let second = build_fixture_db(
        scratch.path(),
        "second.db",
        Some(("msg_fixture_0001", grown)),
    );

    let mut events = parse_db(&first).events().to_vec();
    events.extend(parse_db(&second).events().to_vec());
    assert_eq!(events.len(), 6, "two files of three events each");

    let deduplicated = deduplicate(events);
    assert_eq!(deduplicated.canonical.len(), 3);
    assert_eq!(deduplicated.replayed_occurrences, 3);
    assert_eq!(deduplicated.collisions, 0);
    let replayed = deduplicated
        .canonical
        .iter()
        .find(|event| event.strong_identity() == Some("msg_fixture_0001"))
        .expect("the replayed message survives");
    assert_eq!(
        replayed.usage().known().output().value(),
        90,
        "the larger output wins across the replay"
    );
}

/// Malformed rows quarantine with their failure class and do not abort the
/// rest of the file: unparseable payloads and wrong-typed counts are each
/// counted where they happened.
#[test]
fn malformed_rows_quarantine_without_aborting_the_rest() {
    let scratch = ScratchDir::new("malformed");
    let db_path = scratch.path().join("opencode.db");
    let conn = rusqlite::Connection::open(&db_path).expect("fixture db must open");
    create_tables(&conn);
    insert_message(
        &conn,
        "msg_good",
        "ses_fixture_parent",
        1788220800000,
        1788220815000,
        r#"{"role":"assistant","modelID":"m","providerID":"p","time":{"created":1788220800000,"completed":1788220815000},"tokens":{"input":5,"output":2,"cache":{"write":0,"read":0},"total":7}}"#,
    );
    insert_message(
        &conn,
        "msg_broken_json",
        "ses_fixture_parent",
        1788220820000,
        1788220820000,
        "this is not json",
    );
    insert_message(
        &conn,
        "msg_wrong_type",
        "ses_fixture_parent",
        1788220830000,
        1788220830000,
        r#"{"role":"assistant","tokens":{"input":"lots","output":2}}"#,
    );
    drop(conn);

    let output = parse_db(&db_path);
    assert_eq!(output.events().len(), 1, "the good row must survive");
    assert_eq!(known_of(&output.events()[0]), (5, 2, 0, 0));
    assert_eq!(output.quarantined().len(), 2);
    assert_eq!(
        output.quarantined()[0].class(),
        QuarantineClass::TruncatedStructure
    );
    assert_eq!(
        output.quarantined()[1].class(),
        QuarantineClass::WrongFieldType
    );
}

/// Reading the same database twice yields the same events: re-ingest
/// converges by replay instead of doubling the ledger.
#[test]
fn reading_the_same_database_twice_yields_the_same_events() {
    let scratch = ScratchDir::new("idempotent");
    let db_path = build_fixture_db(scratch.path(), "opencode.db", None);
    let first = parse_db(&db_path);
    let second = parse_db(&db_path);
    assert_eq!(first.events(), second.events());
    assert_eq!(
        first.quarantined().len(),
        second.quarantined().len(),
        "the skip count is stable too"
    );
}

/// Only the opencode parser reads database files: the text parsers keep the
/// default seam, so a database file is never misread as text and a text file
/// is never opened as a database.
#[test]
fn only_the_opencode_parser_reads_database_files() {
    assert!(OpencodeParser.is_database_source());
    assert!(!ClaudeCodeParser.is_database_source());
    assert!(!CodexParser.is_database_source());
    assert!(!PiParser.is_database_source());
}

fn migrated_ledger(state_dir: &Path) -> rusqlite::Connection {
    let policy = PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(1000),
    };
    let mut conn = open(&state_dir.join("ledger.db"), AccessMode::ReadWrite, &policy)
        .expect("ledger must open");
    run_migrations(
        &mut conn,
        &registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
    )
    .expect("migrations must apply");
    conn
}

fn config_for(root: &Path) -> agent_usage_book::config::Config {
    let toml = format!(
        r#"
[[transcripts]]
name = "opencode-fixture"
root = "{}"
pattern = "opencode.db"
format = "opencode"
"#,
        root.display()
    );
    let (config, _) = resolve(
        &Overrides::new(),
        &FakeEnv::new(),
        Some(&toml),
        "/virtual/aub.toml",
    )
    .expect("test config must resolve");
    config
}

/// Ingest of a fixture directory through the configured source: the database
/// is discovered by root plus pattern, parsed whole, landed with zero
/// unreadable files, and a second pass converges instead of doubling.
#[test]
fn ingest_of_a_fixture_directory_through_the_configured_source() {
    let scratch = ScratchDir::new("ingest");
    let source_root = scratch.path().join("opencode-storage");
    std::fs::create_dir(&source_root).expect("source root must be creatable");
    build_fixture_db(&source_root, "opencode.db", None);
    let state_dir = scratch.path().join("state");
    std::fs::create_dir(&state_dir).expect("state dir must be creatable");
    let mut conn = migrated_ledger(&state_dir);
    let config = config_for(&source_root);
    let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1788220900000i64 * 1_000_000));

    let first = run_ingest(
        &mut conn,
        &config,
        &IngestOptions::default(),
        &clock,
        &mut |_| Ok(()),
        &mut |_| Ok(()),
    )
    .expect("first ingest must succeed");
    assert_eq!(first.files_scanned, 1);
    assert_eq!(first.files_parsed, 1);
    assert!(first.unreadable_files.is_empty());
    assert_eq!(first.quarantined, 1, "the tokens-less message is counted");
    assert_eq!(first.outcome.events_written.value(), 3);
    assert_eq!(first.outcome.sessions_upserted.value(), 2);

    let second = run_ingest(
        &mut conn,
        &config,
        &IngestOptions::default(),
        &clock,
        &mut |_| Ok(()),
        &mut |_| Ok(()),
    )
    .expect("second ingest must succeed");
    // The default pass reparses whole files and replaces their contribution,
    // so the same rows land again; the ledger still holds three events, which
    // is what convergence means here rather than a zero-write pass.
    assert_eq!(second.outcome.events_written.value(), 3);
    let stored: i64 = conn
        .query_row("SELECT COUNT(*) FROM usage_event", [], |row| row.get(0))
        .expect("event count must be readable");
    assert_eq!(stored, 3, "re-ingest must converge, not double");

    let changed_only = run_ingest(
        &mut conn,
        &config,
        &IngestOptions {
            changed_only: true,
            ..IngestOptions::default()
        },
        &clock,
        &mut |_| Ok(()),
        &mut |_| Ok(()),
    )
    .expect("changed-only ingest must succeed");
    assert_eq!(changed_only.files_skipped, 1);
}

/// The contract: the format list in help and docs matches the parser
/// registry. Every known format resolves to a parser and a namespace, no
/// unknown format does, and the operator-facing lists name every one.
#[test]
fn format_registry_docs_and_help_agree() {
    assert_eq!(KNOWN_FORMATS, &["claude-code", "codex", "opencode", "pi"]);
    for format in KNOWN_FORMATS {
        assert!(
            parser_for_format(format).is_some(),
            "format {format} must resolve to a parser"
        );
        assert!(
            namespace_for_format(format).is_some(),
            "format {format} must resolve to a namespace"
        );
    }
    assert!(parser_for_format("carrier-pigeon").is_none());
    assert!(namespace_for_format("carrier-pigeon").is_none());

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let readme = std::fs::read_to_string(root.join("README.md")).expect("README must exist");
    let commands =
        std::fs::read_to_string(root.join("docs/commands.md")).expect("commands doc must exist");
    for format in KNOWN_FORMATS {
        assert!(readme.contains(format), "README must list format {format}");
        assert!(
            commands.contains(format),
            "docs/commands.md must list format {format}"
        );
    }
    assert!(
        readme.contains("# or \"codex\", \"pi\", \"opencode\""),
        "README transcripts example must list opencode among the format values"
    );
    assert!(
        readme.contains("format = \"opencode\""),
        "README must show an opencode transcripts entry"
    );
}
