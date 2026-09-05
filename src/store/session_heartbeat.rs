//! The `session_heartbeat` table: contemporary liveness evidence for a session
//! (`aub-mgv.5`, PLAN.md 19.2, 43 Workflow 2; liveness mechanism decided on `aub-mgv.6`:
//! option B, turn-end and throttled post-tool heartbeats).
//!
//! A heartbeat answers "is this session still there," a question a
//! `session_account_marker` cannot answer by itself: a marker names which account a
//! session ran under and applies forward with no expiry, so an abandoned session with
//! no closing marker would otherwise read as active indefinitely. The two facts stay
//! separate typed evidence with separate provenance; neither substitutes for the
//! other.
//!
//! Unlike a marker, a heartbeat's history has no ongoing value once a fresher one
//! exists: only the most recent matters to the freshness-horizon policy that reads it
//! back. So this table keeps at most one row per session and advances it in place,
//! never regressing behind an already-recorded later heartbeat (an out-of-order
//! arrival is simply ignored).
//!
//! Retention: disposable and outside the irreplaceable evidence chain, the same
//! standing `sampling_lease` has (`docs/store/retention.rs`): recreatable empty with
//! no loss to historical account attribution.

use rusqlite::{OptionalExtension, Row, params};

use crate::domain::ids::{NativeSessionId, SessionId, SourceNamespace};
use crate::domain::time::UtcTimestamp;
use crate::error::Error;

/// A heartbeat row's identity: its SQLite rowid. Stable across updates, because
/// `record_heartbeat` advances the existing row in place rather than replacing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HeartbeatId(i64);

impl HeartbeatId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

/// The most recent liveness evidence recorded for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHeartbeat {
    id: HeartbeatId,
    session_id: SessionId,
    last_heartbeat_at: UtcTimestamp,
    heartbeat_source: String,
}

impl SessionHeartbeat {
    pub fn id(&self) -> HeartbeatId {
        self.id
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn last_heartbeat_at(&self) -> UtcTimestamp {
        self.last_heartbeat_at
    }

    pub fn heartbeat_source(&self) -> &str {
        &self.heartbeat_source
    }
}

fn row_to_heartbeat(row: &Row<'_>) -> rusqlite::Result<SessionHeartbeat> {
    let id: i64 = row.get(0)?;
    let session_source: String = row.get(1)?;
    let session_native: String = row.get(2)?;
    let last_heartbeat_at_nanos: i64 = row.get(3)?;
    let heartbeat_source: String = row.get(4)?;

    Ok(SessionHeartbeat {
        id: HeartbeatId::new(id),
        session_id: SessionId::new(
            SourceNamespace::new(session_source),
            NativeSessionId::new(session_native),
        ),
        last_heartbeat_at: UtcTimestamp::from_unix_nanos(last_heartbeat_at_nanos),
        heartbeat_source,
    })
}

/// Records a heartbeat for `session_id`, observed at `observed_at`.
///
/// A fresh session gets its first row; an existing session's `last_heartbeat_at`
/// advances only when `observed_at` is at least as recent as what is already
/// stored, so a heartbeat that arrives out of order (a throttled hook racing a
/// faster one) cannot regress the session behind evidence already recorded.
/// `heartbeat_source` moves together with whichever timestamp wins, so it never
/// names a hook that did not actually produce the stored instant.
pub fn record_heartbeat(
    conn: &rusqlite::Connection,
    session_id: &SessionId,
    observed_at: UtcTimestamp,
    heartbeat_source: &str,
) -> Result<HeartbeatId, Error> {
    if heartbeat_source.is_empty() {
        return Err(Error::Store(
            "heartbeat source must be non-empty".to_string(),
        ));
    }
    conn.query_row(
        "INSERT INTO session_heartbeat (
            session_source, session_native, last_heartbeat_at, heartbeat_source
        ) VALUES (?1, ?2, ?3, ?4)
        ON CONFLICT (session_source, session_native) DO UPDATE SET
            last_heartbeat_at = MAX(session_heartbeat.last_heartbeat_at, excluded.last_heartbeat_at),
            heartbeat_source = CASE
                WHEN excluded.last_heartbeat_at >= session_heartbeat.last_heartbeat_at
                    THEN excluded.heartbeat_source
                ELSE session_heartbeat.heartbeat_source
            END
        RETURNING id",
        params![
            session_id.source().as_str(),
            session_id.native().as_str(),
            observed_at.unix_nanos(),
            heartbeat_source,
        ],
        |row| row.get(0),
    )
    .map(HeartbeatId::new)
    .map_err(|e| Error::Store(format!("cannot record session heartbeat: {e}")))
}

/// Reads the most recent heartbeat for a session, or `None` if none was ever
/// recorded (including a session the hook was never wired for).
pub fn latest_heartbeat(
    conn: &rusqlite::Connection,
    session_id: &SessionId,
) -> Result<Option<SessionHeartbeat>, Error> {
    conn.query_row(
        "SELECT id, session_source, session_native, last_heartbeat_at, heartbeat_source
         FROM session_heartbeat WHERE session_source = ?1 AND session_native = ?2",
        params![session_id.source().as_str(), session_id.native().as_str()],
        row_to_heartbeat,
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot read heartbeat for session: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::FakeClock;
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-store-heartbeat-test-{}-{suffix}",
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
        let db_path = scratch.path().join("heartbeat.db");
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

    fn session(native: &str) -> SessionId {
        SessionId::new(
            SourceNamespace::new("claude-code"),
            NativeSessionId::new(native),
        )
    }

    #[test]
    fn a_session_with_no_recorded_heartbeat_reads_as_none() {
        let (_scratch, conn) = fixture_conn();
        assert_eq!(
            latest_heartbeat(&conn, &session("sess-unknown")).unwrap(),
            None
        );
    }

    #[test]
    fn first_heartbeat_is_recorded_and_read_back() {
        let (_scratch, conn) = fixture_conn();
        let sess = session("sess-a");
        let id = record_heartbeat(
            &conn,
            &sess,
            UtcTimestamp::from_unix_nanos(1000),
            "turn_end",
        )
        .unwrap();
        let read = latest_heartbeat(&conn, &sess).unwrap().unwrap();
        assert_eq!(read.id(), id);
        assert_eq!(
            read.last_heartbeat_at(),
            UtcTimestamp::from_unix_nanos(1000)
        );
        assert_eq!(read.heartbeat_source(), "turn_end");
    }

    #[test]
    fn a_later_heartbeat_advances_the_same_row_in_place() {
        let (_scratch, conn) = fixture_conn();
        let sess = session("sess-b");
        let first_id = record_heartbeat(
            &conn,
            &sess,
            UtcTimestamp::from_unix_nanos(1000),
            "turn_end",
        )
        .unwrap();
        let second_id = record_heartbeat(
            &conn,
            &sess,
            UtcTimestamp::from_unix_nanos(5000),
            "post_tool",
        )
        .unwrap();
        assert_eq!(
            first_id, second_id,
            "the row id stays stable across heartbeats for the same session"
        );
        let read = latest_heartbeat(&conn, &sess).unwrap().unwrap();
        assert_eq!(
            read.last_heartbeat_at(),
            UtcTimestamp::from_unix_nanos(5000)
        );
        assert_eq!(read.heartbeat_source(), "post_tool");
    }

    #[test]
    fn an_out_of_order_heartbeat_never_regresses_the_stored_instant() {
        // Planted negative: a naive "always overwrite" implementation would let
        // a throttled, slow-to-arrive heartbeat move a fresher session backward.
        let (_scratch, conn) = fixture_conn();
        let sess = session("sess-c");
        record_heartbeat(
            &conn,
            &sess,
            UtcTimestamp::from_unix_nanos(9000),
            "post_tool",
        )
        .unwrap();
        record_heartbeat(
            &conn,
            &sess,
            UtcTimestamp::from_unix_nanos(1000),
            "turn_end",
        )
        .unwrap();
        let read = latest_heartbeat(&conn, &sess).unwrap().unwrap();
        assert_eq!(
            read.last_heartbeat_at(),
            UtcTimestamp::from_unix_nanos(9000),
            "an older heartbeat arriving late must not move the stored instant backward"
        );
        assert_eq!(
            read.heartbeat_source(),
            "post_tool",
            "the source travels with whichever timestamp actually won"
        );
    }

    #[test]
    fn two_identical_native_session_ids_from_different_sources_are_distinct_rows() {
        let (_scratch, conn) = fixture_conn();
        let claude = SessionId::new(
            SourceNamespace::new("claude-code"),
            NativeSessionId::new("sess-100"),
        );
        let codex = SessionId::new(
            SourceNamespace::new("codex"),
            NativeSessionId::new("sess-100"),
        );
        record_heartbeat(
            &conn,
            &claude,
            UtcTimestamp::from_unix_nanos(1000),
            "turn_end",
        )
        .unwrap();
        record_heartbeat(
            &conn,
            &codex,
            UtcTimestamp::from_unix_nanos(2000),
            "turn_end",
        )
        .unwrap();

        assert_eq!(
            latest_heartbeat(&conn, &claude)
                .unwrap()
                .unwrap()
                .last_heartbeat_at(),
            UtcTimestamp::from_unix_nanos(1000)
        );
        assert_eq!(
            latest_heartbeat(&conn, &codex)
                .unwrap()
                .unwrap()
                .last_heartbeat_at(),
            UtcTimestamp::from_unix_nanos(2000)
        );
    }

    #[test]
    fn empty_heartbeat_source_is_rejected() {
        let (_scratch, conn) = fixture_conn();
        let err = record_heartbeat(
            &conn,
            &session("sess-d"),
            UtcTimestamp::from_unix_nanos(1),
            "",
        )
        .unwrap_err();
        assert!(err.to_string().contains("non-empty"));
    }
}
