//! The heuristic-dedup collision path end to end (aub-lqe.10, PLAN.md 12.10,
//! 18, 34.13).
//!
//! A fixture transcript carries a deliberate semantic collision: two records
//! that share a heuristic key (same timestamp, session and input counts, no
//! stable identifier) but normalize to materially different payloads, because
//! one counts its output and the other admits it does not know it. The fixture
//! must produce all three consequences together: a collision diagnostic, a
//! quarantine entry, and a partial aggregate, with no code path selecting one
//! of the two occurrences. The integration wires the same seams the ingest
//! path will: detect in dedup, record in the quarantine, mark the aggregate.
//!
//! May not depend on:
//! - presentation
//! - provider adapters
//! - HTTP or terminal formatting

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agent_usage_book::dedup::{HeuristicKey, deduplicate};
use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::report::spend::{SpendWindow, assemble};
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::ingest_quarantine::{
    DEDUP_COLLISION_FAILURE_CLASS, DedupCollisionDescriptor, load_all_quarantine,
    record_dedup_collision,
};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use agent_usage_book::store::usage_occurrence::{
    NewUsageOccurrence, heuristic_dedup_usage, heuristic_rebuild_required, insert_occurrence,
};
use agent_usage_book::transcripts::SourceLocation;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aub-dedup-collision-{tag}-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("scratch dir must be creatable");
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

fn migrated_conn(tag: &str) -> (ScratchDir, rusqlite::Connection) {
    let scratch = ScratchDir::new(tag);
    let policy = PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(1000),
    };
    let mut conn = open(
        &scratch.path().join("collision.db"),
        AccessMode::ReadWrite,
        &policy,
    )
    .unwrap();
    run_migrations(
        &mut conn,
        &registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
    )
    .unwrap();
    (scratch, conn)
}

/// The deliberate semantic collision: the first record counts its output, the
/// second omits the field entirely, and everything the heuristic key sees (the
/// timestamp, the session, the input and cache counts) agrees. A third record
/// carries a stable identifier and survives the collision untouched.
fn fixture_transcript() -> String {
    [
        r#"{"type":"assistant","timestamp":"2026-08-25T10:00:00Z","sessionId":"s1","message":{"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
        r#"{"type":"assistant","timestamp":"2026-08-25T10:00:00Z","sessionId":"s1","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
        r#"{"type":"assistant","timestamp":"2026-08-25T10:05:00Z","sessionId":"s1","message":{"id":"m9","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
    ]
    .join("\n")
}

fn claude_config(root: &Path) -> agent_usage_book::config::Config {
    let (mut config, _) = agent_usage_book::config::resolve(
        &agent_usage_book::config::Overrides::new(),
        &agent_usage_book::config::FakeEnv::new().set("HOME", "/tmp/synthetic-home"),
        None,
        "/test/aub.toml",
    )
    .unwrap();
    config.transcripts = vec![agent_usage_book::config::TranscriptConfig {
        name: "cc".to_string(),
        root: root.to_path_buf(),
        pattern: "**/*.jsonl".to_string(),
        format: Some("claude-code".to_string()),
        usage_evidence: None,
    }];
    config
}

#[test]
fn a_deliberate_collision_produces_diagnostic_quarantine_and_partial_aggregate() {
    let root = ScratchDir::new("fixture");
    std::fs::write(root.path().join("session.jsonl"), fixture_transcript()).unwrap();

    // The real parser over the real file: the collision arises from parsing,
    // not from a hand-built pair.
    let parser = agent_usage_book::transcripts::parser_for_format("claude-code")
        .expect("the claude-code format must parse");
    let contents = std::fs::read_to_string(root.path().join("session.jsonl")).unwrap();
    let output = parser.parse(
        &contents,
        &SourceLocation::new(root.path().join("session.jsonl").display().to_string(), 1),
    );
    assert_eq!(output.events().len(), 3, "all three records normalize");

    // The diagnostic: one collision, both occurrences excluded from canonical,
    // and the pair named in first-seen order.
    let deduplicated = deduplicate(output.events().to_vec());
    assert_eq!(deduplicated.heuristic_collisions.len(), 1);
    assert_eq!(
        deduplicated.canonical.len(),
        1,
        "the colliding pair is quarantined, never merged"
    );
    assert_eq!(deduplicated.replayed_occurrences, 0);
    assert_eq!(deduplicated.without_identity, 2);
    let collision = &deduplicated.heuristic_collisions[0];
    assert_eq!(collision.parser_version().as_str(), "claude-code-1");
    assert_eq!(
        collision.occurrences()[0].source_file(),
        collision.occurrences()[1].source_file()
    );
    let (first_digest, second_digest) = collision.payload_digests();
    assert_ne!(first_digest, second_digest);

    // The quarantine entry: the pair is recorded with the collision as its
    // failure class, and the same collision recurring merges rather than
    // duplicating.
    let (scratch, conn) = migrated_conn("store");
    let descriptor = DedupCollisionDescriptor {
        parser: collision.parser_version().as_str().to_string(),
        heuristic_key: collision.heuristic_key().as_str().to_string(),
        first_file: collision.occurrences()[0].source_file().to_string(),
        first_payload_digest: first_digest.to_string(),
        second_payload_digest: second_digest.to_string(),
        observed_at: UtcTimestamp::from_unix_nanos(10_000),
    };
    record_dedup_collision(&conn, &descriptor).unwrap();
    let rows = load_all_quarantine(&conn).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].failure_class(), DEDUP_COLLISION_FAILURE_CLASS);
    assert_eq!(rows[0].parser(), "claude-code-1");
    record_dedup_collision(&conn, &descriptor).unwrap();
    assert_eq!(
        load_all_quarantine(&conn).unwrap().len(),
        1,
        "the same collision recurring merges, never duplicates"
    );

    // The partial aggregate, through the real spend path over the fixture:
    // the day's group carries only the surviving event's counts and reports
    // the pair's components as its missing coverage.
    let window = SpendWindow::starting(
        agent_usage_book::domain::time::UtcDate::parse("2026-08-25").unwrap(),
        1,
    )
    .unwrap();
    let now = UtcTimestamp::parse_rfc3339("2026-08-30T12:00:00Z").unwrap();
    let report = assemble(&claude_config(root.path()), window, now).unwrap();
    assert_eq!(report.groups.len(), 1);
    let group = &report.groups[0];
    assert_eq!(group.usage.known().input().value(), 100);
    assert_eq!(
        group.usage.known().output().value(),
        50,
        "the group counts only the surviving event: no winner was picked"
    );
    let missing: Vec<String> = group
        .usage
        .coverage()
        .missing()
        .expect("the aggregate must read partial")
        .iter()
        .map(|kind| kind.as_str().to_string())
        .collect();
    assert_eq!(missing, ["input", "output"]);

    // The doctor's data: what ingest persists (the surviving heuristic
    // occurrence with its algorithm version and digest) plus the quarantine
    // row report usage and collisions per parser.
    let namespace = agent_usage_book::domain::ids::SourceNamespace::new("claude-code");
    let version = agent_usage_book::transcripts::ParserVersion::new("claude-code-1");
    let survivor = &deduplicated.canonical[0];
    insert_occurrence(
        &conn,
        &NewUsageOccurrence {
            source_namespace: &namespace,
            native_event_id: None,
            parser_version: &version,
            heuristic_key: Some(HeuristicKey::compute(survivor).as_str()),
            source_file: survivor.source_file(),
            occurred_at_nanos: Some(survivor.occurred_at().unwrap().unix_nanos()),
            event_id: None,
            transcript_file_id: None,
            source_location: None,
            canonical_fingerprint: Some(HeuristicKey::compute(survivor).as_str()),
            identity_strength: Some("heuristic"),
            heuristic_algorithm_version: Some(HeuristicKey::ALGORITHM_VERSION),
            canonical_payload_digest: Some(
                agent_usage_book::dedup::canonical_payload_digest(survivor).as_str(),
            ),
        },
    )
    .unwrap();
    let usage = heuristic_dedup_usage(&conn).unwrap();
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].parser, "claude-code-1");
    assert_eq!(usage[0].heuristic_identities, 1);
    assert_eq!(usage[0].collisions, 1);

    // The version the ledger was built under is the running version, so
    // nothing demands a rebuild; a mismatched stored version is reported and
    // never silently absorbed.
    let rebuilds = heuristic_rebuild_required(&conn, HeuristicKey::ALGORITHM_VERSION).unwrap();
    assert!(
        rebuilds.is_empty(),
        "a ledger built under the running version needs no rebuild"
    );
    drop(conn);
    drop(scratch);
}
