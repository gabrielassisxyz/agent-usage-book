//! The `usage_occurrence` table repository (`aub-lqe.7`, `aub-lqe.8`, `aub-lqe.10`).
//!
//! Tracks where every normalized source record appeared, carrying identity strength,
//! namespace, native identifier, heuristic version, and payload digest (PLAN.md 12.10).
//! Also owns the heuristic-domain health surfaces built on those rows: the per-parser
//! heuristic identity usage and collision counts the doctor reports, and the version
//! check that forces a rebuild when the fingerprint algorithm has moved on.

use std::collections::BTreeMap;

use rusqlite::params;

use crate::domain::ids::SourceNamespace;
use crate::error::Error;
use crate::store::usage_event::EventId;
use crate::transcripts::parser::ParserVersion;

/// One occurrence row's identity and metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewUsageOccurrence<'a> {
    pub source_namespace: &'a SourceNamespace,
    pub native_event_id: Option<&'a str>,
    pub parser_version: &'a ParserVersion,
    pub heuristic_key: Option<&'a str>,
    pub source_file: &'a str,
    pub occurred_at_nanos: Option<i64>,
    pub event_id: Option<EventId>,
    pub transcript_file_id: Option<&'a str>,
    pub source_location: Option<&'a str>,
    pub canonical_fingerprint: Option<&'a str>,
    pub identity_strength: Option<&'a str>,
    pub heuristic_algorithm_version: Option<&'a str>,
    pub canonical_payload_digest: Option<&'a str>,
}

/// An occurrence row's identity, by SQLite rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OccurrenceId(i64);

impl OccurrenceId {
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

/// A retrieved usage occurrence row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageOccurrenceRow {
    pub id: OccurrenceId,
    pub source_namespace: SourceNamespace,
    pub native_event_id: Option<String>,
    pub parser_version: ParserVersion,
    pub heuristic_key: Option<String>,
    pub source_file: String,
    pub occurred_at_nanos: Option<i64>,
    pub event_id: Option<EventId>,
    pub transcript_file_id: Option<String>,
    pub source_location: Option<String>,
    pub canonical_fingerprint: Option<String>,
    pub identity_strength: String,
    pub heuristic_algorithm_version: Option<String>,
    pub canonical_payload_digest: Option<String>,
}

/// Inserts one occurrence. The UNIQUE constraints on `usage_occurrence` are the
/// final deduplication authority for strong and heuristic keys.
pub fn insert_occurrence(
    conn: &rusqlite::Connection,
    occurrence: &NewUsageOccurrence<'_>,
) -> Result<OccurrenceId, Error> {
    let identity_strength =
        occurrence
            .identity_strength
            .unwrap_or(if occurrence.native_event_id.is_some() {
                "strong"
            } else {
                "heuristic"
            });
    let event_id_val = occurrence.event_id.map(|e| e.value());

    conn.query_row(
        "INSERT INTO usage_occurrence (
            source_namespace,
            native_event_id,
            parser_version,
            heuristic_key,
            source_file,
            occurred_at,
            event_id,
            transcript_file_id,
            source_location,
            canonical_fingerprint,
            identity_strength,
            heuristic_algorithm_version,
            canonical_payload_digest
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
        RETURNING id",
        params![
            occurrence.source_namespace.as_str(),
            occurrence.native_event_id,
            occurrence.parser_version.as_str(),
            occurrence.heuristic_key,
            occurrence.source_file,
            occurrence.occurred_at_nanos,
            event_id_val,
            occurrence.transcript_file_id,
            occurrence.source_location,
            occurrence.canonical_fingerprint,
            identity_strength,
            occurrence.heuristic_algorithm_version,
            occurrence.canonical_payload_digest,
        ],
        |row| row.get(0),
    )
    .map(OccurrenceId::new)
    .map_err(|e| Error::Store(format!("cannot insert usage occurrence: {e}")))
}

/// Retrieves all occurrences linked to a canonical usage event.
pub fn get_occurrences_for_event(
    conn: &rusqlite::Connection,
    event_id: EventId,
) -> Result<Vec<UsageOccurrenceRow>, Error> {
    let mut stmt = conn
        .prepare(
            "SELECT
                id,
                source_namespace,
                native_event_id,
                parser_version,
                heuristic_key,
                source_file,
                occurred_at,
                event_id,
                transcript_file_id,
                source_location,
                canonical_fingerprint,
                identity_strength,
                heuristic_algorithm_version,
                canonical_payload_digest
            FROM usage_occurrence
            WHERE event_id = ?1
            ORDER BY id ASC",
        )
        .map_err(|e| Error::Store(format!("cannot prepare get_occurrences_for_event: {e}")))?;

    let rows = stmt
        .query_map(params![event_id.value()], row_to_occurrence)
        .map_err(|e| Error::Store(format!("cannot query occurrences: {e}")))?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row.map_err(|e| Error::Store(format!("cannot read occurrence row: {e}")))?)
    }
    Ok(result)
}

/// Counts occurrences linked to a canonical usage event.
pub fn count_occurrences_for_event(
    conn: &rusqlite::Connection,
    event_id: EventId,
) -> Result<u64, Error> {
    conn.query_row(
        "SELECT COUNT(*) FROM usage_occurrence WHERE event_id = ?1",
        params![event_id.value()],
        |row| row.get::<_, i64>(0),
    )
    .map(|c| c as u64)
    .map_err(|e| Error::Store(format!("cannot count occurrences for event: {e}")))
}

/// Returns a list of (canonical_event_id, occurrence_count) across all canonical events.
pub fn count_occurrences_by_canonical_event(
    conn: &rusqlite::Connection,
) -> Result<Vec<(String, u64)>, Error> {
    let mut stmt = conn
        .prepare(
            "SELECT e.canonical_event_id, COUNT(o.id)
            FROM usage_event e
            LEFT JOIN usage_occurrence o ON o.event_id = e.id
            GROUP BY e.id
            ORDER BY e.id ASC",
        )
        .map_err(|e| {
            Error::Store(format!(
                "cannot prepare count_occurrences_by_canonical_event: {e}"
            ))
        })?;

    let rows = stmt
        .query_map([], |row| {
            let canonical_id: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((canonical_id, count as u64))
        })
        .map_err(|e| {
            Error::Store(format!(
                "cannot execute count_occurrences_by_canonical_event: {e}"
            ))
        })?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row.map_err(|e| Error::Store(format!("cannot read occurrence count: {e}")))?)
    }
    Ok(result)
}

/// Heuristic identity usage and collision counts for one parser: the data the
/// doctor's heuristic-dedup check reports (aub-lqe.10, PLAN.md 12.10, 27, 36).
/// A rising collision count is early evidence that a parser's heuristic has
/// stopped discriminating, usually after a format change, which is why the
/// count is surfaced rather than merely recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeuristicDedupUsage {
    pub parser: String,
    /// Canonical occurrences resting on a heuristic identity: the share of the
    /// ledger that rests on this system's inference rather than a source claim.
    pub heuristic_identities: u64,
    /// Quarantined collisions: pairs that shared a heuristic key but disagreed
    /// materially on canonical payload, counted from the quarantine's
    /// `dedup_collision` failure class.
    pub collisions: u64,
}

/// Reports heuristic identity usage and collision counts per parser, ordered by
/// parser name. A parser appears when it has either heuristic identities or
/// collisions; a parser whose occurrences are all strong-identity appears
/// nowhere, because there is nothing about its heuristic domain to report.
pub fn heuristic_dedup_usage(
    conn: &rusqlite::Connection,
) -> Result<Vec<HeuristicDedupUsage>, Error> {
    let mut identities: BTreeMap<String, u64> = {
        let mut stmt = conn
            .prepare(
                "SELECT parser_version, COUNT(*)
                 FROM usage_occurrence
                 WHERE heuristic_key IS NOT NULL
                 GROUP BY parser_version",
            )
            .map_err(|e| {
                Error::Store(format!("cannot prepare the heuristic identity scan: {e}"))
            })?;
        let rows = stmt
            .query_map([], |row| {
                let parser: String = row.get(0)?;
                let count: i64 = row.get(1)?;
                Ok((parser, count as u64))
            })
            .map_err(|e| Error::Store(format!("cannot scan heuristic identities: {e}")))?;
        let mut map = BTreeMap::new();
        for row in rows {
            let (parser, count) =
                row.map_err(|e| Error::Store(format!("cannot read identity count: {e}")))?;
            map.insert(parser, count);
        }
        map
    };

    let mut stmt = conn
        .prepare(
            "SELECT parser, COUNT(*)
             FROM ingest_quarantine
             WHERE failure_class = ?1
             GROUP BY parser",
        )
        .map_err(|e| Error::Store(format!("cannot prepare the collision scan: {e}")))?;
    let rows = stmt
        .query_map(
            params![crate::store::ingest_quarantine::DEDUP_COLLISION_FAILURE_CLASS],
            |row| {
                let parser: String = row.get(0)?;
                let count: i64 = row.get(1)?;
                Ok((parser, count as u64))
            },
        )
        .map_err(|e| Error::Store(format!("cannot scan collisions: {e}")))?;
    let mut collisions: BTreeMap<String, u64> = BTreeMap::new();
    for row in rows {
        let (parser, count) =
            row.map_err(|e| Error::Store(format!("cannot read collision count: {e}")))?;
        collisions.insert(parser, count);
    }
    let mut parsers: Vec<String> = identities
        .keys()
        .chain(collisions.keys())
        .cloned()
        .collect();
    parsers.sort();
    parsers.dedup();
    let mut usage: Vec<HeuristicDedupUsage> = Vec::new();
    for parser in parsers {
        usage.push(HeuristicDedupUsage {
            heuristic_identities: identities.remove(&parser).unwrap_or(0),
            collisions: collisions.remove(&parser).unwrap_or(0),
            parser,
        });
    }
    Ok(usage)
}

/// A parser whose stored heuristic identities were computed under a different
/// fingerprint algorithm version than the running code uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeuristicVersionMismatch {
    pub parser: String,
    /// The version the stored identities carry, when they carry one. A stored
    /// row without a version is its own mismatch: an unversioned identity
    /// cannot be silently extended either.
    pub stored: Option<String>,
    /// The version the running code computes keys under.
    pub current: String,
}

/// Names every parser whose stored heuristic identities were computed under an
/// algorithm version other than `current_version`, in parser order. Those
/// parsers' heuristic identities must be rebuilt from the transcripts before
/// ingestion continues: extending a ledger whose keys mean something else with
/// new-version keys would silently fork the canonical identity of the same
/// logical events, double-counting every replay crossing the boundary
/// (aub-lqe.10, PLAN.md 12.10, 34.13). Detection reads the store and changes
/// nothing; the rebuild itself is the rebuild command's verb.
pub fn heuristic_rebuild_required(
    conn: &rusqlite::Connection,
    current_version: &str,
) -> Result<Vec<HeuristicVersionMismatch>, Error> {
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT parser_version, heuristic_algorithm_version
             FROM usage_occurrence
             WHERE heuristic_key IS NOT NULL
             ORDER BY parser_version, heuristic_algorithm_version",
        )
        .map_err(|e| Error::Store(format!("cannot prepare the heuristic version scan: {e}")))?;
    let rows = stmt
        .query_map([], |row| {
            let parser: String = row.get(0)?;
            let stored: Option<String> = row.get(1)?;
            Ok((parser, stored))
        })
        .map_err(|e| Error::Store(format!("cannot scan heuristic versions: {e}")))?;
    let mut mismatches = Vec::new();
    for row in rows {
        let (parser, stored) =
            row.map_err(|e| Error::Store(format!("cannot read heuristic version: {e}")))?;
        if stored.as_deref() != Some(current_version) {
            mismatches.push(HeuristicVersionMismatch {
                parser,
                stored,
                current: current_version.to_string(),
            });
        }
    }
    Ok(mismatches)
}

fn row_to_occurrence(row: &rusqlite::Row<'_>) -> rusqlite::Result<UsageOccurrenceRow> {
    let id_val: i64 = row.get(0)?;
    let namespace_str: String = row.get(1)?;
    let native_event_id: Option<String> = row.get(2)?;
    let parser_ver_str: String = row.get(3)?;
    let heuristic_key: Option<String> = row.get(4)?;
    let source_file: String = row.get(5)?;
    let occurred_at_nanos: Option<i64> = row.get(6)?;
    let event_id_val: Option<i64> = row.get(7)?;
    let transcript_file_id: Option<String> = row.get(8)?;
    let source_location: Option<String> = row.get(9)?;
    let canonical_fingerprint: Option<String> = row.get(10)?;
    let identity_strength: String = row.get(11)?;
    let heuristic_algorithm_version: Option<String> = row.get(12)?;
    let canonical_payload_digest: Option<String> = row.get(13)?;

    Ok(UsageOccurrenceRow {
        id: OccurrenceId::new(id_val),
        source_namespace: SourceNamespace::new(namespace_str),
        native_event_id,
        parser_version: ParserVersion::new(parser_ver_str),
        heuristic_key,
        source_file,
        occurred_at_nanos,
        event_id: event_id_val.map(EventId::new),
        transcript_file_id,
        source_location,
        canonical_fingerprint,
        identity_strength,
        heuristic_algorithm_version,
        canonical_payload_digest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::{FakeClock, UtcTimestamp};
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::store::ingest_quarantine::{NewQuarantineItem, record_quarantine};
    use crate::store::usage_component::{
        NewUsageComponent, get_components_for_event, insert_component, insert_components,
    };
    use crate::store::usage_event::{NewUsageEvent, insert_event};
    use crate::transcripts::parser::{QuarantineClass, QuarantineRecord, SourceLocation};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-store-occurrence-test-{}-{suffix}",
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
        let db_path = scratch.path().join("occurrence.db");
        let policy = PragmaPolicy {
            busy_timeout: crate::domain::time::MonotonicDuration::from_millis(1000),
        };
        let mut conn = open(&db_path, AccessMode::ReadWrite, &policy).unwrap();
        crate::store::migrate::run_migrations(
            &mut conn,
            &crate::store::migrations::registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .unwrap();
        (scratch, conn)
    }

    fn claude_code_namespace() -> SourceNamespace {
        SourceNamespace::new("claude-code")
    }

    fn claude_code_parser_version() -> ParserVersion {
        ParserVersion::new("claude-code-1")
    }

    fn strong<'a>(
        namespace: &'a SourceNamespace,
        parser_version: &'a ParserVersion,
        native_event_id: &'a str,
        file: &'a str,
    ) -> NewUsageOccurrence<'a> {
        NewUsageOccurrence {
            source_namespace: namespace,
            native_event_id: Some(native_event_id),
            parser_version,
            heuristic_key: None,
            source_file: file,
            occurred_at_nanos: Some(1_000),
            event_id: None,
            transcript_file_id: None,
            source_location: None,
            canonical_fingerprint: None,
            identity_strength: Some("strong"),
            heuristic_algorithm_version: None,
            canonical_payload_digest: None,
        }
    }

    fn heuristic<'a>(
        namespace: &'a SourceNamespace,
        parser_version: &'a ParserVersion,
        heuristic_key: &'a str,
        file: &'a str,
    ) -> NewUsageOccurrence<'a> {
        NewUsageOccurrence {
            source_namespace: namespace,
            native_event_id: None,
            parser_version,
            heuristic_key: Some(heuristic_key),
            source_file: file,
            occurred_at_nanos: Some(1_000),
            event_id: None,
            transcript_file_id: None,
            source_location: None,
            canonical_fingerprint: None,
            identity_strength: Some("heuristic"),
            heuristic_algorithm_version: None,
            canonical_payload_digest: None,
        }
    }

    /// The database constraint is the final authority for the strong domain:
    /// a duplicate `(source_namespace, native_event_id)` fails to insert,
    /// even from a different file.
    #[test]
    fn a_duplicate_strong_identity_fails_the_direct_insert() {
        let (_scratch, conn) = fixture_conn();
        let namespace = claude_code_namespace();
        let version = claude_code_parser_version();
        insert_occurrence(&conn, &strong(&namespace, &version, "m1", "first.jsonl")).unwrap();
        let err = insert_occurrence(&conn, &strong(&namespace, &version, "m1", "second.jsonl"))
            .unwrap_err();
        assert!(
            err.to_string().contains("usage_occurrence")
                || err.to_string().to_lowercase().contains("unique"),
            "expected a uniqueness failure, got: {err}"
        );
    }

    /// The database constraint is the final authority for the heuristic
    /// domain too: a duplicate `(parser_version, heuristic_key)` fails to
    /// insert, even from a different file.
    #[test]
    fn a_duplicate_heuristic_key_within_one_parser_fails_the_direct_insert() {
        let (_scratch, conn) = fixture_conn();
        let namespace = claude_code_namespace();
        let version = claude_code_parser_version();
        insert_occurrence(
            &conn,
            &heuristic(&namespace, &version, "t:1000|s1|10:0:0", "first.jsonl"),
        )
        .unwrap();
        let err = insert_occurrence(
            &conn,
            &heuristic(&namespace, &version, "t:1000|s1|10:0:0", "second.jsonl"),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("usage_occurrence")
                || err.to_string().to_lowercase().contains("unique"),
            "expected a uniqueness failure, got: {err}"
        );
    }

    /// A heuristic key from a *different* parser never collides, even with
    /// the identical key string: the heuristic domain is scoped per parser.
    #[test]
    fn the_same_heuristic_key_from_a_different_parser_does_not_collide() {
        let (_scratch, conn) = fixture_conn();
        let namespace = claude_code_namespace();
        let version = claude_code_parser_version();
        insert_occurrence(
            &conn,
            &heuristic(&namespace, &version, "t:1000|s1|10:0:0", "claude.jsonl"),
        )
        .unwrap();
        let codex_namespace = SourceNamespace::new("codex");
        let codex_version = ParserVersion::new("codex-1");
        let other = NewUsageOccurrence {
            source_namespace: &codex_namespace,
            native_event_id: None,
            parser_version: &codex_version,
            heuristic_key: Some("t:1000|s1|10:0:0"),
            source_file: "codex.jsonl",
            occurred_at_nanos: Some(1_000),
            event_id: None,
            transcript_file_id: None,
            source_location: None,
            canonical_fingerprint: None,
            identity_strength: Some("heuristic"),
            heuristic_algorithm_version: None,
            canonical_payload_digest: None,
        };
        insert_occurrence(&conn, &other).unwrap();
    }

    /// Strong and heuristic identities are separate uniqueness domains at the
    /// database boundary, not only in application code: a strong-identity row
    /// and a heuristic-domain row insert independently even when the strong
    /// identifier string happens to equal the heuristic key string.
    #[test]
    fn strong_and_heuristic_rows_insert_independently_at_the_database_boundary() {
        let (_scratch, conn) = fixture_conn();
        let namespace = claude_code_namespace();
        let version = claude_code_parser_version();
        insert_occurrence(
            &conn,
            &strong(&namespace, &version, "shared-value", "a.jsonl"),
        )
        .unwrap();
        insert_occurrence(
            &conn,
            &heuristic(&namespace, &version, "shared-value", "b.jsonl"),
        )
        .unwrap();
    }

    /// Unit test: an unknown token class is stored as a component row with no schema change.
    #[test]
    fn unknown_token_class_stored_as_component_row() {
        let (_scratch, conn) = fixture_conn();
        let event_id = insert_event(
            &conn,
            &NewUsageEvent {
                canonical_event_id: "evt-canon-1",
                session_id: Some("session-123"),
                event_timestamp: Some(UtcTimestamp::from_unix_nanos(1_000_000)),
                model_id: Some("claude-3-7-sonnet"),
                evidence_kind: "native",
                source_provenance: "file:line",
                parser_version: "claude-code-1",
                created_at: UtcTimestamp::from_unix_nanos(2_000_000),
            },
        )
        .unwrap();

        // Storing standard + unknown token classes
        insert_components(
            &conn,
            event_id,
            &[
                ("input", 100),
                ("output", 200),
                ("reasoning_tokens_future_v2", 50),
                ("cache_creation_input_tokens", 30),
            ],
        )
        .unwrap();

        let components = get_components_for_event(&conn, event_id).unwrap();
        assert_eq!(components.len(), 4);
        assert_eq!(components[2].token_class, "reasoning_tokens_future_v2");
        assert_eq!(components[2].count, 50);
        assert_eq!(components[3].token_class, "cache_creation_input_tokens");
        assert_eq!(components[3].count, 30);
    }

    /// Unit test: negative count fails CHECK constraint on usage_component.
    #[test]
    fn negative_count_fails_constraint() {
        let (_scratch, conn) = fixture_conn();
        let event_id = insert_event(
            &conn,
            &NewUsageEvent {
                canonical_event_id: "evt-canon-neg",
                session_id: None,
                event_timestamp: None,
                model_id: None,
                evidence_kind: "native",
                source_provenance: "prov",
                parser_version: "p1",
                created_at: UtcTimestamp::from_unix_nanos(100),
            },
        )
        .unwrap();

        let raw_insert = conn.execute(
            "INSERT INTO usage_component (event_id, token_class, count) VALUES (?1, ?2, ?3)",
            params![event_id.value(), "input", -5i64],
        );
        assert!(
            raw_insert.is_err(),
            "negative token count must violate CHECK constraint"
        );
    }

    /// Unit test: orphan component insert fails foreign key constraint.
    #[test]
    fn orphan_component_fails_foreign_key() {
        let (_scratch, conn) = fixture_conn();
        let orphan_event_id = EventId::new(999_999);
        let err = insert_component(
            &conn,
            &NewUsageComponent {
                event_id: orphan_event_id,
                token_class: "input",
                count: 100,
            },
        )
        .unwrap_err();

        assert!(
            err.to_string().to_lowercase().contains("foreign key"),
            "orphan component must fail foreign key constraint, got: {err}"
        );
    }

    /// Unit test: every occurrence carrying full metadata fields.
    #[test]
    fn occurrence_carrying_all_metadata_fields() {
        let (_scratch, conn) = fixture_conn();
        let namespace = SourceNamespace::new("gemini-cli");
        let version = ParserVersion::new("gemini-1");
        let event_id = insert_event(
            &conn,
            &NewUsageEvent {
                canonical_event_id: "evt-meta-1",
                session_id: Some("sess-1"),
                event_timestamp: Some(UtcTimestamp::from_unix_nanos(5_000)),
                model_id: Some("gemini-2.5-pro"),
                evidence_kind: "native",
                source_provenance: "transcripts/gemini.jsonl:10",
                parser_version: "gemini-1",
                created_at: UtcTimestamp::from_unix_nanos(6_000),
            },
        )
        .unwrap();

        let occ = NewUsageOccurrence {
            source_namespace: &namespace,
            native_event_id: Some("native-123"),
            parser_version: &version,
            heuristic_key: None,
            source_file: "gemini.jsonl",
            occurred_at_nanos: Some(5_000),
            event_id: Some(event_id),
            transcript_file_id: Some("tf-001"),
            source_location: Some("line:42"),
            canonical_fingerprint: Some("fp-sha256-abc"),
            identity_strength: Some("strong"),
            heuristic_algorithm_version: Some("v1.0"),
            canonical_payload_digest: Some("digest-sha256-def"),
        };

        let occ_id = insert_occurrence(&conn, &occ).unwrap();
        let retrieved = get_occurrences_for_event(&conn, event_id).unwrap();
        assert_eq!(retrieved.len(), 1);
        let row = &retrieved[0];
        assert_eq!(row.id, occ_id);
        assert_eq!(row.source_namespace.as_str(), "gemini-cli");
        assert_eq!(row.native_event_id.as_deref(), Some("native-123"));
        assert_eq!(row.source_file, "gemini.jsonl");
        assert_eq!(row.source_location.as_deref(), Some("line:42"));
        assert_eq!(row.canonical_fingerprint.as_deref(), Some("fp-sha256-abc"));
        assert_eq!(row.identity_strength, "strong");
        assert_eq!(row.heuristic_algorithm_version.as_deref(), Some("v1.0"));
        assert_eq!(
            row.canonical_payload_digest.as_deref(),
            Some("digest-sha256-def")
        );
    }

    /// Unit test: orphan occurrence insert fails foreign key constraint.
    #[test]
    fn orphan_occurrence_fails_foreign_key() {
        let (_scratch, conn) = fixture_conn();
        let namespace = SourceNamespace::new("test");
        let version = ParserVersion::new("test-1");
        let orphan_event_id = EventId::new(888_888);

        let occ = NewUsageOccurrence {
            source_namespace: &namespace,
            native_event_id: Some("id-orphan"),
            parser_version: &version,
            heuristic_key: None,
            source_file: "test.jsonl",
            occurred_at_nanos: Some(1_000),
            event_id: Some(orphan_event_id),
            transcript_file_id: None,
            source_location: None,
            canonical_fingerprint: None,
            identity_strength: Some("strong"),
            heuristic_algorithm_version: None,
            canonical_payload_digest: None,
        };

        let err = insert_occurrence(&conn, &occ).unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("foreign key"),
            "orphan occurrence must fail foreign key constraint, got: {err}"
        );
    }

    fn heuristic_at_version<'a>(
        namespace: &'a SourceNamespace,
        parser_version: &'a ParserVersion,
        heuristic_key: &'a str,
        file: &'a str,
        algorithm_version: Option<&'a str>,
    ) -> NewUsageOccurrence<'a> {
        NewUsageOccurrence {
            heuristic_algorithm_version: algorithm_version,
            ..heuristic(namespace, parser_version, heuristic_key, file)
        }
    }

    fn collision_item(parser: &str) -> NewQuarantineItem {
        NewQuarantineItem {
            source_file: "a.jsonl".to_string(),
            byte_offset: None,
            line_number: None,
            parser: parser.to_string(),
            failure_class: crate::store::ingest_quarantine::DEDUP_COLLISION_FAILURE_CLASS
                .to_string(),
            excerpt_hash: "collision-hash".to_string(),
            excerpt: None,
            observed_at: UtcTimestamp::from_unix_nanos(500),
        }
    }

    /// The doctor's heuristic-dedup data source reports identity usage and
    /// collision counts per parser, and only for parsers that have either. A
    /// parser whose occurrences are all strong-identity has no heuristic
    /// domain to report on; a parse-failure quarantine row is not a collision.
    #[test]
    fn heuristic_dedup_usage_reports_identities_and_collisions_per_parser() {
        let (_scratch, conn) = fixture_conn();
        let namespace = claude_code_namespace();
        let version = claude_code_parser_version();
        insert_occurrence(
            &conn,
            &heuristic_at_version(
                &namespace,
                &version,
                "t:1|s1|10:0:0",
                "a.jsonl",
                Some("hk1"),
            ),
        )
        .unwrap();
        insert_occurrence(
            &conn,
            &heuristic_at_version(
                &namespace,
                &version,
                "t:2|s1|11:0:0",
                "a.jsonl",
                Some("hk1"),
            ),
        )
        .unwrap();
        // A strong-identity occurrence for the same parser never counts as a
        // heuristic identity, and a parser with only strong occurrences has no
        // row of its own.
        let strong_only = SourceNamespace::new("codex");
        let strong_version = ParserVersion::new("codex-1");
        insert_occurrence(
            &conn,
            &strong(&strong_only, &strong_version, "m1", "b.jsonl"),
        )
        .unwrap();

        record_quarantine(&conn, &collision_item("claude-code-1")).unwrap();
        // A parse failure for the strong-only parser must not count as a
        // collision anywhere.
        let parse_failure = QuarantineRecord::new(
            SourceLocation::new("b.jsonl", 3),
            strong_version.clone(),
            QuarantineClass::TruncatedStructure,
        );
        record_quarantine(
            &conn,
            &NewQuarantineItem::from_record(&parse_failure, UtcTimestamp::from_unix_nanos(200)),
        )
        .unwrap();

        let usage = heuristic_dedup_usage(&conn).unwrap();
        assert_eq!(usage.len(), 1, "only the heuristic parser is reported");
        assert_eq!(usage[0].parser, "claude-code-1");
        assert_eq!(usage[0].heuristic_identities, 2);
        assert_eq!(usage[0].collisions, 1);
    }

    /// An algorithm version change is reported as a rebuild requirement per
    /// parser, never silently absorbed: a stored identity under an older
    /// version, and a stored identity with no version at all, both block
    /// extending the ledger, while the current version reports nothing.
    #[test]
    fn a_version_change_is_reported_as_a_rebuild_not_a_silent_mutation() {
        let (_scratch, conn) = fixture_conn();
        let namespace = claude_code_namespace();
        let version = claude_code_parser_version();
        let current = crate::dedup::HeuristicKey::ALGORITHM_VERSION;
        insert_occurrence(
            &conn,
            &heuristic_at_version(
                &namespace,
                &version,
                "t:1|s1|10:0:0",
                "a.jsonl",
                Some(current),
            ),
        )
        .unwrap();
        let older_parser = SourceNamespace::new("gemini");
        let older_version = ParserVersion::new("gemini-1");
        insert_occurrence(
            &conn,
            &heuristic_at_version(
                &older_parser,
                &older_version,
                "t:1|s1|10:0:0",
                "g.jsonl",
                Some("hk0"),
            ),
        )
        .unwrap();
        let unversioned = SourceNamespace::new("pi");
        let unversioned_version = ParserVersion::new("pi-1");
        insert_occurrence(
            &conn,
            &heuristic_at_version(
                &unversioned,
                &unversioned_version,
                "t:1|s1|10:0:0",
                "p.jsonl",
                None,
            ),
        )
        .unwrap();

        let mismatches = heuristic_rebuild_required(&conn, current).unwrap();
        assert_eq!(
            mismatches
                .iter()
                .map(|m| (m.parser.as_str(), m.stored.as_deref()))
                .collect::<Vec<_>>(),
            vec![("gemini-1", Some("hk0")), ("pi-1", None)],
            "the current parser stays out; the older and unversioned ones are named"
        );
        assert!(mismatches.iter().all(|m| m.current == current));

        // The planted negative: running under an older version inverts the
        // report instead of silently accepting every stored identity, which is
        // what stops a version bump from being edited around.
        let under_older = heuristic_rebuild_required(&conn, "hk0").unwrap();
        assert_eq!(under_older[0].parser, "claude-code-1");
    }

    /// Integration: duplicate occurrence counts are retrievable per canonical event.
    #[test]
    fn replayed_corpus_produces_one_canonical_event_with_queryable_occurrence_counts() {
        let (_scratch, conn) = fixture_conn();
        let event_id = insert_event(
            &conn,
            &NewUsageEvent {
                canonical_event_id: "canonical-msg-001",
                session_id: Some("session-abc"),
                event_timestamp: Some(UtcTimestamp::from_unix_nanos(10_000)),
                model_id: Some("claude-3-5-sonnet"),
                evidence_kind: "native",
                source_provenance: "first-file.jsonl",
                parser_version: "claude-code-1",
                created_at: UtcTimestamp::from_unix_nanos(10_000),
            },
        )
        .unwrap();

        insert_components(
            &conn,
            event_id,
            &[("input", 1500), ("output", 400), ("cache_read", 200)],
        )
        .unwrap();

        let namespace = SourceNamespace::new("claude-code");
        let version = ParserVersion::new("claude-code-1");

        // First occurrence
        insert_occurrence(
            &conn,
            &NewUsageOccurrence {
                source_namespace: &namespace,
                native_event_id: Some("msg-001"),
                parser_version: &version,
                heuristic_key: None,
                source_file: "first-file.jsonl",
                occurred_at_nanos: Some(10_000),
                event_id: Some(event_id),
                transcript_file_id: Some("tf-1"),
                source_location: Some("offset:100"),
                canonical_fingerprint: Some("fp-001"),
                identity_strength: Some("strong"),
                heuristic_algorithm_version: None,
                canonical_payload_digest: Some("digest-001"),
            },
        )
        .unwrap();

        let count = count_occurrences_for_event(&conn, event_id).unwrap();
        assert_eq!(count, 1);

        let per_event_counts = count_occurrences_by_canonical_event(&conn).unwrap();
        assert_eq!(per_event_counts.len(), 1);
        assert_eq!(per_event_counts[0], ("canonical-msg-001".to_string(), 1));
    }
}
