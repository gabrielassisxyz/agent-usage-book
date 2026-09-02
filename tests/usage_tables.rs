//! Integration tests for `usage_event`, `usage_component`, and `usage_occurrence` tables (`aub-lqe.8`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_usage_book::domain::ids::SourceNamespace;
use agent_usage_book::domain::time::{FakeClock, UtcTimestamp};
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use agent_usage_book::store::usage_component::{
    NewUsageComponent, get_components_for_event, insert_component, insert_components,
};
use agent_usage_book::store::usage_event::{EventId, NewUsageEvent, insert_event};
use agent_usage_book::store::usage_occurrence::{
    NewUsageOccurrence, count_occurrences_by_canonical_event, count_occurrences_for_event,
    get_occurrences_for_event, insert_occurrence,
};
use agent_usage_book::transcripts::parser::ParserVersion;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestDb {
    path: PathBuf,
}

impl TestDb {
    fn new() -> Self {
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aub-test-usage-tables-{}-{count}.sqlite3",
            std::process::id()
        ));
        Self { path }
    }

    fn open(&self) -> rusqlite::Connection {
        let policy = PragmaPolicy {
            busy_timeout: agent_usage_book::domain::time::MonotonicDuration::from_millis(5000),
        };
        let mut conn = open(&self.path, AccessMode::ReadWrite, &policy).unwrap();
        run_migrations(
            &mut conn,
            &registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .unwrap();
        conn
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[test]
fn test_unknown_token_class_stored_as_component_row_without_schema_change() {
    let db = TestDb::new();
    let conn = db.open();

    let event_id = insert_event(
        &conn,
        &NewUsageEvent {
            canonical_event_id: "evt-001",
            session_id: Some("session-1"),
            event_timestamp: Some(UtcTimestamp::from_unix_nanos(1_000_000)),
            model_id: Some("claude-3-7-sonnet"),
            evidence_kind: "native",
            source_provenance: "transcripts/file.jsonl:1",
            parser_version: "claude-code-1",
            created_at: UtcTimestamp::from_unix_nanos(1_000_000),
        },
    )
    .unwrap();

    // Standard + future unknown token classes
    insert_components(
        &conn,
        event_id,
        &[
            ("input", 100),
            ("output", 200),
            ("cache_read", 50),
            ("cache_write", 20),
            ("reasoning_future_tokens", 500),
            ("thought_stream_v3", 75),
        ],
    )
    .unwrap();

    let components = get_components_for_event(&conn, event_id).unwrap();
    assert_eq!(components.len(), 6);
    assert_eq!(components[4].token_class, "reasoning_future_tokens");
    assert_eq!(components[4].count, 500);
    assert_eq!(components[5].token_class, "thought_stream_v3");
    assert_eq!(components[5].count, 75);
}

#[test]
fn test_replayed_corpus_produces_one_canonical_event_with_many_occurrences() {
    let db = TestDb::new();
    let conn = db.open();

    let event_id = insert_event(
        &conn,
        &NewUsageEvent {
            canonical_event_id: "canonical-evt-replay",
            session_id: Some("session-replay-100"),
            event_timestamp: Some(UtcTimestamp::from_unix_nanos(2_000_000)),
            model_id: Some("claude-3-5-sonnet"),
            evidence_kind: "native",
            source_provenance: "file-a.jsonl:10",
            parser_version: "claude-code-1",
            created_at: UtcTimestamp::from_unix_nanos(2_000_000),
        },
    )
    .unwrap();

    insert_components(&conn, event_id, &[("input", 1000), ("output", 250)]).unwrap();

    let namespace = SourceNamespace::new("claude-code");
    let version = ParserVersion::new("claude-code-1");

    // Insert 3 occurrences with distinct source positions
    for i in 1..=3 {
        let native_id = format!("msg-replay-{i}");
        let source_file = format!("file-{i}.jsonl");
        let loc = format!("line:{i}");
        insert_occurrence(
            &conn,
            &NewUsageOccurrence {
                source_namespace: &namespace,
                native_event_id: Some(&native_id),
                parser_version: &version,
                heuristic_key: None,
                source_file: &source_file,
                occurred_at_nanos: Some(2_000_000),
                event_id: Some(event_id),
                transcript_file_id: Some("tf-shared"),
                source_location: Some(&loc),
                canonical_fingerprint: Some("fp-canonical-replay"),
                identity_strength: Some("strong"),
                heuristic_algorithm_version: None,
                canonical_payload_digest: Some("digest-payload-1"),
            },
        )
        .unwrap();
    }

    let count = count_occurrences_for_event(&conn, event_id).unwrap();
    assert_eq!(count, 3, "event must have 3 recorded occurrences");

    let per_event = count_occurrences_by_canonical_event(&conn).unwrap();
    assert_eq!(per_event.len(), 1);
    assert_eq!(per_event[0], ("canonical-evt-replay".to_string(), 3));
}

#[test]
fn test_constraint_violations_negative_count_and_orphan_component() {
    let db = TestDb::new();
    let conn = db.open();

    let event_id = insert_event(
        &conn,
        &NewUsageEvent {
            canonical_event_id: "evt-constraint-test",
            session_id: None,
            event_timestamp: None,
            model_id: None,
            evidence_kind: "native",
            source_provenance: "test",
            parser_version: "p1",
            created_at: UtcTimestamp::from_unix_nanos(100),
        },
    )
    .unwrap();

    // Negative count
    let neg_res = conn.execute(
        "INSERT INTO usage_component (event_id, token_class, count) VALUES (?1, ?2, ?3)",
        rusqlite::params![event_id.value(), "input", -10i64],
    );
    assert!(
        neg_res.is_err(),
        "negative count must be rejected by CHECK constraint"
    );

    // Orphan component
    let orphan_event_id = EventId::new(123_456);
    let orphan_err = insert_component(
        &conn,
        &NewUsageComponent {
            event_id: orphan_event_id,
            token_class: "input",
            count: 50,
        },
    )
    .unwrap_err();

    assert!(
        orphan_err
            .to_string()
            .to_lowercase()
            .contains("foreign key"),
        "orphan component must fail foreign key constraint: {orphan_err}"
    );
}

#[test]
fn test_every_occurrence_carrying_full_identity_and_provenance_metadata() {
    let db = TestDb::new();
    let conn = db.open();

    let event_id = insert_event(
        &conn,
        &NewUsageEvent {
            canonical_event_id: "evt-meta-full",
            session_id: Some("sess-full"),
            event_timestamp: Some(UtcTimestamp::from_unix_nanos(500)),
            model_id: Some("gpt-5-6"),
            evidence_kind: "native",
            source_provenance: "codex/session.jsonl:4",
            parser_version: "codex-1",
            created_at: UtcTimestamp::from_unix_nanos(500),
        },
    )
    .unwrap();

    let namespace = SourceNamespace::new("codex");
    let version = ParserVersion::new("codex-1");

    let occ = NewUsageOccurrence {
        source_namespace: &namespace,
        native_event_id: Some("codex-msg-999"),
        parser_version: &version,
        heuristic_key: None,
        source_file: "codex/session.jsonl",
        occurred_at_nanos: Some(500),
        event_id: Some(event_id),
        transcript_file_id: Some("tf-codex-1"),
        source_location: Some("line:4"),
        canonical_fingerprint: Some("fp-sha256-full"),
        identity_strength: Some("strong"),
        heuristic_algorithm_version: Some("v1.2"),
        canonical_payload_digest: Some("digest-sha256-full"),
    };

    let occ_id = insert_occurrence(&conn, &occ).unwrap();
    let occurrences = get_occurrences_for_event(&conn, event_id).unwrap();
    assert_eq!(occurrences.len(), 1);

    let row = &occurrences[0];
    assert_eq!(row.id, occ_id);
    assert_eq!(row.source_namespace.as_str(), "codex");
    assert_eq!(row.native_event_id.as_deref(), Some("codex-msg-999"));
    assert_eq!(row.parser_version.as_str(), "codex-1");
    assert_eq!(row.source_file, "codex/session.jsonl");
    assert_eq!(row.occurred_at_nanos, Some(500));
    assert_eq!(row.event_id, Some(event_id));
    assert_eq!(row.transcript_file_id.as_deref(), Some("tf-codex-1"));
    assert_eq!(row.source_location.as_deref(), Some("line:4"));
    assert_eq!(row.canonical_fingerprint.as_deref(), Some("fp-sha256-full"));
    assert_eq!(row.identity_strength, "strong");
    assert_eq!(row.heuristic_algorithm_version.as_deref(), Some("v1.2"));
    assert_eq!(
        row.canonical_payload_digest.as_deref(),
        Some("digest-sha256-full")
    );
}

#[test]
fn test_all_three_tables_strict_with_foreign_keys_enforced() {
    let db = TestDb::new();
    let conn = db.open();

    let orphan_event_id = EventId::new(999_999);

    // Orphan component
    let comp_res = conn.execute(
        "INSERT INTO usage_component (event_id, token_class, count) VALUES (?1, ?2, ?3)",
        rusqlite::params![orphan_event_id.value(), "input", 10i64],
    );
    assert!(
        comp_res.is_err(),
        "orphan component insert must fail foreign key constraint"
    );

    // Orphan occurrence
    let occ_res = conn.execute(
        "INSERT INTO usage_occurrence (
            source_namespace,
            parser_version,
            source_file,
            event_id,
            native_event_id,
            identity_strength
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            "ns",
            "v1",
            "file.jsonl",
            orphan_event_id.value(),
            "id1",
            "strong"
        ],
    );
    assert!(
        occ_res.is_err(),
        "orphan occurrence insert must fail foreign key constraint"
    );
}
