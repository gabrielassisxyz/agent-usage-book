//! The corrupted-state recovery drill: damages a scratch state directory in
//! one of four distinct ways, or takes a real archive, and proves that the
//! documented recovery procedure (`docs/recovery.md`) brings it back
//! (`aub-n27.2`, PLAN.md sections 33 Phase 13, 36, 38).
//!
//! May not depend on:
//! - provider adapters
//! - presentation
//!
//! Every damage case here corrupts the **surviving** (damaged) directory this
//! module hands `restore::restore_archive` alongside a separately created,
//! undamaged archive, never the archive itself: a drill that corrupted the
//! archive would be testing whether verification catches a bad archive, not
//! whether the documented procedure recovers from live damage. Two of the
//! four cases ([`DamageCase::TruncatedDatabase`] and
//! [`DamageCase::UnsupportedSchemaVersion`]) exist to prove a stronger claim
//! than "recovery works": that recovery never opens the surviving directory's
//! own database at all, by damaging it in a way that would explode loudly if
//! it were ever read.

use std::fs;
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::backup::create_archive;
use crate::domain::time::{Clock, MonotonicDuration, UtcTimestamp};
use crate::error::Error;
use crate::restore::{RestoreSummary, restore_archive, same_directory};
use crate::store::connection::{AccessMode, LEDGER_DATABASE_FILE, PragmaPolicy, open};
use crate::store::meter_attempt::{DueReason, NewMeterAttempt, start_meter_attempt};
use crate::store::meter_evidence::{measurement_basis_sql, quantization_sql};
use crate::store::migrations::registry;
use crate::store::rate_card::open_ledger;
use crate::store::sample_run::{Trigger, start_sample_run};
use crate::store::sampling_policy_snapshot::{ResolvedSamplingPolicy, resolve_policy_snapshot};
use crate::store::spool::{PendingTerminalBundle, PendingWindow, drain_pending, pending_dir};
use crate::store::startup::{MountTable, create_file_mode_0600, ensure_dir_mode_0700};

/// One of the four ways this drill can damage a scratch state directory, each
/// exercising a distinct failure shape rather than a variant of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DamageCase {
    /// The surviving directory's ledger database is truncated to a few
    /// bytes: not SQLite at all any more. Recovery never opens it.
    TruncatedDatabase,
    /// The surviving directory's projection file is overwritten with bytes
    /// that are not a projection. Recovery does not restore it; it rebuilds
    /// a fresh one from the restored database instead.
    CorruptedProjection,
    /// The surviving directory's pending spool holds one record that is not
    /// valid JSON. Recovery reports it as unrecovered evidence rather than
    /// stopping the replay.
    MalformedSpoolRecord,
    /// The surviving directory's ledger database records a schema version
    /// past every version this binary's migration registry knows: the shape
    /// a database a newer `aub` had already migrated would have. Recovery
    /// never opens it either.
    UnsupportedSchemaVersion,
}

impl DamageCase {
    pub const ALL: [DamageCase; 4] = [
        Self::TruncatedDatabase,
        Self::CorruptedProjection,
        Self::MalformedSpoolRecord,
        Self::UnsupportedSchemaVersion,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::TruncatedDatabase => "truncated-database",
            Self::CorruptedProjection => "corrupted-projection",
            Self::MalformedSpoolRecord => "malformed-spool-record",
            Self::UnsupportedSchemaVersion => "unsupported-schema-version",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|case| case.as_str() == name)
    }
}

/// What the drill was pointed at: a seeded, reproducible damage case built
/// entirely by this module, or a real archive an operator names for a
/// periodic run against last night's backup.
#[derive(Debug, Clone)]
pub enum DrillSource {
    Seeded(DamageCase),
    Archive(PathBuf),
}

impl DrillSource {
    /// The machine-readable label this source prints and records: `seeded:`
    /// or `archive:` followed by the case name or path.
    pub fn label(&self) -> String {
        match self {
            Self::Seeded(case) => format!("seeded:{}", case.as_str()),
            Self::Archive(path) => format!("archive:{}", path.display()),
        }
    }
}

/// What one drill run did: the restore it drove, plus the drill-specific
/// proofs a bare restore does not carry on its own (whether the damaged
/// directory came out unmodified, and, for the projection case, whether the
/// rebuilt file is exactly what a second independent rebuild produces).
#[derive(Debug, Clone)]
pub struct DrillReport {
    pub source: DrillSource,
    pub scratch_destination: PathBuf,
    pub restore: RestoreSummary,
    pub damaged_directory: Option<PathBuf>,
    pub damaged_directory_preserved: Option<bool>,
    pub projection_deterministic: Option<bool>,
    pub drilled_at: UtcTimestamp,
}

impl DrillReport {
    /// Whether every check this drill can assert on came back clean. A
    /// restore that itself failed never reaches this struct at all (`run_*`
    /// propagates that `Err` outward), so the only remaining ways a drill can
    /// fail are the two proofs a bare restore is silent about.
    pub fn passed(&self) -> bool {
        self.damaged_directory_preserved.unwrap_or(true)
            && self.projection_deterministic.unwrap_or(true)
    }
}

/// Refuses a drill that would touch the operator's own live state directory,
/// through either argument: as the scratch destination it would write into,
/// or, in real-archive mode, as the archive it would read the "verified"
/// database from. Checked before any filesystem write, in both modes.
fn refuse_operator_state_directory(
    configured_state_dir: &Path,
    scratch_destination: &Path,
    archive_source: Option<&Path>,
) -> Result<(), Error> {
    if same_directory(scratch_destination, configured_state_dir) {
        return Err(Error::Store(format!(
            "drill scratch destination {scratch_destination:?} is the configured state \
             directory; a drill never writes into the directory it exists to protect"
        )));
    }
    if let Some(archive) = archive_source
        && same_directory(archive, configured_state_dir)
    {
        return Err(Error::Store(format!(
            "drill archive source {archive:?} is the configured state directory; \
             the drill reads a verified archive, never the live state directory itself"
        )));
    }
    Ok(())
}

/// Runs one seeded damage case: builds a small, reproducible ledger, backs it
/// up cleanly, damages a copy of it, then restores the clean archive with the
/// damaged copy as the surviving directory, exactly as an operator following
/// `docs/recovery.md` would.
pub fn run_seeded(
    configured_state_dir: &Path,
    case: DamageCase,
    scratch_destination: &Path,
    busy_timeout: MonotonicDuration,
    mounts: &dyn MountTable,
    clock: &impl Clock,
) -> Result<DrillReport, Error> {
    refuse_operator_state_directory(configured_state_dir, scratch_destination, None)?;
    if scratch_destination.exists() {
        return Err(Error::Store(format!(
            "drill scratch destination {scratch_destination:?} already exists; a drill runs \
             into a new directory and never overwrites one"
        )));
    }
    ensure_dir_mode_0700(scratch_destination)?;

    let seed_dir = scratch_destination.join("seed");
    build_seed_state_dir(&seed_dir, clock)?;

    let archive_dir = scratch_destination.join("archive");
    create_archive(&seed_dir, &archive_dir, busy_timeout, clock)?;

    let damaged_dir = scratch_destination.join("damaged");
    copy_directory_recursively(&seed_dir, &damaged_dir)?;
    apply_damage(case, &damaged_dir, busy_timeout, clock)?;
    let damaged_before = snapshot_directory(&damaged_dir)?;

    let restored_dir = scratch_destination.join("restored");
    let restore = restore_archive(
        configured_state_dir,
        &archive_dir,
        &restored_dir,
        Some(&damaged_dir),
        busy_timeout,
        mounts,
        clock,
    )?;

    let damaged_after = snapshot_directory(&damaged_dir)?;
    let damaged_directory_preserved = damaged_before == damaged_after;

    let projection_deterministic = if case == DamageCase::CorruptedProjection {
        Some(projection_matches_a_second_independent_rebuild(
            &restored_dir,
            busy_timeout,
        )?)
    } else {
        None
    };

    Ok(DrillReport {
        source: DrillSource::Seeded(case),
        scratch_destination: scratch_destination.to_path_buf(),
        restore,
        damaged_directory: Some(damaged_dir),
        damaged_directory_preserved: Some(damaged_directory_preserved),
        projection_deterministic,
        drilled_at: clock.now(),
    })
}

/// Runs the drill against a real, named archive: the periodic case, exercised
/// on a schedule against last night's backup rather than against a seeded
/// fixture. Same refusal, same scratch destination, same integrity and
/// foreign-key checking as the seeded cases; the only difference is the
/// input, per this bead's own reasoning for keeping the two in one command.
pub fn run_archive(
    configured_state_dir: &Path,
    archive: &Path,
    scratch_destination: &Path,
    busy_timeout: MonotonicDuration,
    mounts: &dyn MountTable,
    clock: &impl Clock,
) -> Result<DrillReport, Error> {
    refuse_operator_state_directory(configured_state_dir, scratch_destination, Some(archive))?;
    let restore = restore_archive(
        configured_state_dir,
        archive,
        scratch_destination,
        None,
        busy_timeout,
        mounts,
        clock,
    )?;
    Ok(DrillReport {
        source: DrillSource::Archive(archive.to_path_buf()),
        scratch_destination: scratch_destination.to_path_buf(),
        restore,
        damaged_directory: None,
        damaged_directory_preserved: None,
        projection_deterministic: None,
        drilled_at: clock.now(),
    })
}

/// A small, migrated ledger with one account, one committed observation and a
/// published projection: enough for every damage case to have something real
/// to damage and for the restored destination to have observation and
/// projection content worth asserting on.
fn build_seed_state_dir(dir: &Path, clock: &impl Clock) -> Result<(), Error> {
    ensure_dir_mode_0700(dir)?;
    let db_path = dir.join(LEDGER_DATABASE_FILE);
    let mut conn = open_ledger(&db_path, MonotonicDuration::from_millis(1000), clock)?;

    let now = clock.now();
    let account_id = crate::store::account::observe_account(&conn, "anthropic", "drill", now)?;
    let run_id = start_sample_run(&conn, Trigger::Manual, now, "drill")?;
    let snapshot = resolve_policy_snapshot(
        &conn,
        account_id,
        now,
        &ResolvedSamplingPolicy {
            ordinary_cadence: MonotonicDuration::from_seconds(300),
            freshness_horizon: MonotonicDuration::from_seconds(720),
            reset_edge_policy: "lead-120s".into(),
            retry_backoff_policy: "none".into(),
            command_budget: MonotonicDuration::from_seconds(8),
            policy_algorithm_version: "v1".into(),
        },
    )?;
    let attempt_id = start_meter_attempt(
        &conn,
        &NewMeterAttempt {
            run_id,
            account_id,
            provider: "anthropic".into(),
            request_started_at: now,
            credential_context_id: Some("drill-fixture".into()),
            policy_snapshot_id: snapshot,
            due_at: now,
            due_reason: DueReason::OrdinaryCadence,
            due_basis: None,
            provider_contract_id: "endpoint-schema-v3".into(),
            meter_semantics_id: "account-5h-v2".into(),
        },
    )?
    .value();

    crate::store::spool::spool_pending(dir, &drill_bundle(attempt_id, now.unix_nanos()))?;
    let report = drain_pending(&mut conn, dir)?;
    if report.applied != 1 {
        return Err(Error::Internal(
            "drill fixture: the seeded observation did not commit".into(),
        ));
    }

    let projection_target = crate::projection::projection_path_in(dir);
    if !matches!(
        crate::projection::publish(&conn, &projection_target),
        crate::projection::Publication::Published { .. }
    ) {
        return Err(Error::Internal(
            "drill fixture: the seeded projection did not publish".into(),
        ));
    }
    Ok(())
}

/// `PendingTerminalBundle` field names its timestamps in absolute nanos, and
/// the store enforces that a result never precedes its attempt's start, so
/// every one of them is offset from `request_started_at_nanos` rather than
/// hard-coded: a drill seeded against any clock still produces a bundle its
/// own attempt row could actually have received.
fn drill_bundle(attempt_id: i64, request_started_at_nanos: i64) -> PendingTerminalBundle {
    PendingTerminalBundle {
        attempt_id,
        completed_at_nanos: request_started_at_nanos + 2_000,
        elapsed_nanos: 1_000,
        outcome: "success".into(),
        failure_class: None,
        retry_after_nanos: None,
        sanitized_error_classification: None,
        retry_index: None,
        clock_anomaly: false,
        response_classification: "success".into(),
        received_at_nanos: request_started_at_nanos + 1_000,
        provider_observed_at_original: Some("2026-09-02T00:00:00Z".into()),
        evidence_capsule: "{\"sanitized\":true}".into(),
        capsule_schema_version: "v1".into(),
        sanitizer_version: "v1".into(),
        capture_truncated: false,
        account_id: 1,
        provider: "anthropic".into(),
        provider_observed_at_nanos: Some(request_started_at_nanos + 900),
        measurement_basis: measurement_basis_sql::as_sql(
            crate::domain::time::MeasurementBasis::ProviderObserved,
        )
        .to_owned(),
        observed_plan: Some("max".into()),
        observed_tier: None,
        adapter_version: "drill-fixture".into(),
        provider_contract_id: "endpoint-schema-v3".into(),
        meter_semantics_id: "account-5h-v2".into(),
        normalized_fingerprint: format!("drill-fp-{attempt_id}"),
        windows: vec![PendingWindow {
            semantic_key: "five_hour".into(),
            scope_kind: "account_wide".into(),
            scoped_model: None,
            quota_used_ppm: 250_000,
            reported_resolution_ppm: 10_000,
            quantization: quantization_sql::as_sql(
                crate::domain::window::QuantizationSemantics::Exact,
            )
            .to_owned(),
            resets_at_nanos: Some(request_started_at_nanos + 5_000),
            nominal_duration_nanos: 18_000_000_000_000,
        }],
    }
}

fn apply_damage(
    case: DamageCase,
    damaged_dir: &Path,
    busy_timeout: MonotonicDuration,
    clock: &impl Clock,
) -> Result<(), Error> {
    match case {
        DamageCase::TruncatedDatabase => truncate_database(damaged_dir),
        DamageCase::CorruptedProjection => corrupt_projection(damaged_dir),
        DamageCase::MalformedSpoolRecord => write_malformed_spool_record(damaged_dir),
        DamageCase::UnsupportedSchemaVersion => {
            seed_unsupported_schema_version(damaged_dir, busy_timeout, clock)
        }
    }
}

/// Truncates the surviving directory's ledger database to a few bytes: not
/// SQLite any more. `restore_archive` never opens the surviving directory's
/// database, only its pending spool, so a correct recovery never even
/// notices this file is broken.
fn truncate_database(damaged_dir: &Path) -> Result<(), Error> {
    let db_path = damaged_dir.join(LEDGER_DATABASE_FILE);
    let file = fs::OpenOptions::new()
        .write(true)
        .open(&db_path)
        .map_err(|error| Error::Store(format!("cannot open {db_path:?} to damage it: {error}")))?;
    file.set_len(16)
        .map_err(|error| Error::Store(format!("cannot truncate {db_path:?}: {error}")))?;
    Ok(())
}

/// Overwrites the surviving directory's projection file with bytes that are
/// not a projection. `restore_archive` never reads it either: the restored
/// destination gets a freshly rebuilt one, so a naive implementation that
/// tried to copy this file forward is exactly what this case exists to
/// catch.
fn corrupt_projection(damaged_dir: &Path) -> Result<(), Error> {
    let path = crate::projection::projection_path_in(damaged_dir);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
        .map_err(|error| Error::Store(format!("cannot open {path:?} to damage it: {error}")))?;
    file.write_all(b"not-a-projection-file, deliberately corrupted by the drill")
        .map_err(|error| Error::Store(format!("cannot corrupt {path:?}: {error}")))?;
    Ok(())
}

/// Writes one record that is not valid JSON into the surviving directory's
/// pending spool. `restore_archive` does read this: the record is quarantined
/// and named in the restore's unrecovered-evidence report rather than
/// stopping the replay behind it.
fn write_malformed_spool_record(damaged_dir: &Path) -> Result<(), Error> {
    let dir = pending_dir(damaged_dir);
    ensure_dir_mode_0700(&dir)?;
    let mut file = create_file_mode_0600(&dir.join("attempt-9999.json"))?;
    file.write_all(b"{ this is not valid json")
        .map_err(|error| {
            Error::Store(format!("cannot write the malformed spool record: {error}"))
        })?;
    Ok(())
}

/// Records a schema version past every version this binary's migration
/// registry knows in the surviving directory's ledger database: the shape a
/// database a newer `aub` had already migrated past this one's understanding
/// would have. `restore_archive` never opens the surviving directory's
/// database, so a correct recovery never has to refuse it.
fn seed_unsupported_schema_version(
    damaged_dir: &Path,
    busy_timeout: MonotonicDuration,
    clock: &impl Clock,
) -> Result<(), Error> {
    let db_path = damaged_dir.join(LEDGER_DATABASE_FILE);
    let policy = PragmaPolicy { busy_timeout };
    let conn = open(&db_path, AccessMode::ReadWrite, &policy)?;
    crate::store::migrate::seed_schema_version_beyond_registry(&conn, &registry(), clock)?;
    Ok(())
}

/// Whether the projection `restore_archive` left at `restored_dir` is exactly
/// what a second, independent rebuild from a fresh connection to the same
/// restored database produces: the deterministic-reconstruction proof this
/// bead's acceptance criteria ask for, run against the drill's own restored
/// output rather than only in `restore`'s unit tests.
fn projection_matches_a_second_independent_rebuild(
    restored_dir: &Path,
    busy_timeout: MonotonicDuration,
) -> Result<bool, Error> {
    let recorded = fs::read(crate::projection::projection_path_in(restored_dir))
        .map_err(|error| Error::Store(format!("cannot read the rebuilt projection: {error}")))?;
    let policy = PragmaPolicy { busy_timeout };
    let conn = open(
        &restored_dir.join(LEDGER_DATABASE_FILE),
        AccessMode::ReadOnly,
        &policy,
    )?;
    let reconstruction_path = restored_dir.join("projection.drill-reconstruction-check");
    match crate::projection::publish(&conn, &reconstruction_path) {
        crate::projection::Publication::Published { .. } => {}
        crate::projection::Publication::Deferred { reason } => {
            return Err(Error::Store(format!(
                "cannot reconstruct the projection to compare against: {reason}"
            )));
        }
    }
    let reconstructed = fs::read(&reconstruction_path).map_err(|error| {
        Error::Store(format!("cannot read the reconstructed projection: {error}"))
    })?;
    Ok(recorded == reconstructed)
}

/// Copies every regular file under `source` into `destination`, preserving
/// relative paths, without going through the state-directory readiness check
/// (the destination is a drill scratch copy, not a state directory an
/// operator will point `aub` at).
fn copy_directory_recursively(source: &Path, destination: &Path) -> Result<(), Error> {
    ensure_dir_mode_0700(destination)?;
    for entry in fs::read_dir(source)
        .map_err(|error| Error::Store(format!("cannot read {source:?}: {error}")))?
    {
        let entry =
            entry.map_err(|error| Error::Store(format!("cannot read {source:?}: {error}")))?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|error| Error::Store(format!("cannot stat {from:?}: {error}")))?;
        if file_type.is_dir() {
            copy_directory_recursively(&from, &to)?;
        } else {
            fs::copy(&from, &to).map_err(|error| {
                Error::Store(format!("cannot copy {from:?} to {to:?}: {error}"))
            })?;
        }
    }
    Ok(())
}

/// A sorted `(relative path, sha256)` inventory of every regular file under
/// `dir`, deep enough to prove "unmodified" means every byte, not just the
/// files present at the top level.
fn snapshot_directory(dir: &Path) -> Result<Vec<(PathBuf, String)>, Error> {
    let mut entries = Vec::new();
    snapshot_directory_into(dir, dir, &mut entries)?;
    entries.sort();
    Ok(entries)
}

fn snapshot_directory_into(
    root: &Path,
    dir: &Path,
    entries: &mut Vec<(PathBuf, String)>,
) -> Result<(), Error> {
    for entry in
        fs::read_dir(dir).map_err(|error| Error::Store(format!("cannot read {dir:?}: {error}")))?
    {
        let entry = entry.map_err(|error| Error::Store(format!("cannot read {dir:?}: {error}")))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| Error::Store(format!("cannot stat {path:?}: {error}")))?;
        if file_type.is_dir() {
            snapshot_directory_into(root, &path, entries)?;
        } else {
            let bytes = fs::read(&path)
                .map_err(|error| Error::Store(format!("cannot read {path:?}: {error}")))?;
            let digest = Sha256::digest(&bytes);
            let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
            let relative = path
                .strip_prefix(root)
                .expect("a path this walk produced is under the root it walked")
                .to_path_buf();
            entries.push((relative, hex));
        }
    }
    Ok(())
}

/// One durable record of a completed drill run, appended as one JSON line to
/// `config.drill.result`: what it was pointed at, whether every check
/// passed, and when. `doctor` reads the last `passed = true` line for the age
/// of the last successful drill, mirroring how `backup_health` reads the
/// last verified archive's manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrillRunRecord {
    pub drilled_at: UtcTimestamp,
    pub source: String,
    pub scratch_destination: PathBuf,
    pub passed: bool,
}

impl DrillRunRecord {
    pub fn from_report(report: &DrillReport) -> Self {
        Self {
            drilled_at: report.drilled_at,
            source: report.source.label(),
            scratch_destination: report.scratch_destination.clone(),
            passed: report.passed(),
        }
    }

    fn to_json_line(&self) -> String {
        let value = json!({
            "drilled_at_unix_nanos": self.drilled_at.unix_nanos(),
            "source": self.source,
            "scratch_destination": self.scratch_destination.display().to_string(),
            "passed": self.passed,
        });
        serde_json::to_string(&value).expect("a drill run record contains only JSON values") + "\n"
    }
}

/// Appends one durable record to `path`, creating it and its parent
/// directory on first use. Append, never truncate: every run's record is
/// evidence for the next `doctor` read, not just the latest one's.
pub fn record_run(path: &Path, record: &DrillRunRecord) -> Result<(), Error> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|error| {
            Error::Store(format!(
                "cannot create the drill result directory {parent:?}: {error}"
            ))
        })?;
    }
    let mut file = open_append_mode_0600(path)?;
    let line = record.to_json_line();
    file.write_all(line.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            Error::Store(format!(
                "cannot append the drill result record to {path:?}: {error}"
            ))
        })
}

#[cfg(unix)]
fn open_append_mode_0600(path: &Path) -> Result<fs::File, Error> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| {
            Error::Store(format!(
                "cannot open the drill result record {path:?}: {error}"
            ))
        })
}

#[cfg(not(unix))]
fn open_append_mode_0600(path: &Path) -> Result<fs::File, Error> {
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| {
            Error::Store(format!(
                "cannot open the drill result record {path:?}: {error}"
            ))
        })
}

/// The age of the last successful drill, and whether it is past `max_age`:
/// the same `Missing`/`Verified` shape `crate::backup::BackupHealth` uses,
/// read from `doctor`'s own context the same way. There is no `Unverified`
/// arm the way a backup has one: a drill run's pass/fail is already decided
/// at the moment its record is appended, so the file has nothing left to
/// verify after the fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrillHealth {
    Missing,
    Verified {
        drilled_at: UtcTimestamp,
        age: crate::domain::time::Age,
        review_due: bool,
    },
}

pub fn drill_health(
    result_path: &Path,
    now: UtcTimestamp,
    max_age: MonotonicDuration,
) -> Result<DrillHealth, Error> {
    let Some(drilled_at) = last_successful_drill_at(result_path)? else {
        return Ok(DrillHealth::Missing);
    };
    let drill_age = crate::domain::time::age(
        None,
        crate::domain::time::ReceivedAt::new(drilled_at),
        crate::domain::time::MeasurementBasis::LocallyReceived,
        now,
        crate::domain::time::ClockSkewEnvelope::new(MonotonicDuration::from_nanos(0)),
    )
    .map_err(|_| Error::Store("last successful drill timestamp is in the future".into()))?;
    Ok(DrillHealth::Verified {
        drilled_at,
        age: drill_age,
        review_due: drill_age.as_nanos() > max_age.as_nanos(),
    })
}

/// The `drilled_at` of the last line in `result_path` whose `passed` field is
/// `true`. A line that fails to parse is skipped rather than refused: the
/// file is an append-only log a process can be killed mid-write of, and a
/// torn last line must not hide every well-formed line before it.
fn last_successful_drill_at(result_path: &Path) -> Result<Option<UtcTimestamp>, Error> {
    let text = match fs::read_to_string(result_path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Error::Store(format!(
                "cannot read the drill result record {result_path:?}: {error}"
            )));
        }
    };
    let mut last = None;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("passed").and_then(serde_json::Value::as_bool) != Some(true) {
            continue;
        }
        let Some(nanos) = value
            .get("drilled_at_unix_nanos")
            .and_then(serde_json::Value::as_i64)
        else {
            continue;
        };
        last = Some(UtcTimestamp::from_unix_nanos(nanos));
    }
    Ok(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::startup::FakeMountTable;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A fresh scratch directory under the system temp dir, removed on drop.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(tag: &str) -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-drill-test-{tag}-{}-{suffix}",
                std::process::id()
            ));
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

    fn clock_at(nanos: i64) -> crate::domain::time::FakeClock {
        crate::domain::time::FakeClock::new(UtcTimestamp::from_unix_nanos(nanos))
    }

    fn busy_timeout() -> MonotonicDuration {
        MonotonicDuration::from_millis(1000)
    }

    fn mounts() -> FakeMountTable {
        FakeMountTable::new()
    }

    // --- every damage case recovers, and preserves the damaged directory --

    /// Planted negative: a case dropped from [`DamageCase::ALL`] shrinks the
    /// count silently unless something asserts on it directly, since the
    /// loop-based tests below only ever check the cases that are still
    /// there.
    #[test]
    fn exactly_four_damage_cases_are_declared() {
        assert_eq!(DamageCase::ALL.len(), 4);
        let distinct: std::collections::BTreeSet<&str> =
            DamageCase::ALL.iter().map(|case| case.as_str()).collect();
        assert_eq!(distinct.len(), 4, "every case name must be distinct");
    }

    #[test]
    fn every_seeded_damage_case_recovers_and_preserves_the_damaged_directory() {
        for case in DamageCase::ALL {
            let scratch = ScratchDir::new(case.as_str());
            let configured = scratch.path().join("unused-configured-state-dir");
            let scratch_dest = scratch.path().join("scratch");
            let clock = clock_at(10_000);
            let report = run_seeded(
                &configured,
                case,
                &scratch_dest,
                busy_timeout(),
                &mounts(),
                &clock,
            )
            .unwrap_or_else(|error| panic!("{case:?} must recover: {error}"));

            assert!(report.restore.archive_verified, "{case:?}");
            assert_eq!(
                report.damaged_directory_preserved,
                Some(true),
                "{case:?}: the damaged directory must come out byte-identical"
            );
            assert!(report.passed(), "{case:?}");
        }
    }

    /// Planted negative for [`snapshot_directory`] itself, the primitive the
    /// preservation check is built from: a single changed byte anywhere
    /// under the tree must change the snapshot, so a real corruption the
    /// production code introduced could never compare equal by accident.
    #[test]
    fn snapshot_directory_changes_when_a_single_byte_under_it_changes() {
        let scratch = ScratchDir::new("snapshot-sensitivity");
        let dir = scratch.path().join("tree");
        fs::create_dir_all(dir.join("nested")).unwrap();
        fs::write(dir.join("nested/file.txt"), b"original bytes").unwrap();
        let before = snapshot_directory(&dir).unwrap();

        fs::write(dir.join("nested/file.txt"), b"original Bytes").unwrap();
        let after = snapshot_directory(&dir).unwrap();

        assert_ne!(
            before, after,
            "a one-byte change anywhere under the tree must change the snapshot"
        );
    }

    /// Mutation-shaped proof that the preservation check actually detects
    /// damage rather than always reading true: draining the surviving
    /// directory directly, the exact mistake `restore.rs`'s own module doc
    /// comment warns against ("draining the damaged directory directly would
    /// delete or quarantine its records"), changes its snapshot.
    #[test]
    fn draining_the_surviving_directory_directly_would_change_its_snapshot() {
        let scratch = ScratchDir::new("preservation-sensitivity");
        let configured = scratch.path().join("unused-configured-state-dir");
        let scratch_dest = scratch.path().join("scratch");
        let clock = clock_at(10_000);
        let report = run_seeded(
            &configured,
            DamageCase::MalformedSpoolRecord,
            &scratch_dest,
            busy_timeout(),
            &mounts(),
            &clock,
        )
        .unwrap();
        let damaged_dir = report.damaged_directory.as_ref().unwrap();
        let before = snapshot_directory(damaged_dir).unwrap();

        let policy = PragmaPolicy {
            busy_timeout: busy_timeout(),
        };
        let mut conn = open(
            &scratch_dest.join("restored").join(LEDGER_DATABASE_FILE),
            AccessMode::ReadWrite,
            &policy,
        )
        .unwrap();
        crate::store::spool::drain_pending(&mut conn, damaged_dir).unwrap();

        let after = snapshot_directory(damaged_dir).unwrap();
        assert_ne!(
            before, after,
            "draining the surviving directory in place must change its snapshot; \
             the preservation check exists to catch exactly this"
        );
    }

    // --- the corrupted projection is rebuilt, matching a second rebuild ---

    #[test]
    fn the_corrupted_projection_case_rebuilds_deterministically() {
        let scratch = ScratchDir::new("projection");
        let configured = scratch.path().join("unused-configured-state-dir");
        let scratch_dest = scratch.path().join("scratch");
        let clock = clock_at(10_000);
        let report = run_seeded(
            &configured,
            DamageCase::CorruptedProjection,
            &scratch_dest,
            busy_timeout(),
            &mounts(),
            &clock,
        )
        .unwrap();

        assert_eq!(report.restore.projection_recovery.disposition, "rebuilt");
        assert_eq!(report.projection_deterministic, Some(true));

        // The rebuilt file must not equal the corrupted bytes the damage
        // case wrote: proof this is a rebuild, not a restore of the damaged
        // copy.
        let corrupted = fs::read(crate::projection::projection_path_in(
            report.damaged_directory.as_ref().unwrap(),
        ))
        .unwrap();
        let rebuilt = fs::read(crate::projection::projection_path_in(
            &scratch_dest.join("restored"),
        ))
        .unwrap();
        assert_ne!(corrupted, rebuilt);
    }

    /// The other three cases have no reason to touch the projection file at
    /// all, so their determinism check is simply absent.
    #[test]
    fn only_the_corrupted_projection_case_runs_the_determinism_check() {
        for case in DamageCase::ALL {
            let scratch = ScratchDir::new(case.as_str());
            let configured = scratch.path().join("unused-configured-state-dir");
            let scratch_dest = scratch.path().join("scratch");
            let clock = clock_at(10_000);
            let report = run_seeded(
                &configured,
                case,
                &scratch_dest,
                busy_timeout(),
                &mounts(),
                &clock,
            )
            .unwrap();
            if case == DamageCase::CorruptedProjection {
                assert!(report.projection_deterministic.is_some(), "{case:?}");
            } else {
                assert!(report.projection_deterministic.is_none(), "{case:?}");
            }
        }
    }

    // --- the unrecovered-evidence report is accurate per case -------------

    #[test]
    fn the_malformed_spool_record_case_reports_exactly_that_record_as_unrecovered() {
        let scratch = ScratchDir::new("malformed");
        let configured = scratch.path().join("unused-configured-state-dir");
        let scratch_dest = scratch.path().join("scratch");
        let clock = clock_at(10_000);
        let report = run_seeded(
            &configured,
            DamageCase::MalformedSpoolRecord,
            &scratch_dest,
            busy_timeout(),
            &mounts(),
            &clock,
        )
        .unwrap();

        assert_eq!(report.restore.unrecovered.len(), 1);
        let evidence = &report.restore.unrecovered[0];
        assert_eq!(
            evidence.source,
            crate::restore::UnrecoveredEvidenceSource::SurvivingDirectory
        );
        assert_eq!(evidence.file_name, "attempt-9999.json");
        assert!(evidence.reason.contains("invalid JSON"), "{evidence:?}");
    }

    /// Planted negative: the three cases that damage nothing the replay ever
    /// reads must report zero unrecovered evidence, not a stale nonzero
    /// count carried over from a shared fixture.
    #[test]
    fn the_database_shaped_damage_cases_report_no_unrecovered_evidence() {
        for case in [
            DamageCase::TruncatedDatabase,
            DamageCase::CorruptedProjection,
            DamageCase::UnsupportedSchemaVersion,
        ] {
            let scratch = ScratchDir::new(case.as_str());
            let configured = scratch.path().join("unused-configured-state-dir");
            let scratch_dest = scratch.path().join("scratch");
            let clock = clock_at(10_000);
            let report = run_seeded(
                &configured,
                case,
                &scratch_dest,
                busy_timeout(),
                &mounts(),
                &clock,
            )
            .unwrap();
            assert!(
                report.restore.unrecovered.is_empty(),
                "{case:?}: {:?}",
                report.restore.unrecovered
            );
        }
    }

    // --- the refusal covers both modes -------------------------------------

    #[test]
    fn a_seeded_drill_refuses_the_configured_state_directory_as_its_scratch_destination() {
        let scratch = ScratchDir::new("refuse-seeded");
        let configured = scratch.path().join("aub");
        fs::create_dir_all(&configured).unwrap();
        let clock = clock_at(0);
        let error = run_seeded(
            &configured,
            DamageCase::TruncatedDatabase,
            &configured,
            busy_timeout(),
            &mounts(),
            &clock,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("configured state directory"),
            "{error}"
        );
        // Nothing was written: the operator's directory is still exactly
        // what it was, an empty directory with none of the drill's own
        // subdirectories.
        assert!(!configured.join("seed").exists());
    }

    #[test]
    fn an_archive_drill_refuses_the_configured_state_directory_as_either_argument() {
        let scratch = ScratchDir::new("refuse-archive");
        let configured = scratch.path().join("aub");
        fs::create_dir_all(&configured).unwrap();
        let clock = clock_at(0);

        let destination_error = run_archive(
            &configured,
            &scratch.path().join("some-archive"),
            &configured,
            busy_timeout(),
            &mounts(),
            &clock,
        )
        .unwrap_err();
        assert!(
            destination_error
                .to_string()
                .contains("configured state directory"),
            "{destination_error}"
        );

        let source_error = run_archive(
            &configured,
            &configured,
            &scratch.path().join("restored"),
            busy_timeout(),
            &mounts(),
            &clock,
        )
        .unwrap_err();
        assert!(
            source_error
                .to_string()
                .contains("configured state directory"),
            "{source_error}"
        );
    }

    // --- the real-archive mode records a durable result --------------------

    #[test]
    fn a_real_archive_drill_recovers_and_its_record_makes_doctor_see_a_fresh_drill() {
        let scratch = ScratchDir::new("archive-mode");
        let configured = scratch.path().join("unused-configured-state-dir");

        // Build a real archive independently of the drill, the way a nightly
        // backup would produce one.
        let seed_dir = scratch.path().join("seed");
        let clock = clock_at(10_000);
        build_seed_state_dir(&seed_dir, &clock).unwrap();
        let archive_dir = scratch.path().join("archive");
        create_archive(&seed_dir, &archive_dir, busy_timeout(), &clock).unwrap();

        let scratch_dest = scratch.path().join("scratch");
        let report = run_archive(
            &configured,
            &archive_dir,
            &scratch_dest,
            busy_timeout(),
            &mounts(),
            &clock,
        )
        .unwrap();
        assert!(report.passed());
        assert_eq!(report.restore.observation_count.value(), 1);

        let result_path = scratch.path().join("drill-result.jsonl");
        record_run(&result_path, &DrillRunRecord::from_report(&report)).unwrap();

        // The durable record names the source it used, not just that a run
        // happened: read the raw appended line back rather than trusting
        // drill_health, which does not surface this field.
        let raw = fs::read_to_string(&result_path).unwrap();
        assert!(
            raw.contains(&format!("archive:{}", archive_dir.display())),
            "{raw}"
        );
        assert!(raw.contains("\"passed\":true"), "{raw}");

        let later =
            UtcTimestamp::from_unix_nanos(clock_at(10_000).now().unix_nanos() + 60_000_000_000);
        let health =
            drill_health(&result_path, later, MonotonicDuration::from_seconds(3600)).unwrap();
        match health {
            DrillHealth::Verified {
                drilled_at,
                review_due,
                ..
            } => {
                assert_eq!(drilled_at, report.drilled_at);
                assert!(!review_due, "60s old is well within a one-hour max_age");
            }
            DrillHealth::Missing => panic!("a recorded, passing run must not read as missing"),
        }
    }

    #[test]
    fn a_failed_drill_run_is_never_written_because_the_command_errors_out_first() {
        // A restore that itself fails never reaches `record_run`: the error
        // propagates out of `run_archive`/`run_seeded` before a `DrillReport`
        // exists at all, so there is no "failed" record to accidentally
        // treat as a successful one. This is the same guarantee
        // `restore_archive` already gives; the test pins it at this layer.
        let scratch = ScratchDir::new("no-record-on-error");
        let configured = scratch.path().join("aub");
        let missing_archive = scratch.path().join("does-not-exist");
        let clock = clock_at(0);
        let error = run_archive(
            &configured,
            &missing_archive,
            &scratch.path().join("restored"),
            busy_timeout(),
            &mounts(),
            &clock,
        )
        .unwrap_err();
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn drill_health_reports_missing_when_no_result_file_exists() {
        let scratch = ScratchDir::new("health-missing");
        let health = drill_health(
            &scratch.path().join("never-written.jsonl"),
            UtcTimestamp::from_unix_nanos(0),
            MonotonicDuration::from_seconds(3600),
        )
        .unwrap();
        assert_eq!(health, DrillHealth::Missing);
    }

    /// Mutation-shaped proof: a record whose `passed` is `false` must never
    /// be read back as the last successful drill.
    #[test]
    fn drill_health_ignores_a_failed_run_and_falls_back_to_missing() {
        let scratch = ScratchDir::new("health-failed-only");
        let result_path = scratch.path().join("drill-result.jsonl");
        fs::create_dir_all(scratch.path()).unwrap();
        record_run(
            &result_path,
            &DrillRunRecord {
                drilled_at: UtcTimestamp::from_unix_nanos(1_000_000_000),
                source: "archive:/tmp/whatever".into(),
                scratch_destination: PathBuf::from("/tmp/whatever-scratch"),
                passed: false,
            },
        )
        .unwrap();
        let health = drill_health(
            &result_path,
            UtcTimestamp::from_unix_nanos(2_000_000_000),
            MonotonicDuration::from_seconds(3600),
        )
        .unwrap();
        assert_eq!(health, DrillHealth::Missing);
    }

    #[test]
    fn drill_health_flags_a_run_older_than_max_age() {
        let scratch = ScratchDir::new("health-stale");
        let result_path = scratch.path().join("drill-result.jsonl");
        fs::create_dir_all(scratch.path()).unwrap();
        record_run(
            &result_path,
            &DrillRunRecord {
                drilled_at: UtcTimestamp::from_unix_nanos(0),
                source: "archive:/tmp/whatever".into(),
                scratch_destination: PathBuf::from("/tmp/whatever-scratch"),
                passed: true,
            },
        )
        .unwrap();
        let health = drill_health(
            &result_path,
            UtcTimestamp::from_unix_nanos(1_000 * 1_000_000_000),
            MonotonicDuration::from_seconds(500),
        )
        .unwrap();
        match health {
            DrillHealth::Verified { review_due, .. } => {
                assert!(review_due, "1000s old must exceed a 500s max_age")
            }
            DrillHealth::Missing => panic!("a recorded run must not read as missing"),
        }
    }
}
