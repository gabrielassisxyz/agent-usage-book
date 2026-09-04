//! The recovery path: restoring a verified archive into a new state directory
//! (`aub-sth.13`, PLAN.md sections 33 Phase 1, 13, 38).
//!
//! May not depend on:
//! - provider adapters
//! - presentation
//!
//! The procedure is the one PLAN.md section 38 fixes, and `docs/recovery.md`
//! documents for operators. Steps 1 and 2 (stop mutating invocations, preserve
//! the damaged state directory) are operator obligations; this module enforces
//! the second one mechanically: it writes only into the fresh destination and
//! refuses every destination that would destroy evidence, above all the
//! configured state directory itself. Steps 3 through 6 (verify the archive,
//! restore to a new directory, run integrity and migration checks, replay both
//! pending sources idempotently) are performed here in that order. Step 7
//! (rebuild the projection) is performed too, by [`projection_recovery_status`];
//! step 8 has nothing to attempt in this phase and is reported rather than
//! silently skipped: see [`TRANSCRIPT_RECOVERY`].
//!
//! The replay's idempotence is not this module's own property: it is the spool
//! drain's, keyed on the attempt identifier, so a record present in both the
//! archive and the surviving directory commits its observation once. What this
//! module adds to that is the recovery disposition for a record the ledger
//! refuses (quarantine with its reason, and keep going), because a recovery
//! that stops at the first damaged record recovers nothing behind it, and the
//! unrecovered-evidence report that names what the recovery could not bring
//! back.

use std::fs;
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::backup::{ARCHIVE_DATABASE_FILE, archived_pending_records, verify_archive};
use crate::domain::rows::RowCount;
use crate::domain::time::{Clock, MonotonicDuration};
use crate::error::Error;
use crate::store::backup::{
    DatabaseVerification, DatabaseVerificationFailure, archived_database_metadata,
    verify_database_on_connection,
};
use crate::store::connection::{AccessMode, LEDGER_DATABASE_FILE, PragmaPolicy, open};
use crate::store::meter_evidence::observation_row_count;
use crate::store::migrate::run_migrations;
use crate::store::migrations::registry;
use crate::store::spool::{RecoveryDrainReport, drain_pending_recovering};
use crate::store::startup::{
    MountTable, create_file_mode_0600, ensure_dir_mode_0700, ensure_state_dir_ready,
};

/// The archive's own name for the pending spool directory it carries.
const ARCHIVE_PENDING_DIR: &str = "pending";

/// Step 7 of the recovery procedure (PLAN.md section 38: rebuild the
/// projection). The projection is disposable by design (invariant 16): the
/// archive never carries one (`backup::create_archive` writes only the
/// database and pending evidence), so there is nothing to restore. A recovery
/// that copied the damaged directory's own projection file forward would be
/// testing the wrong behaviour, since a corrupted projection is exactly the
/// case this step exists to not reproduce; it is rebuilt deterministically
/// from the restored database's own state instead (`aub-n27.2`).
fn projection_recovery_status(publication: &crate::projection::Publication) -> RecoveryStepStatus {
    match publication {
        crate::projection::Publication::Published { .. } => RecoveryStepStatus {
            disposition: "rebuilt",
            reason: "the projection carries no archived copy; it is rebuilt deterministically \
                     from the restored database's own state rather than restored from evidence",
        },
        crate::projection::Publication::Deferred { .. } => RecoveryStepStatus {
            disposition: "deferred",
            reason: "the projection rebuild was deferred; the restored database is unaffected \
                     and the next publish heals it",
        },
    }
}

/// Step 8 of the recovery procedure (PLAN.md section 38: rebuild
/// transcript-derived tables if necessary). No transcript-derived table has a
/// writer in this phase, so the restored ledger's transcript-derived rows are
/// exactly the rows the archive carried and there is nothing to rebuild.
pub const TRANSCRIPT_RECOVERY: RecoveryStepStatus = RecoveryStepStatus {
    disposition: "not-attempted",
    reason: "no transcript-derived table has a writer in this phase",
};

/// Whether one recovery step applies to the state this binary can hold, and
/// when it does not, the stated reason it does not. A step that does not apply
/// is reported, never skipped in silence: the operator reading a restore
/// result must be able to tell "nothing to do" from "nobody looked".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryStepStatus {
    /// The machine-readable disposition, as the restore result prints it.
    pub disposition: &'static str,
    /// Why this step has nothing to do, for the person reading the result.
    pub reason: &'static str,
}

/// Where an unrecovered piece of evidence was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnrecoveredEvidenceSource {
    /// The record was pending inside the restored archive.
    Archive,
    /// The record was pending in, or already quarantined by, the surviving
    /// (damaged) directory.
    SurvivingDirectory,
}

impl UnrecoveredEvidenceSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Archive => "archive",
            Self::SurvivingDirectory => "surviving",
        }
    }
}

/// One piece of irreplaceable evidence the recovery could not put back into
/// the ledger, with the reason. The record itself is preserved in quarantine;
/// this report is what makes its loss visible instead of quiet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnrecoveredEvidence {
    pub source: UnrecoveredEvidenceSource,
    pub file_name: String,
    pub reason: String,
}

/// What one restore did, and what it could not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreSummary {
    pub archive: PathBuf,
    pub destination: PathBuf,
    pub surviving: Option<PathBuf>,
    /// The archive passed every verification stage during this restore.
    pub archive_verified: bool,
    pub schema_version: u32,
    pub ledger_generation: u64,
    /// Pending records copied out of the archive, the manifest's validated
    /// inventory rather than a directory listing.
    pub pending_restored: usize,
    /// Migrations the restored database needed; zero for an archive from the
    /// same schema era, which is the migration check passing.
    pub migrations_applied: usize,
    pub archive_replay: RecoveryDrainReport,
    pub surviving_replay: Option<RecoveryDrainReport>,
    /// Observation rows in the restored database after the replay: the number
    /// the drill asserts against the source's own count, exactly.
    pub observation_count: RowCount,
    pub unrecovered: Vec<UnrecoveredEvidence>,
    pub projection_recovery: RecoveryStepStatus,
    pub transcript_recovery: RecoveryStepStatus,
}

/// Restores `archive` into the new directory `destination`, replaying pending
/// evidence from the restored archive and, when given, from the surviving
/// (damaged) directory, and reports what could not be recovered.
///
/// The function never writes to `configured_state_dir`, to `archive`, or to a
/// `surviving` directory's ledger: the destination is created fresh, and the
/// refusals below are what makes "preserve the damaged state directory" a
/// property of the code rather than of the operator's memory.
pub fn restore_archive(
    configured_state_dir: &Path,
    archive: &Path,
    destination: &Path,
    surviving: Option<&Path>,
    busy_timeout: MonotonicDuration,
    mounts: &dyn MountTable,
    clock: &impl Clock,
) -> Result<RestoreSummary, Error> {
    refuse_evidence_destroying_destinations(configured_state_dir, archive, destination, surviving)?;
    if let Some(dir) = surviving
        && !dir.is_dir()
    {
        return Err(Error::Store(format!(
            "surviving directory {dir:?} does not exist; pass the damaged state directory that still holds pending evidence, or omit it"
        )));
    }

    // Step 3: verify the archive before anything is restored from it. An
    // unverified archive is not yet a backup, and a recovery built on one
    // would inherit whatever the checksums never checked.
    let verified = verify_archive(archive, busy_timeout, clock)?;
    let pending_records = archived_pending_records(archive)?;

    // Step 4: restore to a new directory. The destination exists only from
    // here on, and only because the refusals above held.
    ensure_state_dir_ready(destination, mounts)?;
    let destination_database = destination.join(LEDGER_DATABASE_FILE);
    copy_database_file(&archive.join(ARCHIVE_DATABASE_FILE), &destination_database)?;
    let pending_restored = copy_pending_records(archive, destination, &pending_records)?;

    let policy = PragmaPolicy { busy_timeout };
    let mut conn = open(&destination_database, AccessMode::ReadWrite, &policy)?;

    // Step 5, first half: integrity and foreign-key checks on the restored
    // copy, before the replay writes anything into it. The archive database
    // was verified in place; this proves the copy on disk is the thing that
    // was verified, and refuses to replay into a ledger that is already wrong.
    check_database(&conn)?;

    // Step 5, second half: the migration check. A restored database from the
    // current schema era applies zero migrations, which is the check passing;
    // an older one is brought forward; a newer one is refused by the
    // migration machinery rather than silently opened.
    let migration_summary = run_migrations(&mut conn, &registry(), None, clock)?;

    // The damaged directory is forensic evidence. Take one stable read-only
    // snapshot of its active pending files and existing quarantine while the
    // spool barrier excludes concurrent mutation, then replay only the copied
    // files in the new destination. Draining the damaged directory directly
    // would delete or quarantine its records and turn recovery into the
    // destructive operation step 2 expressly forbids.
    let (surviving_pending, preexisting_surviving_quarantine) = match surviving {
        Some(dir) => snapshot_surviving_evidence(dir)?,
        None => (Vec::new(), Vec::new()),
    };

    // Step 6: replay both sources, the archive's restored pending records
    // first. The drain is keyed on the attempt identifier, so a record the
    // two sources hold in common applies once: the archive's copy (the older
    // evidence) wins, and the surviving snapshot is deleted as a no-op from
    // the restored directory only.
    let archive_replay = drain_pending_recovering(&mut conn, destination)?;
    let surviving_replay = if surviving.is_some() {
        copy_pending_snapshots(destination, &surviving_pending)?;
        Some(drain_pending_recovering(&mut conn, destination)?)
    } else {
        None
    };

    // The final verdict: the checks run again over the restored database with
    // the replay's commits in it, because the criterion a recovery answers to
    // is about the database the operator is left holding, not the one it
    // started from.
    check_database(&conn)?;
    let observation_count = observation_row_count(&conn)?;

    // Step 7: rebuild the projection from the same restored, replayed state
    // the operator is left holding, never from the archive (which carries
    // none) or from the damaged directory's own copy (which may be exactly
    // what this recovery was called in to fix).
    let projection_target = crate::projection::projection_path_in(destination);
    let publication = crate::projection::publish(&conn, &projection_target);
    let projection_recovery = projection_recovery_status(&publication);
    drop(conn);
    let (schema_version, ledger_generation) =
        archived_database_metadata(&destination_database, busy_timeout)?;

    // Records this replay quarantined are already named in the drain reports;
    // records the surviving directory had already quarantined before the
    // replay are evidence the recovery cannot apply either, and the pre-replay
    // scan adds them to the same report rather than leaving them invisible.
    let mut unrecovered = quarantined_from(
        UnrecoveredEvidenceSource::Archive,
        &archive_replay.quarantined,
    );
    if surviving.is_some() {
        if let Some(report) = &surviving_replay {
            unrecovered.extend(quarantined_from(
                UnrecoveredEvidenceSource::SurvivingDirectory,
                &report.quarantined,
            ));
        }
        unrecovered.extend(quarantined_from(
            UnrecoveredEvidenceSource::SurvivingDirectory,
            &preexisting_surviving_quarantine,
        ));
    }
    unrecovered.sort_by(|a, b| {
        (a.source.as_str(), a.file_name.as_str(), a.reason.as_str()).cmp(&(
            b.source.as_str(),
            b.file_name.as_str(),
            b.reason.as_str(),
        ))
    });

    Ok(RestoreSummary {
        archive: archive.to_path_buf(),
        destination: destination.to_path_buf(),
        surviving: surviving.map(Path::to_path_buf),
        archive_verified: verified.verified,
        schema_version,
        ledger_generation,
        pending_restored,
        migrations_applied: migration_summary.applied.len(),
        archive_replay,
        surviving_replay,
        observation_count,
        unrecovered,
        projection_recovery,
        transcript_recovery: TRANSCRIPT_RECOVERY,
    })
}

/// Every refusal that protects evidence: the configured state directory the
/// recovery must never overwrite, the archive the recovery is reading, the
/// surviving directory the recovery is rescuing pending evidence from, and a
/// destination that already exists (the restore must be into a new directory).
/// All of them are checked before anything is read or written, so a refused
/// restore leaves every byte it was handed exactly where it was.
fn refuse_evidence_destroying_destinations(
    configured_state_dir: &Path,
    archive: &Path,
    destination: &Path,
    surviving: Option<&Path>,
) -> Result<(), Error> {
    if same_directory(destination, configured_state_dir) {
        return Err(Error::Store(format!(
            "restore destination {destination:?} is the configured state directory; a recovery \
             never writes into the state directory it is recovering from, because restoring on \
             top of damage destroys the forensic copy at the moment somebody needs to know what \
             happened (docs/recovery.md, step 2)"
        )));
    }
    if same_directory(destination, archive) {
        return Err(Error::Store(format!(
            "restore destination {destination:?} is the archive itself; restoring into it would \
             destroy the only verified copy of the state"
        )));
    }
    if let Some(dir) = surviving {
        if same_directory(destination, dir) {
            return Err(Error::Store(format!(
                "restore destination {destination:?} is the surviving directory; the replay \
                 deletes and quarantines pending records as it drains them, which must never \
                 happen to the directory the damaged state still lives in"
            )));
        }
        if same_directory(dir, archive) {
            return Err(Error::Store(format!(
                "surviving directory {dir:?} is the archive itself; replaying from it would \
                 quarantine records inside the archive and break its verified format"
            )));
        }
    }
    if destination.exists() {
        return Err(Error::Store(format!(
            "restore destination {destination:?} already exists; a recovery restores into a new \
             directory and never overwrites one (docs/recovery.md, step 4)"
        )));
    }
    Ok(())
}

/// Whether two paths name the same directory, resolved so that a path spelled
/// through a symlink or a `..` still compares equal to the directory it
/// reaches. A path that does not exist yet cannot be canonicalized, so two
/// non-existent paths compare lexically after component normalization; a
/// missing destination next to an existing other path is a different directory
/// by construction, because the destination is created fresh at restore time.
///
/// `pub(crate)` so the drill (`aub-n27.2`) can refuse the same way against the
/// same configured state directory, without a second implementation of what
/// "the same directory" means drifting from this one.
pub(crate) fn same_directory(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        (Err(_), Err(_)) => normalize_components(a) == normalize_components(b),
        // One side exists and the other does not: the existing side is a
        // directory this process can name, and the missing side names
        // something else until it is created.
        _ => false,
    }
}

fn normalize_components(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            other @ std::path::Component::Prefix(_)
            | other @ std::path::Component::RootDir
            | other @ std::path::Component::Normal(_) => normalized.push(other),
        }
    }
    normalized
}

/// Runs the two SQLite checks against an open connection and turns a failure
/// into the store-failure class, naming the stage that rejected the database.
fn check_database(conn: &rusqlite::Connection) -> Result<DatabaseVerification, Error> {
    verify_database_on_connection(conn)?.map_err(|failure: DatabaseVerificationFailure| {
        Error::Store(format!(
            "restored database failed {} checking: {}",
            failure.stage.as_str(),
            failure.detail
        ))
    })
}

/// Copies the archive's database into the destination durably: streamed, then
/// fsynced, so a crash immediately after the restore leaves a database that is
/// whole on disk rather than one that only looks whole in the page cache.
fn copy_database_file(source: &Path, destination: &Path) -> Result<(), Error> {
    let mut from = fs::File::open(source).map_err(|error| {
        Error::Store(format!("cannot read archive database {source:?}: {error}"))
    })?;
    let mut to = create_file_mode_0600(destination)?;
    io::copy(&mut from, &mut to)
        .map_err(|error| Error::Store(format!("cannot copy the archive database: {error}")))?;
    to.sync_all()
        .map_err(|error| Error::Store(format!("cannot sync the restored database: {error}")))?;
    Ok(())
}

/// Copies exactly the pending records the archive's manifest lists, the
/// inventory the verification stage validated. A file that happens to sit in
/// the archive's pending directory without being in the manifest is not part
/// of the verified archive, and a restore built on the manifest is a restore
/// of the thing that was verified, not of whatever the directory holds.
fn copy_pending_records(
    archive: &Path,
    destination: &Path,
    pending_records: &[String],
) -> Result<usize, Error> {
    ensure_dir_mode_0700(&destination.join(ARCHIVE_PENDING_DIR))?;
    for relative in pending_records {
        let bytes = fs::read(archive.join(relative)).map_err(|error| {
            Error::Store(format!(
                "cannot read archived pending record {relative}: {error}"
            ))
        })?;
        let mut file = create_file_mode_0600(&destination.join(relative))?;
        file.write_all(&bytes).map_err(|error| {
            Error::Store(format!(
                "cannot write restored pending record {relative:?}: {error}"
            ))
        })?;
        file.sync_all().map_err(|error| {
            Error::Store(format!(
                "cannot sync restored pending record {relative:?}: {error}"
            ))
        })?;
    }
    Ok(pending_records.len())
}

/// Captures the active pending files and existing quarantine from the damaged
/// directory under one read-only snapshot barrier. The returned bytes are the
/// only surviving-state inputs the replay writes or removes from thereafter.
fn snapshot_surviving_evidence(
    surviving: &Path,
) -> Result<
    (
        Vec<crate::store::spool::PendingRecordSnapshot>,
        Vec<crate::store::spool::QuarantinedRecord>,
    ),
    Error,
> {
    let barrier = crate::store::spool::acquire_state_snapshot_barrier(surviving)?;
    let quarantine = scan_existing_quarantine(surviving)?;
    let pending = crate::store::spool::snapshot_pending_records(surviving, &barrier)?;
    Ok((pending, quarantine))
}

/// Materializes a read-only surviving snapshot in the restored pending spool.
/// The snapshot is copied only after archive replay has removed or quarantined
/// its active records, so a same-named source record replaces no evidence and
/// is then evaluated by the normal idempotent drain.
fn copy_pending_snapshots(
    destination: &Path,
    records: &[crate::store::spool::PendingRecordSnapshot],
) -> Result<(), Error> {
    for record in records {
        let path = destination
            .join(ARCHIVE_PENDING_DIR)
            .join(&record.file_name);
        let mut file = create_file_mode_0600(&path)?;
        file.write_all(&record.bytes).map_err(|error| {
            Error::Store(format!(
                "cannot write surviving pending snapshot {path:?}: {error}"
            ))
        })?;
        file.sync_all().map_err(|error| {
            Error::Store(format!(
                "cannot sync surviving pending snapshot {path:?}: {error}"
            ))
        })?;
    }
    Ok(())
}

fn quarantined_from(
    source: UnrecoveredEvidenceSource,
    records: &[crate::store::spool::QuarantinedRecord],
) -> Vec<UnrecoveredEvidence> {
    records
        .iter()
        .map(|record| UnrecoveredEvidence {
            source,
            file_name: record.file_name.clone(),
            reason: record.reason.clone(),
        })
        .collect()
}

/// Reads a directory's existing quarantine holding area: each record's reason
/// file (`<name>.reason`) beside the record quarantine moved there with. A
/// record whose reason file is missing is still reported, with the absence as
/// its reason, because an unnamed loss is the one failure a recovery report
/// exists to prevent.
fn scan_existing_quarantine(
    dir: &Path,
) -> Result<Vec<crate::store::spool::QuarantinedRecord>, Error> {
    let quarantine = crate::store::spool::quarantine_dir(dir);
    if !quarantine.is_dir() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(&quarantine)
        .map_err(|error| {
            Error::Store(format!(
                "cannot list the surviving directory's quarantine {quarantine:?}: {error}"
            ))
        })?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_none_or(|extension| extension != "reason")
        })
        .collect();
    entries.sort();
    entries
        .into_iter()
        .map(|path| {
            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    Error::Store(format!(
                        "quarantined record {path:?} has no UTF-8 file name"
                    ))
                })?
                .to_owned();
            let reason = fs::read_to_string(quarantine.join(format!("{file_name}.reason")))
                .unwrap_or_else(|_| "quarantine reason file is missing".to_owned());
            Ok(crate::store::spool::QuarantinedRecord { file_name, reason })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::backup::create_archive;
    use crate::domain::time::{FakeClock, UtcTimestamp};
    use crate::domain::window::QuantizationSemantics;
    use crate::store::account::observe_account;
    use crate::store::connection::{AccessMode, PragmaPolicy};
    use crate::store::meter_attempt::{DueReason, NewMeterAttempt, start_meter_attempt};
    use crate::store::meter_evidence::{measurement_basis_sql, quantization_sql};
    use crate::store::sample_run::{Trigger, start_sample_run};
    use crate::store::sampling_policy_snapshot::{ResolvedSamplingPolicy, resolve_policy_snapshot};
    use crate::store::spool::{PendingTerminalBundle, PendingWindow, drain_pending, spool_pending};
    use crate::store::startup::FakeMountTable;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A fresh scratch directory under the system temp dir, removed on drop.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("aub-restore-test-{}-{suffix}", std::process::id()));
            fs::create_dir_all(&path).expect("scratch dir must be creatable");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn clock_at(nanos: i64) -> FakeClock {
        FakeClock::new(UtcTimestamp::from_unix_nanos(nanos))
    }

    fn policy() -> PragmaPolicy {
        PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1000),
        }
    }

    /// A migrated ledger database at `state_dir`, ready for attempt and
    /// observation writes. The directory is created at the production mode
    /// first, because `open` creates the file, never its parent directory.
    fn migrated_state_dir(state_dir: &Path) -> rusqlite::Connection {
        ensure_dir_mode_0700(state_dir).unwrap();
        let mut conn = open(
            &state_dir.join(LEDGER_DATABASE_FILE),
            AccessMode::ReadWrite,
            &policy(),
        )
        .unwrap();
        crate::store::migrate::run_migrations(&mut conn, &registry(), None, &clock_at(0)).unwrap();
        conn
    }

    /// Seeds one fixture account and one started attempt, and returns the
    /// attempt's row id: the parent row a pending record's replay needs.
    fn seed_attempt(conn: &rusqlite::Connection) -> i64 {
        let account =
            observe_account(conn, "anthropic", "work", UtcTimestamp::from_unix_nanos(0)).unwrap();
        let run = start_sample_run(
            conn,
            Trigger::Manual,
            UtcTimestamp::from_unix_nanos(0),
            "test",
        )
        .unwrap();
        let snapshot = resolve_policy_snapshot(
            conn,
            account,
            UtcTimestamp::from_unix_nanos(0),
            &ResolvedSamplingPolicy {
                ordinary_cadence: MonotonicDuration::from_seconds(300),
                freshness_horizon: MonotonicDuration::from_seconds(720),
                reset_edge_policy: "lead-120s".into(),
                retry_backoff_policy: "exponential-3".into(),
                command_budget: MonotonicDuration::from_seconds(8),
                policy_algorithm_version: "v1".into(),
            },
        )
        .unwrap();
        start_meter_attempt(
            conn,
            &NewMeterAttempt {
                run_id: run,
                account_id: account,
                provider: "anthropic".into(),
                request_started_at: UtcTimestamp::from_unix_nanos(0),
                credential_context_id: Some("ctx-1".into()),
                policy_snapshot_id: snapshot,
                due_at: UtcTimestamp::from_unix_nanos(0),
                due_reason: DueReason::OrdinaryCadence,
                due_basis: None,
                provider_contract_id: "endpoint-schema-v3".into(),
                meter_semantics_id: "account-5h-v2".into(),
            },
        )
        .unwrap()
        .value()
    }

    /// A structurally valid bundle for `attempt_id`, one window.
    fn valid_bundle(attempt_id: i64) -> PendingTerminalBundle {
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
            normalized_fingerprint: format!("fp-{attempt_id}"),
            windows: vec![PendingWindow {
                semantic_key: "five_hour".into(),
                scope_kind: "account_wide".into(),
                scoped_model: None,
                quota_used_ppm: 250_000,
                reported_resolution_ppm: 10_000,
                quantization: quantization_sql::as_sql(QuantizationSemantics::Exact).to_owned(),
                resets_at_nanos: Some(5_000),
                nominal_duration_nanos: 18_000_000_000_000,
            }],
        }
    }

    /// Commits one observation for an existing attempt row through the same
    /// path a drain uses: spool the record, then drain it.
    fn commit_observation(state_dir: &Path, conn: &mut rusqlite::Connection, attempt_id: i64) {
        spool_pending(state_dir, &valid_bundle(attempt_id)).unwrap();
        let report = drain_pending(conn, state_dir).unwrap();
        assert_eq!(
            report.applied, 1,
            "the seeded record must commit, not quarantine"
        );
    }

    fn mounts() -> FakeMountTable {
        FakeMountTable::new()
    }

    #[test]
    fn recovery_documentation_keeps_the_eight_steps_in_order() {
        let procedure = include_str!("../docs/recovery.md");
        let steps = [
            "1. Stop every mutating `aub` invocation",
            "2. Preserve the damaged state directory.",
            "3. Verify the archive checksum and manifest",
            "4. Restore into a directory that does not exist yet:",
            "5. Read the restore result.",
            "6. Check both replay lines and the exact `observations=N` count.",
            "7. The projection is rebuilt, never restored:",
            "8. Transcript-derived tables have no writer in Phase 1",
        ];
        let mut prior = 0;
        for step in steps {
            let position = procedure
                .find(step)
                .unwrap_or_else(|| panic!("recovery procedure is missing {step:?}"));
            assert!(
                position >= prior,
                "recovery procedure puts {step:?} before its predecessor"
            );
            prior = position;
        }
        assert!(
            procedure.contains("Do not overwrite it"),
            "step 2 must forbid overwriting damaged evidence"
        );
    }

    // --- refusals --------------------------------------------------------

    #[test]
    fn restore_refuses_a_destination_that_is_the_configured_state_directory() {
        let scratch = ScratchDir::new();
        let state_dir = scratch.path().join("aub");
        {
            let conn = migrated_state_dir(&state_dir);
            let _first = seed_attempt(&conn);
        }
        ensure_dir_mode_0700(&state_dir).unwrap();
        fs::write(state_dir.join("operator-file"), b"irreplaceable").unwrap();
        let archive = scratch.path().join("archive");
        let clock = clock_at(5_000);
        create_archive(
            &state_dir,
            &archive,
            MonotonicDuration::from_millis(1000),
            &clock,
        )
        .unwrap();

        let error = restore_archive(
            &state_dir,
            &archive,
            &state_dir,
            None,
            MonotonicDuration::from_millis(1000),
            &mounts(),
            &clock,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("configured state directory"),
            "the refusal must say what it protected: {error}"
        );

        // Nothing was written into the configured directory: the operator
        // file is still exactly where it was.
        assert!(state_dir.join("operator-file").exists());
    }

    #[test]
    fn restore_refuses_an_existing_destination() {
        let scratch = ScratchDir::new();
        let state_dir = scratch.path().join("aub");
        {
            let conn = migrated_state_dir(&state_dir);
            let _first = seed_attempt(&conn);
        }
        let archive = scratch.path().join("archive");
        let clock = clock_at(5_000);
        create_archive(
            &state_dir,
            &archive,
            MonotonicDuration::from_millis(1000),
            &clock,
        )
        .unwrap();

        let existing = scratch.path().join("existing");
        ensure_dir_mode_0700(&existing).unwrap();
        let error = restore_archive(
            &scratch.path().join("unused-configured"),
            &archive,
            &existing,
            None,
            MonotonicDuration::from_millis(1000),
            &mounts(),
            &clock,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("new directory"),
            "the refusal must say the restore must be into a new directory: {error}"
        );
    }

    #[test]
    fn restore_snapshots_a_surviving_configured_state_directory_without_mutating_it() {
        let scratch = ScratchDir::new();
        let state_dir = scratch.path().join("aub");
        {
            let conn = migrated_state_dir(&state_dir);
            let _first = seed_attempt(&conn);
        }
        let archive = scratch.path().join("archive");
        let clock = clock_at(5_000);
        create_archive(
            &state_dir,
            &archive,
            MonotonicDuration::from_millis(1000),
            &clock,
        )
        .unwrap();

        fs::write(state_dir.join("operator-file"), b"irreplaceable").unwrap();
        let restored = scratch.path().join("restored");
        let summary = restore_archive(
            &state_dir,
            &archive,
            &restored,
            Some(&state_dir),
            MonotonicDuration::from_millis(1000),
            &mounts(),
            &clock,
        )
        .unwrap();
        assert!(
            summary.unrecovered.is_empty(),
            "a clean configured-state snapshot has no unrecovered evidence: {:?}",
            summary.unrecovered
        );
        assert_eq!(
            fs::read(state_dir.join("operator-file")).unwrap(),
            b"irreplaceable"
        );
    }

    // --- the clean restore -------------------------------------------------

    #[test]
    fn a_clean_archive_restores_with_the_exact_source_counts_and_an_empty_report() {
        let scratch = ScratchDir::new();
        let state_dir = scratch.path().join("aub");
        let mut conn = migrated_state_dir(&state_dir);
        let first = seed_attempt(&conn);
        let second = seed_attempt(&conn);
        commit_observation(&state_dir, &mut conn, first);
        commit_observation(&state_dir, &mut conn, second);

        // One record left pending: the backup's own cut drain applies it, so
        // the archive is clean: nothing pending, nothing lost.
        let third = seed_attempt(&conn);
        spool_pending(&state_dir, &valid_bundle(third)).unwrap();
        drop(conn);

        let archive = scratch.path().join("archive");
        let clock = clock_at(5_000);
        let backup = create_archive(
            &state_dir,
            &archive,
            MonotonicDuration::from_millis(1000),
            &clock,
        )
        .unwrap();
        assert_eq!(
            backup.pending_records, 0,
            "the cut drain must have applied the record"
        );
        assert!(backup.verified);

        let restored = scratch.path().join("restored");
        let summary = restore_archive(
            &scratch.path().join("unused-configured"),
            &archive,
            &restored,
            None,
            MonotonicDuration::from_millis(1000),
            &mounts(),
            &clock,
        )
        .unwrap();

        // The restored ledger holds exactly what the source held: three
        // observations, no more and no fewer.
        assert_eq!(summary.observation_count.value(), 3);
        assert_eq!(summary.pending_restored, 0);
        assert_eq!(
            summary.migrations_applied, 0,
            "a current-era archive needs no migration"
        );
        assert!(summary.archive_verified);
        assert!(
            summary.unrecovered.is_empty(),
            "a clean archive must recover everything: {:?}",
            summary.unrecovered
        );
        assert_eq!(summary.projection_recovery.disposition, "rebuilt");
        assert_eq!(summary.transcript_recovery, TRANSCRIPT_RECOVERY);
    }

    // --- the projection is rebuilt, never restored -------------------------

    /// The archive carries no projection file at all (`create_archive` writes
    /// only the database and pending evidence), so a restored destination's
    /// projection cannot be a copy of anything the archive held. This proves
    /// the file that lands there instead is a deterministic function of the
    /// restored database: rebuilding it a second time, from a fresh
    /// connection to the same restored database, reproduces the exact bytes
    /// `restore_archive` itself wrote.
    #[test]
    fn the_restored_projection_is_rebuilt_and_reproducible_from_the_restored_database() {
        let scratch = ScratchDir::new();
        let state_dir = scratch.path().join("aub");
        let mut conn = migrated_state_dir(&state_dir);
        let attempt = seed_attempt(&conn);
        commit_observation(&state_dir, &mut conn, attempt);
        drop(conn);

        let archive = scratch.path().join("archive");
        let clock = clock_at(5_000);
        create_archive(
            &state_dir,
            &archive,
            MonotonicDuration::from_millis(1000),
            &clock,
        )
        .unwrap();
        assert!(
            !archive
                .join(crate::projection::PROJECTION_FILE_NAME)
                .exists(),
            "the archive must carry no projection file for this to be a real rebuild proof"
        );

        let restored = scratch.path().join("restored");
        let summary = restore_archive(
            &scratch.path().join("unused-configured"),
            &archive,
            &restored,
            None,
            MonotonicDuration::from_millis(1000),
            &mounts(),
            &clock,
        )
        .unwrap();
        assert_eq!(summary.projection_recovery.disposition, "rebuilt");

        let rebuilt_bytes = fs::read(crate::projection::projection_path_in(&restored)).unwrap();

        // A second, independent rebuild from a fresh connection to the same
        // restored database: the deterministic-reconstruction proof the
        // module doc comment promises (no wall clock in the content).
        let policy = policy();
        let reconnected = open(
            &restored.join(LEDGER_DATABASE_FILE),
            AccessMode::ReadOnly,
            &policy,
        )
        .unwrap();
        let reconstruction_path = restored.join("projection.reconstruction-check");
        let publication = crate::projection::publish(&reconnected, &reconstruction_path);
        assert!(matches!(
            publication,
            crate::projection::Publication::Published { .. }
        ));
        let reconstructed_bytes = fs::read(&reconstruction_path).unwrap();
        assert_eq!(
            rebuilt_bytes, reconstructed_bytes,
            "the rebuilt projection must be a deterministic function of the restored database"
        );
    }

    // --- the partial recovery: dual-source replay and the report ----------

    #[test]
    fn dual_source_replay_is_idempotent_and_the_report_names_what_could_not_be_recovered() {
        let scratch = ScratchDir::new();
        let state_dir = scratch.path().join("aub");
        let mut conn = migrated_state_dir(&state_dir);
        let first = seed_attempt(&conn);
        let second = seed_attempt(&conn);
        commit_observation(&state_dir, &mut conn, first);
        commit_observation(&state_dir, &mut conn, second);

        // A drainable pending record (its attempt row exists in the ledger),
        // and an orphan one whose attempt row exists nowhere. The orphan
        // sorts lexically before the drainable record, so the backup's cut
        // drain fails on it and stops, leaving BOTH records for the archive
        // to carry: the exact shape the recovery procedure's dual-source
        // replay exists for.
        let third = seed_attempt(&conn);
        spool_pending(&state_dir, &valid_bundle(third)).unwrap();
        spool_pending(&state_dir, &valid_bundle(11)).unwrap();

        let archive = scratch.path().join("archive");
        let clock = clock_at(5_000);
        let backup = create_archive(
            &state_dir,
            &archive,
            MonotonicDuration::from_millis(1000),
            &clock,
        )
        .unwrap();
        assert_eq!(
            backup.pending_records, 2,
            "the failed cut drain must leave both records for the archive"
        );
        assert!(!backup.drain_completed);

        // Evidence newer than the cut: its attempt row is in the surviving
        // ledger only, so the restored database cannot take its record.
        let fourth = seed_attempt(&conn);
        spool_pending(&state_dir, &valid_bundle(fourth)).unwrap();
        drop(conn);

        let surviving_before_restore = [
            (
                "attempt-11.json",
                fs::read(state_dir.join("pending/attempt-11.json")).unwrap(),
            ),
            (
                "attempt-3.json",
                fs::read(state_dir.join("pending/attempt-3.json")).unwrap(),
            ),
            (
                "attempt-4.json",
                fs::read(state_dir.join("pending/attempt-4.json")).unwrap(),
            ),
        ];

        // The restored database starts from the archive, and the surviving
        // directory replays into it: attempt 3's record is in BOTH places,
        // and must land exactly once.
        let restored = scratch.path().join("restored");
        let summary = restore_archive(
            &scratch.path().join("unused-configured"),
            &archive,
            &restored,
            Some(&state_dir),
            MonotonicDuration::from_millis(1000),
            &mounts(),
            &clock,
        )
        .unwrap();

        assert_eq!(summary.pending_restored, 2);
        assert_eq!(
            summary.archive_replay.applied, 1,
            "attempt 3 replays from the restored archive"
        );
        assert_eq!(summary.archive_replay.already_applied, 0);
        assert_eq!(summary.surviving_replay.as_ref().unwrap().applied, 0);
        assert_eq!(
            summary.surviving_replay.as_ref().unwrap().already_applied,
            1,
            "the surviving copy of attempt 3's record must be a no-op against the applied one"
        );

        // Three observations exactly: the two committed into the archive plus
        // attempt 3's replay. A duplicated replay would make this four, and
        // this assertion is what catches it.
        assert_eq!(summary.observation_count.value(), 3);

        // Three unrecovered records, each named with its reason: the orphan
        // from both sources (its attempt row never existed anywhere) and the
        // post-cut record (its attempt row was never backed up).
        assert_eq!(summary.unrecovered.len(), 3, "{:?}", summary.unrecovered);
        assert!(
            summary
                .unrecovered
                .iter()
                .all(|item| !item.reason.is_empty())
        );
        assert_eq!(
            summary
                .unrecovered
                .iter()
                .filter(|item| item.source == UnrecoveredEvidenceSource::Archive)
                .count(),
            1
        );
        assert_eq!(
            summary
                .unrecovered
                .iter()
                .filter(|item| item.source == UnrecoveredEvidenceSource::SurvivingDirectory)
                .count(),
            2
        );
        assert!(summary.unrecovered.iter().any(|item| item.source
            == UnrecoveredEvidenceSource::SurvivingDirectory
            && item.file_name == "attempt-4.json"));

        // The damaged directory is evidence, not a second replay workspace.
        // A direct drain would remove the duplicate record and quarantine the
        // two rejected records; a snapshot replay leaves every source byte in
        // place while still recovering the restored ledger.
        for (name, expected) in surviving_before_restore {
            assert_eq!(
                fs::read(state_dir.join("pending").join(name)).unwrap(),
                expected
            );
        }
        assert!(
            !state_dir.join("pending/quarantine").exists(),
            "the recovery must not quarantine records in the damaged directory"
        );
    }

    #[test]
    fn a_preexisting_quarantine_in_the_surviving_directory_is_reported_as_unrecovered() {
        let scratch = ScratchDir::new();
        let state_dir = scratch.path().join("aub");
        let mut conn = migrated_state_dir(&state_dir);
        let first = seed_attempt(&conn);
        commit_observation(&state_dir, &mut conn, first);
        drop(conn);

        let archive = scratch.path().join("archive");
        let clock = clock_at(5_000);
        create_archive(
            &state_dir,
            &archive,
            MonotonicDuration::from_millis(1000),
            &clock,
        )
        .unwrap();

        // Evidence the damaged directory had already given up applying before
        // the recovery ran: malformed, quarantined in place, and carried by no
        // archive. The report is the only place it stays visible.
        ensure_dir_mode_0700(&state_dir.join("pending")).unwrap();
        fs::write(
            state_dir.join("pending").join("attempt-77.json"),
            b"{ not json",
        )
        .unwrap();
        let mut conn = migrated_state_dir(&state_dir);
        crate::store::spool::drain_pending(&mut conn, &state_dir).unwrap();
        drop(conn);

        let restored = scratch.path().join("restored");
        let summary = restore_archive(
            &scratch.path().join("unused-configured"),
            &archive,
            &restored,
            Some(&state_dir),
            MonotonicDuration::from_millis(1000),
            &mounts(),
            &clock,
        )
        .unwrap();

        assert_eq!(summary.observation_count.value(), 1);
        assert_eq!(summary.unrecovered.len(), 1, "{:?}", summary.unrecovered);
        assert_eq!(summary.unrecovered[0].file_name, "attempt-77.json");
        assert_eq!(
            summary.unrecovered[0].source,
            UnrecoveredEvidenceSource::SurvivingDirectory
        );
        assert!(
            summary.unrecovered[0].reason.contains("invalid JSON"),
            "{:?}",
            summary.unrecovered[0].reason
        );
    }
}
