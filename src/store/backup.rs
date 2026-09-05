//! Store-side backup capture and database verification.
//!
//! This module owns the SQLite portion of backup so SQL and connection policy
//! remain inside `store`. The archive module owns the filesystem format and
//! checksum manifest.

use std::path::Path;
use std::time::Duration;

use rusqlite::backup::Backup;

use crate::domain::time::MonotonicDuration;
use crate::error::Error;

/// One pending spool record that belonged to the captured cut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedPendingRecord {
    pub file_name: String,
    pub bytes: Vec<u8>,
}

/// Metadata read from the database snapshot itself, plus records still pending
/// after the best-effort drain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedBackupCut {
    pub schema_version: u32,
    pub ledger_generation: u64,
    pub pending_records: Vec<CapturedPendingRecord>,
    pub drain_completed: bool,
}

/// The database verification stage that rejected an archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseVerificationStage {
    Integrity,
    ForeignKeys,
}

impl DatabaseVerificationStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Integrity => "integrity",
            Self::ForeignKeys => "foreign_keys",
        }
    }
}

/// A logical database verification failure, distinct from inability to open or
/// query the archive at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseVerificationFailure {
    pub stage: DatabaseVerificationStage,
    pub detail: String,
}

/// The two independent SQLite checks that passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatabaseVerification {
    pub integrity_check: bool,
    pub foreign_key_check: bool,
}

/// Creates a consistent SQLite snapshot and captures the spool records that
/// remain pending in the same cut. Spool mutations are excluded only across
/// drain, database backup and pending-file reads.
pub fn capture_backup_cut(
    state_dir: &Path,
    source_database: &Path,
    destination_database: &Path,
    busy_timeout: MonotonicDuration,
) -> Result<CapturedBackupCut, Error> {
    if !source_database.is_file() {
        return Err(Error::Store(format!(
            "cannot back up missing ledger database {source_database:?}"
        )));
    }

    let policy = crate::store::connection::PragmaPolicy { busy_timeout };
    let mut source = crate::store::connection::open(
        source_database,
        crate::store::connection::AccessMode::ReadWrite,
        &policy,
    )?;
    let barrier = crate::store::spool::acquire_state_snapshot_barrier(state_dir)?;

    let drain_completed =
        crate::store::spool::drain_pending_while_snapshot_barrier_held(&mut source, state_dir)
            .is_ok();

    let mut destination = crate::store::connection::open(
        destination_database,
        crate::store::connection::AccessMode::ReadWrite,
        &policy,
    )?;
    let backup = Backup::new(&source, &mut destination)
        .map_err(|error| Error::Store(format!("cannot start SQLite backup: {error}")))?;
    backup
        .run_to_completion(32, Duration::from_millis(1), None)
        .map_err(|error| Error::Store(format!("cannot complete SQLite backup: {error}")))?;
    drop(backup);

    let schema_version = crate::store::migrate::recorded_schema_version(&destination)?;
    let ledger_generation = crate::store::ledger_generation::current(&destination)?.value();
    destination
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .map_err(|error| Error::Store(format!("cannot checkpoint backup database: {error}")))?;
    drop(destination);
    crate::store::startup::force_file_mode_0600(destination_database)?;

    let pending_records = crate::store::spool::snapshot_pending_records(state_dir, &barrier)?
        .into_iter()
        .map(|record| CapturedPendingRecord {
            file_name: record.file_name,
            bytes: record.bytes,
        })
        .collect();

    Ok(CapturedBackupCut {
        schema_version,
        ledger_generation,
        pending_records,
        drain_completed,
    })
}

/// Opens the archived database read-only, then runs the two checks in the order
/// required by the recovery design. A foreign-key violation cannot be hidden by
/// a successful integrity check because the second query is independent.
pub fn verify_database(
    database: &Path,
    busy_timeout: MonotonicDuration,
) -> Result<Result<DatabaseVerification, DatabaseVerificationFailure>, Error> {
    let connection = crate::store::connection::open(
        database,
        crate::store::connection::AccessMode::ReadOnly,
        &crate::store::connection::PragmaPolicy { busy_timeout },
    )?;
    verify_database_on_connection(&connection)
}

/// The two checks against an already-open connection: the same queries
/// [`verify_database`] runs, for the caller that holds the connection it just
/// wrote through. A read-only reopen of a database carrying a live WAL
/// sidecar is the case this split exists to avoid, not an optimisation.
pub fn verify_database_on_connection(
    connection: &rusqlite::Connection,
) -> Result<Result<DatabaseVerification, DatabaseVerificationFailure>, Error> {
    let mut integrity = connection
        .prepare("PRAGMA integrity_check")
        .map_err(|error| Error::Store(format!("cannot prepare backup integrity check: {error}")))?;
    let rows = integrity
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| Error::Store(format!("cannot run backup integrity check: {error}")))?;
    let messages = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| Error::Store(format!("cannot read backup integrity result: {error}")))?;
    if messages.as_slice() != ["ok"] {
        return Ok(Err(DatabaseVerificationFailure {
            stage: DatabaseVerificationStage::Integrity,
            detail: messages.join("; "),
        }));
    }
    drop(integrity);

    let mut foreign_keys = connection
        .prepare("PRAGMA foreign_key_check")
        .map_err(|error| Error::Store(format!("cannot prepare foreign-key check: {error}")))?;
    let violations = foreign_keys
        .query_map([], |row| {
            Ok(format!(
                "table={} rowid={} parent={} constraint={}",
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?
                    .map_or_else(|| "none".to_owned(), |value| value.to_string()),
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(|error| Error::Store(format!("cannot run foreign-key check: {error}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| Error::Store(format!("cannot read foreign-key result: {error}")))?;
    if !violations.is_empty() {
        return Ok(Err(DatabaseVerificationFailure {
            stage: DatabaseVerificationStage::ForeignKeys,
            detail: violations.join("; "),
        }));
    }

    Ok(Ok(DatabaseVerification {
        integrity_check: true,
        foreign_key_check: true,
    }))
}

/// Reads the archived database metadata for comparison with its manifest.
pub fn archived_database_metadata(
    database: &Path,
    busy_timeout: MonotonicDuration,
) -> Result<(u32, u64), Error> {
    let connection = crate::store::connection::open(
        database,
        crate::store::connection::AccessMode::ReadOnly,
        &crate::store::connection::PragmaPolicy { busy_timeout },
    )?;
    Ok((
        crate::store::migrate::recorded_schema_version(&connection)?,
        crate::store::ledger_generation::current(&connection)?.value(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::domain::time::{FakeClock, UtcTimestamp};
    use crate::domain::window::QuantizationSemantics;
    use crate::store::meter_evidence::{measurement_basis_sql, quantization_sql};
    use crate::store::spool::{PendingTerminalBundle, PendingWindow, spool_pending};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-store-backup-test-{}-{suffix}",
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

    fn migrated_db(path: &Path) {
        let policy = crate::store::connection::PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1000),
        };
        let mut conn = crate::store::connection::open(
            path,
            crate::store::connection::AccessMode::ReadWrite,
            &policy,
        )
        .unwrap();
        crate::store::migrate::run_migrations(
            &mut conn,
            &crate::store::migrations::registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .unwrap();
    }

    /// A structurally valid bundle whose account and attempt do not exist,
    /// so the first insert of the drain hits a foreign-key constraint and
    /// the whole drain call returns `Err` rather than quarantining a
    /// malformed record.
    fn undrainable_bundle(attempt_id: i64) -> PendingTerminalBundle {
        PendingTerminalBundle {
            attempt_id,
            completed_at_nanos: 2_000,
            elapsed_nanos: 1_000,
            outcome: "success".into(),
            failure_class: None,
            retry_after_nanos: None,
            sanitized_error_classification: None,
            retry_index: None,
            clock_anomaly: false,
            response_classification: "success".into(),
            received_at_nanos: 1_000,
            provider_observed_at_original: Some("2026-09-02T00:00:00Z".into()),
            evidence_capsule: "{\"sanitized\":true}".into(),
            capsule_schema_version: "v1".into(),
            sanitizer_version: "v1".into(),
            capture_truncated: false,
            account_id: 1,
            provider: "anthropic".into(),
            provider_observed_at_nanos: Some(900),
            measurement_basis: measurement_basis_sql::as_sql(
                crate::domain::time::MeasurementBasis::ProviderObserved,
            )
            .to_owned(),
            observed_plan: Some("max".into()),
            observed_tier: None,
            adapter_version: "adapter-v1".into(),
            provider_contract_id: "contract-v1".into(),
            meter_semantics_id: "semantics-v1".into(),
            normalized_fingerprint: "fp-1".into(),
            windows: vec![PendingWindow {
                semantic_key: "five_hour".into(),
                scope_kind: "account_wide".into(),
                scoped_model: None,
                quota_used_ppm: 250_000,
                reported_resolution_ppm: 10_000,
                quantization: quantization_sql::as_sql(QuantizationSemantics::Exact).to_owned(),
                resets_at_nanos: Some(5_000),
                nominal_duration_nanos: 18_000_000_000_000,
                is_active: true,
                severity: "unknown".into(),
            }],
        }
    }

    #[test]
    fn a_pending_record_survives_into_the_cut_when_the_drain_cannot_complete() {
        let scratch = ScratchDir::new();
        let source_database = scratch.path().join("ledger.db");
        migrated_db(&source_database);
        let bundle = undrainable_bundle(1);
        spool_pending(scratch.path(), &bundle).unwrap();

        let destination_database = scratch.path().join("backup-ledger.db");
        let cut = capture_backup_cut(
            scratch.path(),
            &source_database,
            &destination_database,
            MonotonicDuration::from_millis(1000),
        )
        .unwrap();

        assert!(
            !cut.drain_completed,
            "the drain must fail outright against the unseeded ledger"
        );
        assert_eq!(cut.pending_records.len(), 1);
        assert_eq!(cut.pending_records[0].file_name, "attempt-1.json");
        assert_eq!(cut.pending_records[0].bytes, bundle.to_json().into_bytes());
    }
}
