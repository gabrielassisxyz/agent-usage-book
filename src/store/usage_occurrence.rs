//! The `usage_occurrence` table's identity columns (PLAN.md 12.10, `aub-lqe.7`).
//!
//! Carries only what proves the database uniqueness constraint is the final
//! deduplication authority in two separate domains. `aub-lqe.8` adds the
//! remaining occurrence metadata once `usage_event` exists to reference.

use rusqlite::params;

use crate::domain::ids::SourceNamespace;
use crate::error::Error;
use crate::transcripts::parser::ParserVersion;

/// One occurrence row's identity, exactly as `crate::dedup` computes it: a
/// source-provided identifier (the strong domain) or a parser-scoped
/// heuristic fingerprint (the heuristic domain). `native_event_id` and
/// `heuristic_key` carry no domain newtype of their own: they are opaque
/// strings computed by `crate::dedup`, which owns their shape, and this table
/// only ever compares them for equality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewUsageOccurrence<'a> {
    pub source_namespace: &'a SourceNamespace,
    pub native_event_id: Option<&'a str>,
    pub parser_version: &'a ParserVersion,
    pub heuristic_key: Option<&'a str>,
    pub source_file: &'a str,
    pub occurred_at_nanos: Option<i64>,
}

/// An occurrence row's identity, by SQLite rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct OccurrenceId(i64);

impl OccurrenceId {
    pub const fn value(self) -> i64 {
        self.0
    }
}

/// Inserts one occurrence, unconditionally. The two UNIQUE constraints on
/// `usage_occurrence` are the final authority: a duplicate strong identity or
/// a duplicate heuristic key within its parser's domain fails here rather
/// than being caught, if at all, only in application code.
pub fn insert_occurrence(
    conn: &rusqlite::Connection,
    occurrence: &NewUsageOccurrence<'_>,
) -> Result<OccurrenceId, Error> {
    conn.query_row(
        "INSERT INTO usage_occurrence (
            source_namespace,
            native_event_id,
            parser_version,
            heuristic_key,
            source_file,
            occurred_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        RETURNING id",
        params![
            occurrence.source_namespace.as_str(),
            occurrence.native_event_id,
            occurrence.parser_version.as_str(),
            occurrence.heuristic_key,
            occurrence.source_file,
            occurrence.occurred_at_nanos,
        ],
        |row| row.get(0),
    )
    .map(OccurrenceId)
    .map_err(|e| Error::Store(format!("cannot insert usage occurrence: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::{FakeClock, UtcTimestamp};
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
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

    /// Two occurrences sharing neither a native ID nor a heuristic key insert
    /// as two independent rows, proving the constraint does not accidentally
    /// reject ordinary distinct occurrences.
    #[test]
    fn distinct_occurrences_insert_independently() {
        let (_scratch, conn) = fixture_conn();
        let namespace = claude_code_namespace();
        let version = claude_code_parser_version();
        let a = insert_occurrence(&conn, &strong(&namespace, &version, "m1", "a.jsonl")).unwrap();
        let b = insert_occurrence(&conn, &strong(&namespace, &version, "m2", "b.jsonl")).unwrap();
        assert_ne!(a, b);
    }
}
