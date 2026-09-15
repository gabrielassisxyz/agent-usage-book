//! Session-id resolution and session-to-file lookup behind
//! `aub export transcript` (`aub-xpfl`).
//!
//! A transcript export starts from a short string the operator typed, not from
//! a path: this module resolves that string against the `session` table (full
//! or prefix, case-insensitive, across every harness unless narrowed) and
//! lists every distinct `usage_occurrence.source_file` the resolved session's
//! events were ingested from, so the caller reads the transcripts from disk
//! rather than re-discovering them. Rendering lives in `presentation`; this
//! module only answers which session and which files.
//!
//! May not depend on:
//! - HTTP or terminal-formatting crates
//! - presentation
//! - provider adapters

use rusqlite::{Connection, params};

use crate::domain::ids::{NativeSessionId, SourceNamespace};
use crate::domain::time::UtcTimestamp;
use crate::error::Error;
use crate::sessions::resolver::ProjectKey;

/// One session row the transcript export may render: the namespaced identity
/// the resolution matched, when it started, and the logical project key the
/// heading and the default output name are derived from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptSession {
    /// The harness namespace, e.g. `claude-code`.
    pub source: SourceNamespace,
    /// The full native session id, e.g. a UUID.
    pub native_session_id: NativeSessionId,
    /// The session start, which orders candidates for `--latest`.
    pub start: UtcTimestamp,
    /// The logical project key, e.g. `unknown-project` when unmapped.
    pub project_key: ProjectKey,
}

/// Resolves `<id>` against `session.native_session_id`, exactly or as a
/// prefix, case-insensitively, across every harness unless `harness` narrows
/// the match to one source. Results come back in `(start, source, native
/// session id)` order, so the last row is the most recent session and
/// `--latest` is deterministic even when two sessions share a start instant.
/// An empty id is a usage error: an empty prefix would match every session.
pub fn resolve_transcript_sessions(
    conn: &Connection,
    id_prefix: &str,
    harness: Option<&str>,
) -> Result<Vec<TranscriptSession>, Error> {
    let trimmed = id_prefix.trim();
    if trimmed.is_empty() {
        return Err(Error::Usage(
            "export transcript requires a session id or id prefix".into(),
        ));
    }
    let pattern = format!("{}%", escape_like_pattern(&trimmed.to_lowercase()));
    let sql = match harness {
        Some(_) => {
            "SELECT source, native_session_id, start, project_key FROM session \
             WHERE lower(native_session_id) LIKE ?1 ESCAPE '\\' AND source = ?2 \
             ORDER BY start ASC, source ASC, native_session_id ASC"
        }
        None => {
            "SELECT source, native_session_id, start, project_key FROM session \
             WHERE lower(native_session_id) LIKE ?1 ESCAPE '\\' \
             ORDER BY start ASC, source ASC, native_session_id ASC"
        }
    };
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| Error::Store(format!("cannot prepare the session id lookup: {e}")))?;
    let rows = match harness {
        Some(source) => stmt
            .query_map(params![pattern, source], transcript_session_from_row)
            .map_err(|e| Error::Store(format!("cannot run the session id lookup: {e}")))?,
        None => stmt
            .query_map(params![pattern], transcript_session_from_row)
            .map_err(|e| Error::Store(format!("cannot run the session id lookup: {e}")))?,
    };
    let mut sessions = Vec::new();
    for row in rows {
        sessions
            .push(row.map_err(|e| Error::Store(format!("cannot read a session id match: {e}")))?);
    }
    Ok(sessions)
}

/// Escapes the three characters `LIKE` treats specially, so an id prefix
/// containing `%` or `_` matches itself rather than acting as a wildcard.
fn escape_like_pattern(prefix: &str) -> String {
    let mut escaped = String::with_capacity(prefix.len());
    for ch in prefix.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

fn transcript_session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TranscriptSession> {
    let source: String = row.get(0)?;
    let native_session_id: String = row.get(1)?;
    let start_nanos: i64 = row.get(2)?;
    let project_key: String = row.get(3)?;
    Ok(TranscriptSession {
        source: SourceNamespace::new(source),
        native_session_id: NativeSessionId::new(native_session_id),
        start: UtcTimestamp::from_unix_nanos(start_nanos),
        project_key: ProjectKey::new(project_key),
    })
}

/// Every distinct transcript file the session's events were ingested from, in
/// path order: the `source_file` values the ingest recorded on
/// `usage_occurrence` for events carrying this session's native id under this
/// session's own namespace. The namespace join matters: two harnesses may
/// hold textually identical native ids, and only this session's files may be
/// rendered. Files are read from disk by the caller at export time; a path
/// that has since moved is still listed here, and the caller reports it.
pub fn transcript_source_files(
    conn: &Connection,
    session: &TranscriptSession,
) -> Result<Vec<String>, Error> {
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT o.source_file FROM usage_occurrence o \
             JOIN usage_event e ON e.id = o.event_id \
             WHERE e.session_id = ?1 AND o.source_namespace = ?2 \
             ORDER BY o.source_file ASC",
        )
        .map_err(|e| Error::Store(format!("cannot prepare the session file lookup: {e}")))?;
    let rows = stmt
        .query_map(
            params![session.native_session_id.as_str(), session.source.as_str()],
            |row| row.get::<_, String>(0),
        )
        .map_err(|e| Error::Store(format!("cannot run the session file lookup: {e}")))?;
    let mut files = Vec::new();
    for row in rows {
        files.push(row.map_err(|e| Error::Store(format!("cannot read a session file row: {e}")))?);
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::MonotonicDuration;
    use crate::store::connection::PragmaPolicy;
    use crate::store::usage_event::{NewUsageEvent, insert_event};
    use crate::store::usage_occurrence::{NewUsageOccurrence, insert_occurrence};
    use crate::transcripts::parser::ParserVersion;

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-store-transcript-test-{}-{suffix}",
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

    fn conn() -> (ScratchDir, Connection) {
        let scratch = ScratchDir::new();
        let policy = PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1000),
        };
        let conn = crate::store::test_schema::open_migrated(
            &scratch.path().join("transcript.db"),
            &policy,
        );
        (scratch, conn)
    }

    fn seed_session(conn: &Connection, source: &str, native: &str, project: &str, start: i64) {
        conn.execute(
            "INSERT INTO session (source, native_session_id, run_id, project_key, repository_key, start, end)
             VALUES (?1, ?2, NULL, ?3, 'repo-x', ?4, ?5)",
            rusqlite::params![source, native, project, start, start + 50],
        )
        .expect("session insert");
    }

    /// The bead's fixture ledger: `aaaa0001…` (claude-code), `aaaa0002…`
    /// (codex), `01a0318b-1…` and `01a0318b-2…` (codex), the later of each
    /// pair starting later.
    fn seed_resolution_fixture(conn: &Connection) {
        seed_session(
            conn,
            "claude-code",
            "aaaa0001-1111-4222-8333-444444444444",
            "proj-alpha",
            100,
        );
        seed_session(
            conn,
            "codex",
            "aaaa0002-1111-7222-9333-444444444444",
            "proj-beta",
            200,
        );
        seed_session(conn, "codex", "01a0318b-1aaa", "proj-gamma", 300);
        seed_session(conn, "codex", "01a0318b-2bbb", "proj-gamma", 400);
    }

    #[test]
    fn a_full_prefix_resolves_to_exactly_one_session() {
        let (_s, conn) = conn();
        seed_resolution_fixture(&conn);
        let hits = resolve_transcript_sessions(&conn, "aaaa0001", None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].native_session_id.as_str(),
            "aaaa0001-1111-4222-8333-444444444444"
        );
        assert_eq!(hits[0].source.as_str(), "claude-code");
    }

    #[test]
    fn a_shared_prefix_matches_every_session_case_insensitively() {
        let (_s, conn) = conn();
        seed_resolution_fixture(&conn);
        let hits = resolve_transcript_sessions(&conn, "aaaa", None).unwrap();
        assert_eq!(hits.len(), 2, "both aaaa sessions match");
        assert_eq!(hits[0].start.unix_nanos(), 100);
        assert_eq!(hits[1].start.unix_nanos(), 200);
        // The near-identical negative: uppercase matches the same rows,
        // because the match is case-insensitive.
        let upper = resolve_transcript_sessions(&conn, "AAAA", None).unwrap();
        assert_eq!(upper, hits);
    }

    #[test]
    fn the_timestamp_prefix_pair_matches_twice_and_orders_by_start() {
        let (_s, conn) = conn();
        seed_resolution_fixture(&conn);
        let hits = resolve_transcript_sessions(&conn, "01a0318b", None).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(
            hits[0].native_session_id.as_str().ends_with("1aaa")
                && hits[1].native_session_id.as_str().ends_with("2bbb"),
            "start order puts the earlier session first: {hits:?}"
        );
    }

    #[test]
    fn a_harness_narrows_the_match_and_an_unknown_id_matches_nothing() {
        let (_s, conn) = conn();
        seed_resolution_fixture(&conn);
        let narrowed = resolve_transcript_sessions(&conn, "aaaa", Some("codex")).unwrap();
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].source.as_str(), "codex");
        // The narrowed query orders by start too, so `--latest` on the
        // `01a0318b` codex pair picks the later session.
        let pair = resolve_transcript_sessions(&conn, "01a0318b", Some("codex")).unwrap();
        assert_eq!(pair.len(), 2);
        assert_eq!(pair[1].native_session_id.as_str(), "01a0318b-2bbb");
        let missing = resolve_transcript_sessions(&conn, "zzzz", None).unwrap();
        assert!(missing.is_empty());
        let missing_narrowed = resolve_transcript_sessions(&conn, "aaaa", Some("pi")).unwrap();
        assert!(missing_narrowed.is_empty());
    }

    #[test]
    fn a_like_wildcard_in_the_prefix_matches_itself_not_everything() {
        let (_s, conn) = conn();
        seed_resolution_fixture(&conn);
        // Without escaping, `%` would match every session row.
        let hits = resolve_transcript_sessions(&conn, "aaaa%", None).unwrap();
        assert!(
            hits.is_empty(),
            "no native id literally contains a percent sign: {hits:?}"
        );
        let underscore = resolve_transcript_sessions(&conn, "aaaa_001", None).unwrap();
        assert!(
            underscore.is_empty(),
            "an underscore matches itself, not any character: {underscore:?}"
        );
    }

    #[test]
    fn an_empty_prefix_is_a_usage_error_not_a_match_all() {
        let (_s, conn) = conn();
        seed_resolution_fixture(&conn);
        match resolve_transcript_sessions(&conn, "   ", None) {
            Err(Error::Usage(message)) => assert!(message.contains("session id")),
            other => panic!("expected a usage error, got {other:?}"),
        }
    }

    /// Seeds one event for `native` under `source`, with one occurrence
    /// pointing at `file`, so the file lookup has a row to find. Identity
    /// columns carry a per-call counter: two seeds for one file must still
    /// insert as two rows, or the uniqueness constraints would refuse the
    /// second.
    fn seed_event_with_file(conn: &Connection, source: &str, native: &str, file: &str) {
        static SEED_COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = SEED_COUNTER.fetch_add(1, Ordering::Relaxed);
        let event_id = insert_event(
            conn,
            &NewUsageEvent {
                canonical_event_id: Box::leak(format!("ce-{n}-{native}-{file}").into_boxed_str()),
                session_id: Some(Box::leak(native.to_string().into_boxed_str())),
                event_timestamp: Some(UtcTimestamp::from_unix_nanos(100)),
                model_id: None,
                evidence_kind: "transcript",
                source_provenance: Box::leak(file.to_string().into_boxed_str()),
                parser_version: "v1",
                created_at: UtcTimestamp::from_unix_nanos(100),
            },
        )
        .unwrap();
        let namespace = SourceNamespace::new(source);
        let version = ParserVersion::new("v1");
        insert_occurrence(
            conn,
            &NewUsageOccurrence {
                source_namespace: &namespace,
                native_event_id: Some(Box::leak(format!("ne-{n}-{native}").into_boxed_str())),
                parser_version: &version,
                heuristic_key: None,
                source_file: Box::leak(file.to_string().into_boxed_str()),
                occurred_at_nanos: Some(100),
                event_id: Some(event_id),
                transcript_file_id: None,
                source_location: None,
                canonical_fingerprint: None,
                identity_strength: Some("strong"),
                heuristic_algorithm_version: None,
                canonical_payload_digest: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn files_come_back_distinct_in_path_order_for_the_session_only() {
        let (_s, conn) = conn();
        seed_resolution_fixture(&conn);
        seed_event_with_file(
            &conn,
            "claude-code",
            "aaaa0001-1111-4222-8333-444444444444",
            "/tmp/t/b.jsonl",
        );
        seed_event_with_file(
            &conn,
            "claude-code",
            "aaaa0001-1111-4222-8333-444444444444",
            "/tmp/t/a.jsonl",
        );
        // The same file twice still lists once.
        seed_event_with_file(
            &conn,
            "claude-code",
            "aaaa0001-1111-4222-8333-444444444444",
            "/tmp/t/a.jsonl",
        );
        // The textually identical native id under another harness belongs
        // to another session: its files must not leak into this one.
        seed_event_with_file(
            &conn,
            "codex",
            "aaaa0001-1111-4222-8333-444444444444",
            "/tmp/t/leak.jsonl",
        );
        let session = resolve_transcript_sessions(&conn, "aaaa0001", None).unwrap();
        assert_eq!(session.len(), 1);
        let files = transcript_source_files(&conn, &session[0]).unwrap();
        assert_eq!(files, vec!["/tmp/t/a.jsonl", "/tmp/t/b.jsonl"]);
    }

    #[test]
    fn a_session_without_events_lists_no_files() {
        let (_s, conn) = conn();
        seed_resolution_fixture(&conn);
        let session = resolve_transcript_sessions(&conn, "aaaa0002", None).unwrap();
        assert_eq!(session.len(), 1);
        assert!(
            transcript_source_files(&conn, &session[0])
                .unwrap()
                .is_empty()
        );
    }
}
