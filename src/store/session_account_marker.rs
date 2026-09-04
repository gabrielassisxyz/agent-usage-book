//! The `session_account_marker` table: durable session-to-account markers (PLAN.md 6, 11.5, 12.6, 19.2, 32).
//!
//! Markers record which account a session was running under. They are irreplaceable
//! evidence: hook invocations and launcher markers cannot be reconstructed after the fact.
//!
//! Retention policy (PLAN.md 11.5): Forever.
//! Markers are append-only in ordinary operation and are never automatically pruned.
//! Explicit purge requires an affirmative authorization token with a recorded reason and authorizer.

use std::fmt;
use std::str::FromStr;

use rusqlite::{OptionalExtension, Row, params};

use crate::domain::ids::{NativeRunId, NativeSessionId, RunId, SessionId, SourceNamespace};
use crate::domain::time::UtcTimestamp;
use crate::error::Error;
use crate::store::account::AccountId;

/// The retention class for session account markers (PLAN.md 11.5: Forever).
pub const RETENTION_CLASS: &str = "forever";

/// An account marker row's identity: its SQLite rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MarkerId(i64);

impl MarkerId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

/// A sequence number or ordering key provided by the marker's source tool.
///
/// Used to order markers that share a single timestamp due to timestamp resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceOrderingKey(i64);

impl SourceOrderingKey {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

/// The originating source that emitted the marker (e.g., "launcher", "hook", "import").
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MarkerSource(String);

impl MarkerSource {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Account evidence ranking designation (PLAN.md 19.2).
///
/// Ordered by evidence strength:
/// 1. Explicit session/account marker from launcher or hook
/// 2. Explicit provider/account identity returned during that session
/// 3. Configured credential-source identity with validated mapping
/// 4. Conservative temporal inference
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EvidenceDesignation {
    /// Explicit session/account marker from launcher or hook (Rank 1).
    ExplicitLauncherOrHook = 1,
    /// Explicit provider/account identity returned during that session (Rank 2).
    ExplicitProviderIdentity = 2,
    /// Configured credential-source identity with validated mapping (Rank 3).
    ConfiguredCredentialMapping = 3,
    /// Conservative temporal inference (Rank 4).
    ConservativeTemporalInference = 4,
}

impl EvidenceDesignation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ExplicitLauncherOrHook => "launcher_or_hook",
            Self::ExplicitProviderIdentity => "provider_identity",
            Self::ConfiguredCredentialMapping => "credential_mapping",
            Self::ConservativeTemporalInference => "conservative_temporal_inference",
        }
    }
}

impl fmt::Display for EvidenceDesignation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EvidenceDesignation {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "launcher_or_hook" | "explicit_launcher_or_hook" => Ok(Self::ExplicitLauncherOrHook),
            "provider_identity" | "explicit_provider_identity" => {
                Ok(Self::ExplicitProviderIdentity)
            }
            "credential_mapping" | "configured_credential_mapping" => {
                Ok(Self::ConfiguredCredentialMapping)
            }
            "conservative_temporal_inference" | "temporal_inference" | "inferred" => {
                Ok(Self::ConservativeTemporalInference)
            }
            other => Err(Error::Store(format!(
                "unknown evidence designation: '{other}'"
            ))),
        }
    }
}

impl From<EvidenceDesignation> for crate::attribution::AccountEvidenceClass {
    fn from(designation: EvidenceDesignation) -> Self {
        match designation {
            EvidenceDesignation::ExplicitLauncherOrHook => Self::ExplicitLauncherOrHook,
            EvidenceDesignation::ExplicitProviderIdentity => Self::ExplicitProviderIdentity,
            EvidenceDesignation::ConfiguredCredentialMapping => Self::ConfiguredCredentialMapping,
            EvidenceDesignation::ConservativeTemporalInference => {
                Self::ConservativeTemporalInference
            }
        }
    }
}

/// A stored session-account marker row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAccountMarker {
    id: MarkerId,
    session_id: SessionId,
    observed_at: UtcTimestamp,
    source_ordering_key: Option<SourceOrderingKey>,
    logical_account: String,
    resolved_account_id: Option<AccountId>,
    marker_source: MarkerSource,
    run_id: Option<RunId>,
    evidence_designation: EvidenceDesignation,
}

impl SessionAccountMarker {
    pub fn id(&self) -> MarkerId {
        self.id
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn observed_at(&self) -> UtcTimestamp {
        self.observed_at
    }

    pub fn source_ordering_key(&self) -> Option<SourceOrderingKey> {
        self.source_ordering_key
    }

    pub fn logical_account(&self) -> &str {
        &self.logical_account
    }

    pub fn resolved_account_id(&self) -> Option<AccountId> {
        self.resolved_account_id
    }

    pub fn marker_source(&self) -> &MarkerSource {
        &self.marker_source
    }

    pub fn run_id(&self) -> Option<&RunId> {
        self.run_id.as_ref()
    }

    pub fn evidence_designation(&self) -> EvidenceDesignation {
        self.evidence_designation
    }
}

/// Parameters for creating a new session-account marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSessionAccountMarker {
    pub session_id: SessionId,
    pub observed_at: UtcTimestamp,
    pub source_ordering_key: Option<SourceOrderingKey>,
    pub logical_account: String,
    pub resolved_account_id: Option<AccountId>,
    pub marker_source: MarkerSource,
    pub run_id: Option<RunId>,
    pub evidence_designation: EvidenceDesignation,
}

/// Explicit authorization required to purge markers for an exact session.
///
/// Ordinary repository paths expose no update or delete. Purging is a maintenance
/// operation that requires an affirmative authorization object with a recorded reason
/// and authorizer identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeAuthorization {
    reason: String,
    authorized_by: String,
}

impl PurgeAuthorization {
    pub fn new(reason: impl Into<String>, authorized_by: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            authorized_by: authorized_by.into(),
        }
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub fn authorized_by(&self) -> &str {
        &self.authorized_by
    }
}

fn row_to_marker(row: &Row<'_>) -> rusqlite::Result<SessionAccountMarker> {
    let id: i64 = row.get(0)?;
    let session_source: String = row.get(1)?;
    let session_native: String = row.get(2)?;
    let observed_at_nanos: i64 = row.get(3)?;
    let source_ordering_key: Option<i64> = row.get(4)?;
    let logical_account: String = row.get(5)?;
    let resolved_account_id: Option<i64> = row.get(6)?;
    let marker_source: String = row.get(7)?;
    let run_source: Option<String> = row.get(8)?;
    let run_native: Option<String> = row.get(9)?;
    let evidence_designation_str: String = row.get(10)?;

    let evidence_designation =
        EvidenceDesignation::from_str(&evidence_designation_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(10, rusqlite::types::Type::Text, Box::new(e))
        })?;

    let session_id = SessionId::new(
        SourceNamespace::new(session_source),
        NativeSessionId::new(session_native),
    );

    let run_id = match (run_source, run_native) {
        (Some(src), Some(nat)) => {
            Some(RunId::new(SourceNamespace::new(src), NativeRunId::new(nat)))
        }
        _ => None,
    };

    Ok(SessionAccountMarker {
        id: MarkerId::new(id),
        session_id,
        observed_at: UtcTimestamp::from_unix_nanos(observed_at_nanos),
        source_ordering_key: source_ordering_key.map(SourceOrderingKey::new),
        logical_account,
        resolved_account_id: resolved_account_id.map(AccountId::new),
        marker_source: MarkerSource::new(marker_source),
        run_id,
        evidence_designation,
    })
}

/// Appends a new session account marker.
///
/// Markers are append-only: this insert is unconditional and produces a new row.
pub fn insert_marker(
    conn: &rusqlite::Connection,
    marker: &NewSessionAccountMarker,
) -> Result<MarkerId, Error> {
    let run_source = marker.run_id.as_ref().map(|r| r.source().as_str());
    let run_native = marker.run_id.as_ref().map(|r| r.native().as_str());

    conn.query_row(
        "INSERT INTO session_account_marker (
            session_source,
            session_native,
            observed_at,
            source_ordering_key,
            logical_account,
            resolved_account_id,
            marker_source,
            run_source,
            run_native,
            evidence_designation
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
        RETURNING id",
        params![
            marker.session_id.source().as_str(),
            marker.session_id.native().as_str(),
            marker.observed_at.unix_nanos(),
            marker.source_ordering_key.map(|k| k.value()),
            marker.logical_account,
            marker.resolved_account_id.map(|a| a.value()),
            marker.marker_source.as_str(),
            run_source,
            run_native,
            marker.evidence_designation.as_str(),
        ],
        |row| row.get(0),
    )
    .map(MarkerId::new)
    .map_err(|e| Error::Store(format!("cannot insert session account marker: {e}")))
}

/// Reads one marker by id, or `None` if no such marker exists.
pub fn marker_by_id(
    conn: &rusqlite::Connection,
    id: MarkerId,
) -> Result<Option<SessionAccountMarker>, Error> {
    conn.query_row(
        "SELECT id, session_source, session_native, observed_at, source_ordering_key,
                logical_account, resolved_account_id, marker_source, run_source, run_native,
                evidence_designation
         FROM session_account_marker WHERE id = ?1",
        params![id.value()],
        row_to_marker,
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot read marker {}: {e}", id.value())))
}

/// Reads all markers for a namespaced session ID, ordered by timestamp, source ordering key, and row ID.
///
/// Order is total and deterministic across re-reads.
pub fn markers_for_session(
    conn: &rusqlite::Connection,
    session_id: &SessionId,
) -> Result<Vec<SessionAccountMarker>, Error> {
    let mut stmt = conn
        .prepare(
            "SELECT id, session_source, session_native, observed_at, source_ordering_key,
                    logical_account, resolved_account_id, marker_source, run_source, run_native,
                    evidence_designation
             FROM session_account_marker
             WHERE session_source = ?1 AND session_native = ?2
             ORDER BY observed_at ASC,
                      source_ordering_key ASC,
                      id ASC",
        )
        .map_err(|e| Error::Store(format!("cannot prepare session markers query: {e}")))?;

    let rows = stmt
        .query_map(
            params![session_id.source().as_str(), session_id.native().as_str()],
            row_to_marker,
        )
        .map_err(|e| Error::Store(format!("cannot query session markers: {e}")))?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row.map_err(|e| Error::Store(format!("cannot read marker row: {e}")))?);
    }
    Ok(result)
}

/// Reads all markers in the database in deterministic order.
pub fn all_markers(conn: &rusqlite::Connection) -> Result<Vec<SessionAccountMarker>, Error> {
    let mut stmt = conn
        .prepare(
            "SELECT id, session_source, session_native, observed_at, source_ordering_key,
                    logical_account, resolved_account_id, marker_source, run_source, run_native,
                    evidence_designation
             FROM session_account_marker
             ORDER BY observed_at ASC,
                      source_ordering_key ASC,
                      id ASC",
        )
        .map_err(|e| Error::Store(format!("cannot prepare all markers query: {e}")))?;

    let rows = stmt
        .query_map([], row_to_marker)
        .map_err(|e| Error::Store(format!("cannot query all markers: {e}")))?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row.map_err(|e| Error::Store(format!("cannot read marker row: {e}")))?);
    }
    Ok(result)
}

/// Explicitly purges markers for an exact session target with affirmative authorization.
///
/// Returns the count of purged rows. Ordinary repository paths expose no delete.
pub fn purge_markers_for_session(
    conn: &rusqlite::Connection,
    session_id: &SessionId,
    auth: &PurgeAuthorization,
) -> Result<usize, Error> {
    if auth.reason().trim().is_empty() || auth.authorized_by().trim().is_empty() {
        return Err(Error::Store(
            "purge requires affirmative authorization with non-empty reason and authorizer".into(),
        ));
    }
    conn.execute(
        "DELETE FROM session_account_marker WHERE session_source = ?1 AND session_native = ?2",
        params![session_id.source().as_str(), session_id.native().as_str()],
    )
    .map_err(|e| Error::Store(format!("cannot purge markers for session: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::FakeClock;
    use crate::store::account::observe_account;
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-store-marker-test-{}-{suffix}",
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
        let db_path = scratch.path().join("marker.db");
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

    #[test]
    fn two_identical_native_session_ids_from_different_sources_are_distinct_rows() {
        let (_scratch, conn) = fixture_conn();
        let session_a = SessionId::new(
            SourceNamespace::new("claude-code"),
            NativeSessionId::new("sess-100"),
        );
        let session_b = SessionId::new(
            SourceNamespace::new("codex"),
            NativeSessionId::new("sess-100"),
        );

        let marker_a = NewSessionAccountMarker {
            session_id: session_a.clone(),
            observed_at: UtcTimestamp::from_unix_nanos(1000),
            source_ordering_key: None,
            logical_account: "work".into(),
            resolved_account_id: None,
            marker_source: MarkerSource::new("launcher"),
            run_id: None,
            evidence_designation: EvidenceDesignation::ExplicitLauncherOrHook,
        };

        let marker_b = NewSessionAccountMarker {
            session_id: session_b.clone(),
            observed_at: UtcTimestamp::from_unix_nanos(2000),
            source_ordering_key: None,
            logical_account: "personal".into(),
            resolved_account_id: None,
            marker_source: MarkerSource::new("hook"),
            run_id: None,
            evidence_designation: EvidenceDesignation::ExplicitLauncherOrHook,
        };

        let id_a = insert_marker(&conn, &marker_a).unwrap();
        let id_b = insert_marker(&conn, &marker_b).unwrap();
        assert_ne!(id_a, id_b);

        let rows_a = markers_for_session(&conn, &session_a).unwrap();
        assert_eq!(rows_a.len(), 1);
        assert_eq!(rows_a[0].id(), id_a);
        assert_eq!(rows_a[0].logical_account(), "work");
        assert_eq!(rows_a[0].session_id(), &session_a);

        let rows_b = markers_for_session(&conn, &session_b).unwrap();
        assert_eq!(rows_b.len(), 1);
        assert_eq!(rows_b[0].id(), id_b);
        assert_eq!(rows_b[0].logical_account(), "personal");
        assert_eq!(rows_b[0].session_id(), &session_b);
    }

    #[test]
    fn two_markers_sharing_timestamp_both_retained_and_ordered_by_source_ordering_key() {
        let (_scratch, conn) = fixture_conn();
        let session = SessionId::new(
            SourceNamespace::new("claude-code"),
            NativeSessionId::new("sess-simultaneous"),
        );
        let timestamp = UtcTimestamp::from_unix_nanos(50_000);

        // Insert marker with ordering key 2 FIRST
        let marker_second_in_sequence = NewSessionAccountMarker {
            session_id: session.clone(),
            observed_at: timestamp,
            source_ordering_key: Some(SourceOrderingKey::new(2)),
            logical_account: "second-account".into(),
            resolved_account_id: None,
            marker_source: MarkerSource::new("hook"),
            run_id: None,
            evidence_designation: EvidenceDesignation::ExplicitLauncherOrHook,
        };
        let id_second = insert_marker(&conn, &marker_second_in_sequence).unwrap();

        // Insert marker with ordering key 1 SECOND
        let marker_first_in_sequence = NewSessionAccountMarker {
            session_id: session.clone(),
            observed_at: timestamp,
            source_ordering_key: Some(SourceOrderingKey::new(1)),
            logical_account: "first-account".into(),
            resolved_account_id: None,
            marker_source: MarkerSource::new("hook"),
            run_id: None,
            evidence_designation: EvidenceDesignation::ExplicitLauncherOrHook,
        };
        let id_first = insert_marker(&conn, &marker_first_in_sequence).unwrap();

        // Both are retained, and query ordering is ordered by source_ordering_key (1 before 2)
        let rows = markers_for_session(&conn, &session).unwrap();
        assert_eq!(
            rows.len(),
            2,
            "both markers sharing a timestamp must be retained"
        );
        assert_eq!(rows[0].id(), id_first);
        assert_eq!(rows[0].logical_account(), "first-account");
        assert_eq!(
            rows[0].source_ordering_key(),
            Some(SourceOrderingKey::new(1))
        );

        assert_eq!(rows[1].id(), id_second);
        assert_eq!(rows[1].logical_account(), "second-account");
        assert_eq!(
            rows[1].source_ordering_key(),
            Some(SourceOrderingKey::new(2))
        );
    }

    #[test]
    fn ordinary_paths_expose_no_update_or_delete_and_purge_requires_affirmative_authorization() {
        let (_scratch, conn) = fixture_conn();
        let session = SessionId::new(
            SourceNamespace::new("claude-code"),
            NativeSessionId::new("sess-purge-test"),
        );
        let marker = NewSessionAccountMarker {
            session_id: session.clone(),
            observed_at: UtcTimestamp::from_unix_nanos(1000),
            source_ordering_key: None,
            logical_account: "work".into(),
            resolved_account_id: None,
            marker_source: MarkerSource::new("launcher"),
            run_id: None,
            evidence_designation: EvidenceDesignation::ExplicitLauncherOrHook,
        };
        let id = insert_marker(&conn, &marker).unwrap();
        assert!(marker_by_id(&conn, id).unwrap().is_some());

        // Purge with empty reason fails
        let empty_reason_auth = PurgeAuthorization::new("", "admin");
        let err = purge_markers_for_session(&conn, &session, &empty_reason_auth).unwrap_err();
        assert!(err.to_string().contains("affirmative authorization"));

        // Purge with empty authorizer fails
        let empty_authorizer_auth = PurgeAuthorization::new("testing purge", "");
        let err = purge_markers_for_session(&conn, &session, &empty_authorizer_auth).unwrap_err();
        assert!(err.to_string().contains("affirmative authorization"));

        // Marker still exists
        assert!(marker_by_id(&conn, id).unwrap().is_some());

        // Purge with affirmative authorization succeeds
        let valid_auth =
            PurgeAuthorization::new("compliance deletion request #42", "compliance-officer");
        let purged_count = purge_markers_for_session(&conn, &session, &valid_auth).unwrap();
        assert_eq!(purged_count, 1);
        assert!(marker_by_id(&conn, id).unwrap().is_none());
    }

    #[test]
    fn marker_naming_unconfigured_account_retained_with_null_resolved_account_id() {
        let (_scratch, conn) = fixture_conn();
        let session = SessionId::new(
            SourceNamespace::new("claude-code"),
            NativeSessionId::new("sess-unconfigured"),
        );
        let marker = NewSessionAccountMarker {
            session_id: session.clone(),
            observed_at: UtcTimestamp::from_unix_nanos(10_000),
            source_ordering_key: None,
            logical_account: "unconfigured-corp-xyz".into(),
            resolved_account_id: None,
            marker_source: MarkerSource::new("hook"),
            run_id: None,
            evidence_designation: EvidenceDesignation::ExplicitLauncherOrHook,
        };
        let id = insert_marker(&conn, &marker).unwrap();
        let read = marker_by_id(&conn, id).unwrap().unwrap();
        assert_eq!(read.logical_account(), "unconfigured-corp-xyz");
        assert_eq!(read.resolved_account_id(), None);
    }

    #[test]
    fn marker_with_resolved_account_id_persists_foreign_key() {
        let (_scratch, conn) = fixture_conn();
        let account_id = observe_account(
            &conn,
            "anthropic",
            "work",
            UtcTimestamp::from_unix_nanos(1000),
        )
        .unwrap();

        let session = SessionId::new(
            SourceNamespace::new("claude-code"),
            NativeSessionId::new("sess-configured"),
        );
        let marker = NewSessionAccountMarker {
            session_id: session.clone(),
            observed_at: UtcTimestamp::from_unix_nanos(10_000),
            source_ordering_key: None,
            logical_account: "work".into(),
            resolved_account_id: Some(account_id),
            marker_source: MarkerSource::new("hook"),
            run_id: None,
            evidence_designation: EvidenceDesignation::ExplicitLauncherOrHook,
        };
        let id = insert_marker(&conn, &marker).unwrap();
        let read = marker_by_id(&conn, id).unwrap().unwrap();
        assert_eq!(read.logical_account(), "work");
        assert_eq!(read.resolved_account_id(), Some(account_id));
    }

    #[test]
    fn evidence_designation_persisted_and_read_back_unchanged() {
        let (_scratch, conn) = fixture_conn();
        let session = SessionId::new(
            SourceNamespace::new("claude-code"),
            NativeSessionId::new("sess-evidence-ranking"),
        );

        let designations = [
            EvidenceDesignation::ExplicitLauncherOrHook,
            EvidenceDesignation::ExplicitProviderIdentity,
            EvidenceDesignation::ConfiguredCredentialMapping,
        ];

        for (idx, designation) in designations.iter().enumerate() {
            let marker = NewSessionAccountMarker {
                session_id: session.clone(),
                observed_at: UtcTimestamp::from_unix_nanos((idx as i64 + 1) * 1000),
                source_ordering_key: Some(SourceOrderingKey::new(idx as i64)),
                logical_account: format!("acc-{idx}"),
                resolved_account_id: None,
                marker_source: MarkerSource::new("test"),
                run_id: None,
                evidence_designation: *designation,
            };
            let id = insert_marker(&conn, &marker).unwrap();
            let read = marker_by_id(&conn, id).unwrap().unwrap();
            assert_eq!(read.evidence_designation(), *designation);
        }
    }

    #[test]
    fn stored_order_is_total_and_stable_across_re_reads() {
        let (_scratch, conn) = fixture_conn();
        let session = SessionId::new(
            SourceNamespace::new("claude-code"),
            NativeSessionId::new("sess-ordering-property"),
        );

        // Generate markers with varying timestamps and ordering keys
        let test_data = [
            (3000, Some(1), "d"),
            (1000, Some(2), "b"),
            (1000, Some(1), "a"),
            (2000, None, "c"),
            (3000, Some(2), "e"),
        ];

        for (ts, ord, name) in test_data {
            let marker = NewSessionAccountMarker {
                session_id: session.clone(),
                observed_at: UtcTimestamp::from_unix_nanos(ts),
                source_ordering_key: ord.map(SourceOrderingKey::new),
                logical_account: name.into(),
                resolved_account_id: None,
                marker_source: MarkerSource::new("generator"),
                run_id: None,
                evidence_designation: EvidenceDesignation::ExplicitLauncherOrHook,
            };
            insert_marker(&conn, &marker).unwrap();
        }

        // Expected sorted order:
        // ts 1000 key 1 ("a")
        // ts 1000 key 2 ("b")
        // ts 2000 key None ("c")
        // ts 3000 key 1 ("d")
        // ts 3000 key 2 ("e")
        let read1 = markers_for_session(&conn, &session).unwrap();
        let read2 = markers_for_session(&conn, &session).unwrap();

        assert_eq!(read1, read2, "re-reads must be identical");
        let accounts: Vec<&str> = read1.iter().map(|m| m.logical_account()).collect();
        assert_eq!(accounts, vec!["a", "b", "c", "d", "e"]);
    }
}
