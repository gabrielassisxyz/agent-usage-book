//! The pending-observation spool: a durable, on-disk holding area for one
//! terminal attempt result between the moment a network response is parsed
//! and the moment it is safely committed into SQLite (`aub-sth.10`,
//! PLAN.md sections 13, 34.7, 38).
//!
//! The window this closes is narrow and real: the request succeeded, the
//! result is parsed, and SQLite is busy or the process dies before the
//! commit. Without a durable intermediate, that is an irreplaceable
//! observation destroyed by a lock timeout.
//!
//! The write sequence is write a new file, fsync it, atomically rename it
//! into the pending directory, fsync the directory where the platform
//! supports it. Only once renamed is a record discoverable by
//! [`drain_pending`]; a crash before the rename leaves an unfsynced or
//! unfinished temp file that drain never looks at, so a torn write is never
//! mistaken for a durable pending record. A crash after the SQLite commit
//! and before the pending file is deleted replays a record whose evidence is
//! already present, which is why replay is keyed on the attempt identifier
//! and is idempotent by construction rather than by a check somebody
//! remembered to write.
//!
//! The pending record holds the terminal attempt result's normalized,
//! typed fields (the sanitized response-evidence capsule and the
//! interpretation and windows needed to commit the complete terminal
//! bundle). It never holds a raw provider HTTP body or credential material:
//! those carry account and request information the ledger does not need,
//! and the design declines to spool them.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::domain::ids::{AdapterVersion, MeterSemanticsId, ProviderContractId};
use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
use crate::domain::time::UtcTimestamp;
use crate::domain::window::{
    MeterWindow, ModelId, NominalWindowDuration, ReportedResolution, WindowScope, WindowSemanticKey,
};
use crate::error::Error;
use crate::store::account::AccountId;
use crate::store::meter_attempt::{
    MeterAttemptRowId, NewMeterAttemptResult, attempt_outcome_as_sql, attempt_outcome_from_sql,
    outcome_failure_fields,
};
use crate::store::meter_evidence::{
    NewMeterResponseEvidence, measurement_basis_sql, quantization_sql,
};
use crate::store::repository::{
    NewMeterInterpretation, Repository, TerminalBundleIds, TerminalMeterBundle,
    commit_terminal_bundle_on_connection,
};
use crate::store::startup::{create_file_mode_0600, ensure_dir_mode_0700};

const SNAPSHOT_BARRIER_FILE: &str = ".spool-snapshot.lock";

/// An exclusive lease over the pending spool while a backup cut is captured.
/// Ordinary spool writes and drains take a shared lease, so they may proceed
/// together but cannot create, delete or quarantine a record during the cut.
pub(crate) struct StateSnapshotBarrier {
    _file: fs::File,
}

/// One pending file captured while [`StateSnapshotBarrier`] is held.
pub(crate) struct PendingRecordSnapshot {
    pub file_name: String,
    pub bytes: Vec<u8>,
}

fn open_snapshot_barrier_file(state_dir: &Path) -> Result<fs::File, Error> {
    ensure_dir_mode_0700(state_dir)?;
    create_file_mode_0600(&state_dir.join(SNAPSHOT_BARRIER_FILE))
}

fn acquire_spool_mutation_lease(state_dir: &Path) -> Result<fs::File, Error> {
    let file = open_snapshot_barrier_file(state_dir)?;
    file.lock_shared().map_err(|error| {
        Error::Store(format!(
            "cannot take the shared pending-spool lease: {error}"
        ))
    })?;
    Ok(file)
}

/// Prevents spool creation, deletion and quarantine moves until the returned
/// guard is dropped. The backup path holds this only across drain, SQLite
/// snapshot and pending-file capture.
pub(crate) fn acquire_state_snapshot_barrier(
    state_dir: &Path,
) -> Result<StateSnapshotBarrier, Error> {
    let file = open_snapshot_barrier_file(state_dir)?;
    file.lock().map_err(|error| {
        Error::Store(format!(
            "cannot take the exclusive pending-spool snapshot barrier: {error}"
        ))
    })?;
    Ok(StateSnapshotBarrier { _file: file })
}

/// One provider-reported quota window, as flat primitives rather than the
/// live domain types: the spool's on-disk format must survive a binary
/// upgrade even if a domain type's internal representation changes, and
/// serializing the domain types directly would leak their internals into a
/// durable file. `observation_id` is deliberately absent: it does not exist
/// until [`drain_pending`] inserts the observation this window belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingWindow {
    pub semantic_key: String,
    pub scope_kind: String,
    pub scoped_model: Option<String>,
    pub quota_used_ppm: i64,
    pub reported_resolution_ppm: i64,
    pub quantization: String,
    pub resets_at_nanos: i64,
    pub nominal_duration_nanos: i64,
}

/// The complete terminal bundle for one attempt: its result, response evidence,
/// one observation, and that observation's windows, as flat primitives (see
/// [`PendingWindow`] for why). Generated evidence and observation identifiers are
/// deliberately absent because they do not exist until [`drain_pending`] inserts
/// the bundle.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingTerminalBundle {
    pub attempt_id: i64,

    pub completed_at_nanos: i64,
    pub elapsed_nanos: i64,
    pub outcome: String,
    pub failure_class: Option<String>,
    pub retry_after_nanos: Option<i64>,
    pub sanitized_error_classification: Option<String>,
    pub retry_index: Option<i64>,
    pub clock_anomaly: bool,

    pub response_classification: String,
    pub received_at_nanos: i64,
    pub provider_observed_at_original: Option<String>,
    pub evidence_capsule: String,
    pub capsule_schema_version: String,
    pub sanitizer_version: String,
    pub capture_truncated: bool,

    pub account_id: i64,
    pub provider: String,
    pub provider_observed_at_nanos: Option<i64>,
    pub measurement_basis: String,
    pub observed_plan: Option<String>,
    pub observed_tier: Option<String>,
    pub adapter_version: String,
    pub provider_contract_id: String,
    pub meter_semantics_id: String,
    pub normalized_fingerprint: String,

    pub windows: Vec<PendingWindow>,
}

impl PendingWindow {
    fn to_json(&self) -> Value {
        json!({
            "semantic_key": self.semantic_key,
            "scope_kind": self.scope_kind,
            "scoped_model": self.scoped_model,
            "quota_used_ppm": self.quota_used_ppm,
            "reported_resolution_ppm": self.reported_resolution_ppm,
            "quantization": self.quantization,
            "resets_at_nanos": self.resets_at_nanos,
            "nominal_duration_nanos": self.nominal_duration_nanos,
        })
    }

    fn from_value(value: &Value) -> Result<Self, String> {
        Ok(Self {
            semantic_key: required_str(value, "semantic_key")?,
            scope_kind: required_str(value, "scope_kind")?,
            scoped_model: optional_str(value, "scoped_model"),
            quota_used_ppm: required_i64(value, "quota_used_ppm")?,
            reported_resolution_ppm: required_i64(value, "reported_resolution_ppm")?,
            quantization: required_str(value, "quantization")?,
            resets_at_nanos: required_i64(value, "resets_at_nanos")?,
            nominal_duration_nanos: required_i64(value, "nominal_duration_nanos")?,
        })
    }
}

impl PendingTerminalBundle {
    /// Renders the bundle to its durable JSON form.
    pub fn to_json(&self) -> String {
        let value = json!({
            "attempt_id": self.attempt_id,
            "completed_at_nanos": self.completed_at_nanos,
            "elapsed_nanos": self.elapsed_nanos,
            "outcome": self.outcome,
            "failure_class": self.failure_class,
            "retry_after_nanos": self.retry_after_nanos,
            "sanitized_error_classification": self.sanitized_error_classification,
            "retry_index": self.retry_index,
            "clock_anomaly": self.clock_anomaly,
            "response_classification": self.response_classification,
            "received_at_nanos": self.received_at_nanos,
            "provider_observed_at_original": self.provider_observed_at_original,
            "evidence_capsule": self.evidence_capsule,
            "capsule_schema_version": self.capsule_schema_version,
            "sanitizer_version": self.sanitizer_version,
            "capture_truncated": self.capture_truncated,
            "account_id": self.account_id,
            "provider": self.provider,
            "provider_observed_at_nanos": self.provider_observed_at_nanos,
            "measurement_basis": self.measurement_basis,
            "observed_plan": self.observed_plan,
            "observed_tier": self.observed_tier,
            "adapter_version": self.adapter_version,
            "provider_contract_id": self.provider_contract_id,
            "meter_semantics_id": self.meter_semantics_id,
            "normalized_fingerprint": self.normalized_fingerprint,
            "windows": self.windows.iter().map(PendingWindow::to_json).collect::<Vec<_>>(),
        });
        value.to_string()
    }

    /// Parses a bundle back from its durable JSON form. A parse failure or a
    /// missing/mistyped required field is reported as a human-readable
    /// reason rather than [`Error`]: the caller's response to a malformed
    /// pending record is to quarantine it, not to propagate a store failure.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let value: Value =
            serde_json::from_str(text).map_err(|error| format!("invalid JSON: {error}"))?;
        let windows = value
            .get("windows")
            .and_then(Value::as_array)
            .ok_or_else(|| "missing or non-array field \"windows\"".to_owned())?
            .iter()
            .map(PendingWindow::from_value)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            attempt_id: required_i64(&value, "attempt_id")?,
            completed_at_nanos: required_i64(&value, "completed_at_nanos")?,
            elapsed_nanos: required_i64(&value, "elapsed_nanos")?,
            outcome: required_str(&value, "outcome")?,
            failure_class: optional_str(&value, "failure_class"),
            retry_after_nanos: optional_i64(&value, "retry_after_nanos")?,
            sanitized_error_classification: optional_str(&value, "sanitized_error_classification"),
            retry_index: optional_i64(&value, "retry_index")?,
            clock_anomaly: value
                .get("clock_anomaly")
                .and_then(Value::as_bool)
                .ok_or_else(|| "missing or non-boolean field \"clock_anomaly\"".to_owned())?,
            response_classification: required_str(&value, "response_classification")?,
            received_at_nanos: required_i64(&value, "received_at_nanos")?,
            provider_observed_at_original: optional_str(&value, "provider_observed_at_original"),
            evidence_capsule: required_str(&value, "evidence_capsule")?,
            capsule_schema_version: required_str(&value, "capsule_schema_version")?,
            sanitizer_version: required_str(&value, "sanitizer_version")?,
            capture_truncated: value
                .get("capture_truncated")
                .and_then(Value::as_bool)
                .ok_or_else(|| "missing or non-boolean field \"capture_truncated\"".to_owned())?,
            account_id: required_i64(&value, "account_id")?,
            provider: required_str(&value, "provider")?,
            provider_observed_at_nanos: value
                .get("provider_observed_at_nanos")
                .and_then(Value::as_i64),
            measurement_basis: required_str(&value, "measurement_basis")?,
            observed_plan: optional_str(&value, "observed_plan"),
            observed_tier: optional_str(&value, "observed_tier"),
            adapter_version: required_str(&value, "adapter_version")?,
            provider_contract_id: required_str(&value, "provider_contract_id")?,
            meter_semantics_id: required_str(&value, "meter_semantics_id")?,
            normalized_fingerprint: required_str(&value, "normalized_fingerprint")?,
            windows,
        })
    }
}

fn required_str(value: &Value, field: &str) -> Result<String, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("missing or non-string field {field:?}"))
}

impl PendingTerminalBundle {
    /// The flat durable form of a terminal bundle the live path already holds
    /// as domain types: the exact inverse of the reconstruction a drain
    /// performs, so a bundle spooled now and drained later round-trips into
    /// the same commit. The conversion is infallible because the bundle's
    /// fields are already valid domain values; only the flattening happens
    /// here, and the outcome's failure fields go through the one mapping the
    /// live insert uses (`outcome_failure_fields`), so the two durable
    /// spellings of one outcome cannot drift apart.
    pub fn from_bundle(bundle: &TerminalMeterBundle) -> Self {
        let result = bundle.result();
        let evidence = bundle.evidence();
        let interpretation = bundle.interpretation();
        let (failure_class, retry_after_nanos) = outcome_failure_fields(&result.outcome);
        Self {
            attempt_id: result.attempt_id.value(),
            completed_at_nanos: result.completed_at.unix_nanos(),
            elapsed_nanos: result.elapsed.as_nanos() as i64,
            outcome: attempt_outcome_as_sql(&result.outcome).to_owned(),
            failure_class,
            retry_after_nanos,
            sanitized_error_classification: result.sanitized_error_classification.clone(),
            retry_index: result.retry_index.map(i64::from),
            clock_anomaly: result.clock_anomaly,
            response_classification: evidence.response_classification.clone(),
            received_at_nanos: evidence.received_at.unix_nanos(),
            provider_observed_at_original: evidence.provider_observed_at_original.clone(),
            evidence_capsule: evidence.evidence_capsule.clone(),
            capsule_schema_version: evidence.capsule_schema_version.clone(),
            sanitizer_version: evidence.sanitizer_version.clone(),
            capture_truncated: evidence.capture_truncated,
            account_id: interpretation.account_id.value(),
            provider: interpretation.provider.clone(),
            provider_observed_at_nanos: interpretation
                .provider_observed_at
                .map(|at| at.unix_nanos()),
            measurement_basis: measurement_basis_sql::as_sql(interpretation.measurement_basis)
                .to_owned(),
            observed_plan: interpretation.observed_plan.clone(),
            observed_tier: interpretation.observed_tier.clone(),
            adapter_version: interpretation.adapter_version.as_str().to_owned(),
            provider_contract_id: interpretation.provider_contract_id.as_str().to_owned(),
            meter_semantics_id: interpretation.meter_semantics_id.as_str().to_owned(),
            normalized_fingerprint: interpretation.normalized_fingerprint.clone(),
            windows: bundle
                .windows()
                .iter()
                .map(pending_window_from_window)
                .collect(),
        }
    }
}

/// One window's flat durable form, the inverse of [`reconstruct_window`].
fn pending_window_from_window(window: &MeterWindow) -> PendingWindow {
    let (scope_kind, scoped_model) = match window.scope() {
        WindowScope::AccountWide => ("account_wide".to_owned(), None),
        WindowScope::ModelSpecific(model) => {
            ("model_specific".to_owned(), Some(model.as_str().to_owned()))
        }
    };
    PendingWindow {
        semantic_key: window.semantic_key().as_str().to_owned(),
        scope_kind,
        scoped_model,
        quota_used_ppm: i64::from(window.quota_used().as_ppm().get()),
        reported_resolution_ppm: i64::from(window.reported_resolution().as_ppm().get()),
        quantization: quantization_sql::as_sql(window.quantization()).to_owned(),
        resets_at_nanos: window.resets_at().unix_nanos(),
        nominal_duration_nanos: window.nominal_duration().as_nanos() as i64,
    }
}

fn optional_str(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(Value::as_str).map(str::to_owned)
}

fn required_i64(value: &Value, field: &str) -> Result<i64, String> {
    value
        .get(field)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("missing or non-integer field {field:?}"))
}

fn optional_i64(value: &Value, field: &str) -> Result<Option<i64>, String> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| format!("non-integer field {field:?}")),
    }
}

/// The directory pending records are spooled into, inside the state
/// directory.
pub fn pending_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("pending")
}

/// Where a pending record that cannot be applied is moved, alongside its
/// reason (`<name>.reason`), preserving the evidence for manual recovery
/// rather than discarding it.
pub fn quarantine_dir(state_dir: &Path) -> PathBuf {
    pending_dir(state_dir).join("quarantine")
}

pub fn pending_file_path(state_dir: &Path, attempt_id: i64) -> PathBuf {
    pending_dir(state_dir).join(format!("attempt-{attempt_id}.json"))
}

fn temp_file_path(state_dir: &Path, attempt_id: i64) -> PathBuf {
    pending_dir(state_dir).join(format!(
        ".tmp-attempt-{attempt_id}-{}.json",
        std::process::id()
    ))
}

/// Write-sequence step 1: writes the bundle's JSON bytes to a fresh temp file
/// inside the pending directory (created at mode 0700 if absent). A crash
/// here, or before [`fsync_temp_file`] runs, leaves only this unfsynced temp
/// file: [`drain_pending`] never looks at `.tmp-*` names, so the write is
/// never mistaken for a durable record.
fn write_temp_file(
    state_dir: &Path,
    bundle: &PendingTerminalBundle,
) -> Result<(fs::File, PathBuf), Error> {
    let dir = pending_dir(state_dir);
    ensure_dir_mode_0700(&dir)?;
    let temp_path = temp_file_path(state_dir, bundle.attempt_id);
    let mut file = create_file_mode_0600(&temp_path)?;
    file.write_all(bundle.to_json().as_bytes())
        .map_err(|error| {
            Error::Store(format!(
                "cannot write the pending record {temp_path:?}: {error}"
            ))
        })?;
    Ok((file, temp_path))
}

/// Write-sequence step 2: fsyncs the temp file's contents. A crash after this
/// point and before [`rename_into_place`] still leaves only the temp file.
fn fsync_temp_file(file: &fs::File, temp_path: &Path) -> Result<(), Error> {
    file.sync_all().map_err(|error| {
        Error::Store(format!(
            "cannot fsync the pending record {temp_path:?}: {error}"
        ))
    })
}

/// Write-sequence step 3: atomically renames the fsynced temp file into its
/// final name, the first point at which [`drain_pending`] can discover it.
/// POSIX guarantees a same-filesystem rename is atomic: a reader never
/// observes a partial name or partial content once this call returns.
fn rename_into_place(
    state_dir: &Path,
    attempt_id: i64,
    temp_path: &Path,
) -> Result<PathBuf, Error> {
    let final_path = pending_file_path(state_dir, attempt_id);
    fs::rename(temp_path, &final_path).map_err(|error| {
        Error::Store(format!(
            "cannot rename the pending record {temp_path:?} into place: {error}"
        ))
    })?;
    Ok(final_path)
}

/// Write-sequence step 4: fsyncs the pending directory itself, where the
/// platform supports opening a directory for fsync, so the rename's
/// directory-entry update survives a crash immediately after.
#[cfg(unix)]
fn fsync_pending_dir(state_dir: &Path) -> Result<(), Error> {
    let dir = pending_dir(state_dir);
    fs::File::open(&dir)
        .and_then(|handle| handle.sync_all())
        .map_err(|error| {
            Error::Store(format!(
                "cannot fsync the pending directory {dir:?}: {error}"
            ))
        })
}

#[cfg(not(unix))]
fn fsync_pending_dir(_state_dir: &Path) -> Result<(), Error> {
    Ok(())
}

/// Spools one terminal bundle durably: write, fsync, atomic rename, fsync
/// directory, in that order. Returns only once the record is discoverable by
/// [`drain_pending`] on this or a future process.
pub fn spool_pending(state_dir: &Path, bundle: &PendingTerminalBundle) -> Result<(), Error> {
    let _lease = acquire_spool_mutation_lease(state_dir)?;
    let (file, temp_path) = write_temp_file(state_dir, bundle)?;
    fsync_temp_file(&file, &temp_path)?;
    drop(file);
    rename_into_place(state_dir, bundle.attempt_id, &temp_path)?;
    fsync_pending_dir(state_dir)
}

/// What [`spool_then_commit`] did with one terminal bundle, and how long the
/// commit waited for the writer slot before it could run (or give up).
#[derive(Debug)]
pub enum SpoolCycleOutcome {
    /// The bundle committed into SQLite and the pending record was deleted:
    /// PLAN.md section 13's steps 5 through 7 completed. `commit_wait` is how
    /// long the commit call took, which under contention is dominated by the
    /// wait for the writer slot.
    Committed {
        ids: TerminalBundleIds,
        commit_wait: crate::domain::time::MonotonicDuration,
    },
    /// The bundle could not commit (the writer slot stayed held past the
    /// caller's busy bound, or the database refused), so the record remains
    /// durably spooled and the next drain applies it. The error is returned
    /// inside the outcome rather than propagated, because the spool means the
    /// evidence is safe: the caller reports the store failure without the
    /// observation being lost.
    LeftPending {
        error: Error,
        commit_wait: crate::domain::time::MonotonicDuration,
    },
}

/// The meter evidence cycle the sampling flow is specified as (PLAN.md
/// section 13, steps 5 to 7): spool the terminal bundle durably, commit it
/// through the same atomic repository boundary live sampling uses, and delete
/// the pending record only once the commit is durable. A commit that cannot
/// run leaves the record in the spool, which is the whole point: an
/// irreplaceable meter result never disappears because SQLite was busy, and a
/// crash between the spool and the commit leaves exactly the state a drain
/// recovers once. A commit that succeeds removes the record, so replay after
/// a post-commit crash finds evidence already present and deletes the file as
/// a no-op.
pub fn spool_then_commit(
    repository: &Repository,
    bundle: &TerminalMeterBundle,
    clock: &impl crate::domain::time::Clock,
) -> Result<SpoolCycleOutcome, Error> {
    let state_dir = repository
        .database_path()
        .parent()
        .unwrap_or_else(|| Path::new(""));
    spool_pending(state_dir, &PendingTerminalBundle::from_bundle(bundle))?;
    let commit_start = clock.monotonic_now();
    match repository.commit_terminal_bundle(bundle) {
        Ok(commit) => {
            let commit_wait = clock.monotonic_now().duration_since(commit_start);
            remove_pending_file(&pending_file_path(
                state_dir,
                bundle.result().attempt_id.value(),
            ))?;
            Ok(SpoolCycleOutcome::Committed {
                ids: commit.ids,
                commit_wait,
            })
        }
        Err(error) => Ok(SpoolCycleOutcome::LeftPending {
            error,
            commit_wait: clock.monotonic_now().duration_since(commit_start),
        }),
    }
}

/// What happened to one pending record during a drain pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainOutcome {
    /// Applied to SQLite for the first time.
    Applied,
    /// Evidence for this attempt was already in SQLite; the file was deleted
    /// without writing anything, per the idempotent-replay contract.
    AlreadyApplied,
    /// The record could not be parsed or reconstructed and was moved to
    /// quarantine with its reason.
    Quarantined,
}

/// How many pending records a [`drain_pending`] pass disposed of, and how,
/// together with the publication that followed the pass. The publication is
/// unconditional: draining is spool recovery, which is a projection-relevant
/// change when it applies anything, and a pass that applies nothing still
/// refreshes a projection a crash may have left older than the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainReport {
    pub applied: usize,
    pub already_applied: usize,
    pub quarantined: usize,
    pub publication: crate::projection::Publication,
}

/// Drains every pending record into `conn`: applies it if its attempt has no
/// evidence yet, deletes it as a no-op if it does (idempotent replay), or
/// moves it to quarantine if it cannot even be parsed or reconstructed. A
/// record that fails to apply for an infrastructure reason (SQLite busy or
/// unavailable) is left exactly where it was and that error is returned,
/// rather than being discarded or quarantined: the caller's contract is to
/// report a store-failure class and let a later drain retry it.
pub fn drain_pending(conn: &mut Connection, state_dir: &Path) -> Result<DrainReport, Error> {
    let _lease = acquire_spool_mutation_lease(state_dir)?;
    drain_pending_while_snapshot_barrier_held(conn, state_dir)
}

/// The drain implementation used after a caller has already excluded spool
/// mutations with [`StateSnapshotBarrier`].
pub(crate) fn drain_pending_while_snapshot_barrier_held(
    conn: &mut Connection,
    state_dir: &Path,
) -> Result<DrainReport, Error> {
    let dir = pending_dir(state_dir);
    let mut applied = 0;
    let mut already_applied = 0;
    let mut quarantined = 0;
    if dir.exists() {
        let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
            .map_err(|error| {
                Error::Store(format!(
                    "cannot list the pending directory {dir:?}: {error}"
                ))
            })?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| is_pending_record_name(path))
            .collect();
        // Deterministic order, oldest attempt first: filenames are `attempt-<id>.json`
        // and a lexical sort over that shape sorts attempt ids ascending for any
        // run whose ids share a digit width, which every test and every real
        // run within one spool generation does.
        entries.sort();

        for path in entries {
            match drain_one(conn, state_dir, &path)? {
                DrainOutcome::Applied => applied += 1,
                DrainOutcome::AlreadyApplied => already_applied += 1,
                DrainOutcome::Quarantined => quarantined += 1,
            }
        }
    }
    // The pass's committed state is what the projection now describes. Every
    // applied record advanced the ledger generation inside its own commit;
    // publication after the pass covers all of them, and refreshes the file
    // even when the pass applied nothing, repairing what a crash left older.
    let publication =
        crate::projection::publish(conn, &crate::projection::projection_path_in(state_dir));
    Ok(DrainReport {
        applied,
        already_applied,
        quarantined,
        publication,
    })
}

/// Reads the pending files that remain after the best-effort drain while the
/// snapshot barrier is still held. The barrier argument makes it impossible to
/// call this helper without demonstrating that deletion and rotation are
/// excluded for the duration of the read.
pub(crate) fn snapshot_pending_records(
    state_dir: &Path,
    _barrier: &StateSnapshotBarrier,
) -> Result<Vec<PendingRecordSnapshot>, Error> {
    let dir = pending_dir(state_dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .map_err(|error| {
            Error::Store(format!(
                "cannot list the pending directory {dir:?}: {error}"
            ))
        })?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| is_pending_record_name(path))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    Error::Store(format!("pending record {path:?} has no UTF-8 file name"))
                })?
                .to_owned();
            let bytes = fs::read(&path).map_err(|error| {
                Error::Store(format!("cannot read the pending record {path:?}: {error}"))
            })?;
            Ok(PendingRecordSnapshot { file_name, bytes })
        })
        .collect()
}

/// Validates an archived pending record against both its durable JSON shape and
/// the live domain constraints that a drain would enforce.
pub fn validate_pending_record(bytes: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(bytes).map_err(|error| format!("invalid UTF-8: {error}"))?;
    let bundle = PendingTerminalBundle::from_json(text)?;
    reconstruct(&bundle).map(|_| ())
}

pub fn is_pending_record_name(path: &Path) -> bool {
    path.is_file()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("attempt-") && name.ends_with(".json"))
}

fn drain_one(conn: &mut Connection, state_dir: &Path, path: &Path) -> Result<DrainOutcome, Error> {
    let text = fs::read_to_string(path).map_err(|error| {
        Error::Store(format!("cannot read the pending record {path:?}: {error}"))
    })?;
    let bundle = match PendingTerminalBundle::from_json(&text) {
        Ok(bundle) => bundle,
        Err(reason) => {
            quarantine(state_dir, path, &reason)?;
            return Ok(DrainOutcome::Quarantined);
        }
    };

    if evidence_exists_for_attempt(conn, bundle.attempt_id)? {
        remove_pending_file(path)?;
        return Ok(DrainOutcome::AlreadyApplied);
    }

    let reconstructed = match reconstruct(&bundle) {
        Ok(reconstructed) => reconstructed,
        Err(reason) => {
            quarantine(state_dir, path, &reason)?;
            return Ok(DrainOutcome::Quarantined);
        }
    };

    apply(conn, &reconstructed)?;
    remove_pending_file(path)?;
    Ok(DrainOutcome::Applied)
}

fn remove_pending_file(path: &Path) -> Result<(), Error> {
    fs::remove_file(path).map_err(|error| {
        Error::Store(format!(
            "cannot remove the pending record {path:?}: {error}"
        ))
    })
}

fn evidence_exists_for_attempt(conn: &Connection, attempt_id: i64) -> Result<bool, Error> {
    conn.query_row(
        "SELECT 1 FROM meter_response_evidence WHERE attempt_id = ?1 LIMIT 1",
        params![attempt_id],
        |_| Ok(()),
    )
    .optional()
    .map(|found| found.is_some())
    .map_err(|error| {
        Error::Store(format!(
            "cannot check for existing evidence for attempt {attempt_id}: {error}"
        ))
    })
}

/// The bundle's fields reconstructed into their live domain and store types,
/// the reverse of the flattening [`PendingTerminalBundle`] performs. Kept
/// separate from [`apply`] so a reconstruction failure (a value the domain
/// types refuse) is distinguishable from a SQLite failure: the former means
/// the record is malformed and belongs in quarantine, the latter means the
/// record is fine and SQLite is the problem.
fn reconstruct(bundle: &PendingTerminalBundle) -> Result<TerminalMeterBundle, String> {
    if bundle.elapsed_nanos < 0 {
        return Err(format!(
            "elapsed_nanos {} must be non-negative",
            bundle.elapsed_nanos
        ));
    }
    let retry_index = bundle
        .retry_index
        .map(u32::try_from)
        .transpose()
        .map_err(|_| format!("retry_index {:?} is out of range", bundle.retry_index))?;
    let (outcome, _) = attempt_outcome_from_sql(
        &bundle.outcome,
        bundle.failure_class.clone(),
        bundle.retry_after_nanos,
    )
    .map_err(|error| error.to_string())?;
    let result = NewMeterAttemptResult {
        attempt_id: MeterAttemptRowId::new(bundle.attempt_id),
        completed_at: UtcTimestamp::from_unix_nanos(bundle.completed_at_nanos),
        elapsed: crate::domain::time::MonotonicDuration::from_nanos(bundle.elapsed_nanos as u64),
        outcome,
        sanitized_error_classification: bundle.sanitized_error_classification.clone(),
        retry_index,
        clock_anomaly: bundle.clock_anomaly,
    };
    let evidence = NewMeterResponseEvidence {
        attempt_id: MeterAttemptRowId::new(bundle.attempt_id),
        response_classification: bundle.response_classification.clone(),
        received_at: UtcTimestamp::from_unix_nanos(bundle.received_at_nanos),
        provider_observed_at_original: bundle.provider_observed_at_original.clone(),
        evidence_capsule: bundle.evidence_capsule.clone(),
        capsule_schema_version: bundle.capsule_schema_version.clone(),
        sanitizer_version: bundle.sanitizer_version.clone(),
        capture_truncated: bundle.capture_truncated,
    };

    let measurement_basis =
        measurement_basis_sql::from_sql(&bundle.measurement_basis).map_err(|error| {
            format!(
                "invalid measurement_basis {:?}: {error}",
                bundle.measurement_basis
            )
        })?;

    let interpretation = NewMeterInterpretation {
        account_id: AccountId::new(bundle.account_id),
        provider: bundle.provider.clone(),
        provider_observed_at: bundle
            .provider_observed_at_nanos
            .map(UtcTimestamp::from_unix_nanos),
        received_at: UtcTimestamp::from_unix_nanos(bundle.received_at_nanos),
        measurement_basis,
        observed_plan: bundle.observed_plan.clone(),
        observed_tier: bundle.observed_tier.clone(),
        adapter_version: AdapterVersion::new(bundle.adapter_version.clone()),
        provider_contract_id: ProviderContractId::new(bundle.provider_contract_id.clone()),
        meter_semantics_id: MeterSemanticsId::new(bundle.meter_semantics_id.clone()),
        normalized_fingerprint: bundle.normalized_fingerprint.clone(),
    };

    let windows = bundle
        .windows
        .iter()
        .map(reconstruct_window)
        .collect::<Result<Vec<_>, _>>()?;

    TerminalMeterBundle::new(result, evidence, interpretation, windows)
        .map_err(|error| error.to_string())
}

fn reconstruct_window(window: &PendingWindow) -> Result<MeterWindow, String> {
    let scope = match (window.scope_kind.as_str(), &window.scoped_model) {
        ("account_wide", None) => WindowScope::AccountWide,
        ("model_specific", Some(model)) => WindowScope::ModelSpecific(ModelId::new(model.clone())),
        (kind, model) => {
            return Err(format!(
                "inconsistent window scope: kind {kind:?} with model {model:?}"
            ));
        }
    };
    let quantization = quantization_sql::from_sql(&window.quantization)
        .map_err(|error| format!("invalid quantization {:?}: {error}", window.quantization))?;
    let quota_used = QuotaFractionPpm::new(window.quota_used_ppm as i32)
        .map(QuotaUsed::new)
        .ok_or_else(|| format!("quota_used_ppm {} is out of range", window.quota_used_ppm))?;
    let reported_resolution = QuotaFractionPpm::new(window.reported_resolution_ppm as i32)
        .ok_or_else(|| {
            format!(
                "reported_resolution_ppm {} is out of range",
                window.reported_resolution_ppm
            )
        })
        .and_then(|ppm| {
            ReportedResolution::new(ppm)
                .ok_or_else(|| "reported_resolution_ppm must be non-zero".to_owned())
        })?;
    Ok(MeterWindow::new(
        WindowSemanticKey::new(window.semantic_key.clone()),
        scope,
        quota_used,
        reported_resolution,
        quantization,
        UtcTimestamp::from_unix_nanos(window.resets_at_nanos),
        NominalWindowDuration::from_nanos(window.nominal_duration_nanos as u64),
    ))
}

/// Commits the reconstructed result and interpretation through the same atomic
/// repository boundary used by live sampling.
fn apply(conn: &mut Connection, bundle: &TerminalMeterBundle) -> Result<(), Error> {
    commit_terminal_bundle_on_connection(conn, bundle, || Ok(())).map(|_| ())
}

fn quarantine(state_dir: &Path, path: &Path, reason: &str) -> Result<(), Error> {
    let qdir = quarantine_dir(state_dir);
    ensure_dir_mode_0700(&qdir)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| Error::Store(format!("pending record {path:?} has no file name")))?;
    let dest = qdir.join(file_name);
    fs::rename(path, &dest).map_err(|error| {
        Error::Store(format!(
            "cannot quarantine the pending record {path:?}: {error}"
        ))
    })?;
    let reason_path = PathBuf::from(format!("{}.reason", dest.display()));
    fs::write(&reason_path, reason).map_err(|error| {
        Error::Store(format!(
            "cannot write the quarantine reason for {dest:?}: {error}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::{FakeClock, UtcTimestamp};
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::store::meter_attempt::attempt_outcome_as_sql;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A fresh scratch directory under the system temp dir, removed on drop.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("aub-spool-test-{}-{suffix}", std::process::id()));
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

    /// A migrated in-memory-backed scratch database, ready for
    /// `meter_response_evidence`/`meter_observation`/`meter_window` writes.
    fn migrated_conn(scratch: &ScratchDir) -> Connection {
        let policy = PragmaPolicy {
            busy_timeout: crate::domain::time::MonotonicDuration::from_millis(1000),
        };
        let mut conn = open(
            &scratch.path().join("spool-test.db"),
            AccessMode::ReadWrite,
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
        conn
    }

    /// A minimal but fully valid bundle, one window, ready to spool.
    fn sample_bundle(attempt_id: i64) -> PendingTerminalBundle {
        PendingTerminalBundle {
            attempt_id,
            completed_at_nanos: 2_000,
            elapsed_nanos: 1_000,
            outcome: attempt_outcome_as_sql(&crate::domain::attempt::AttemptOutcome::Success)
                .to_owned(),
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
                quantization: quantization_sql::as_sql(
                    crate::domain::window::QuantizationSemantics::Exact,
                )
                .to_owned(),
                resets_at_nanos: 5_000,
                nominal_duration_nanos: 18_000_000_000_000,
            }],
        }
    }

    fn seed_account(conn: &Connection) -> AccountId {
        crate::store::account::observe_account(
            conn,
            "anthropic",
            "work",
            UtcTimestamp::from_unix_nanos(0),
        )
        .unwrap()
    }

    fn seed_attempt(conn: &Connection, account: AccountId) -> i64 {
        let run = crate::store::sample_run::start_sample_run(
            conn,
            crate::store::sample_run::Trigger::Manual,
            UtcTimestamp::from_unix_nanos(0),
            "test",
        )
        .unwrap();
        let snapshot = crate::store::sampling_policy_snapshot::resolve_policy_snapshot(
            conn,
            account,
            UtcTimestamp::from_unix_nanos(0),
            &crate::store::sampling_policy_snapshot::ResolvedSamplingPolicy {
                ordinary_cadence: crate::domain::time::MonotonicDuration::from_seconds(300),
                freshness_horizon: crate::domain::time::MonotonicDuration::from_seconds(720),
                reset_edge_policy: "lead-120s".into(),
                retry_backoff_policy: "exponential-3".into(),
                command_budget: crate::domain::time::MonotonicDuration::from_seconds(8),
                policy_algorithm_version: "v1".into(),
            },
        )
        .unwrap();
        crate::store::meter_attempt::start_meter_attempt(
            conn,
            &crate::store::meter_attempt::NewMeterAttempt {
                run_id: run,
                account_id: account,
                provider: "anthropic".into(),
                request_started_at: UtcTimestamp::from_unix_nanos(0),
                credential_context_id: Some("ctx-1".into()),
                policy_snapshot_id: snapshot,
                due_at: UtcTimestamp::from_unix_nanos(0),
                due_reason: crate::store::meter_attempt::DueReason::OrdinaryCadence,
                due_basis: None,
                provider_contract_id: "endpoint-schema-v3".into(),
                meter_semantics_id: "account-5h-v2".into(),
            },
        )
        .unwrap()
        .value()
    }

    // --- to_json / from_json round trip --------------------------------

    #[test]
    fn a_bundle_round_trips_through_json_unchanged() {
        let bundle = sample_bundle(1);
        let json = bundle.to_json();
        let parsed = PendingTerminalBundle::from_json(&json).unwrap();
        assert_eq!(parsed, bundle);
    }

    /// The flat form a live caller produces (`from_bundle`) must be the exact
    /// inverse of the reconstruction a drain performs, or a bundle spooled now
    /// would commit differently after recovery. The fixture below covers the
    /// fields the cycle actually carries: an unreachable outcome with a
    /// rate-limit retry delay, and one account-wide plus one model-specific
    /// window, because both scope kinds flatten differently.
    #[test]
    fn from_bundle_is_the_exact_inverse_of_reconstruction() {
        let mut pending = sample_bundle(7);
        pending.outcome = "unreachable".to_owned();
        pending.failure_class = Some("rate_limited".to_owned());
        pending.retry_after_nanos = Some(90_000);
        pending.windows.push(PendingWindow {
            semantic_key: "seven_day".to_owned(),
            scope_kind: "model_specific".to_owned(),
            scoped_model: Some("claude-sonnet-4".to_owned()),
            quota_used_ppm: 400_000,
            reported_resolution_ppm: 10_000,
            quantization: "rounded_to_nearest".to_owned(),
            resets_at_nanos: 6_000,
            nominal_duration_nanos: 604_800_000_000_000,
        });

        let reconstructed = reconstruct(&pending).unwrap();
        let flattened = PendingTerminalBundle::from_bundle(&reconstructed);
        assert_eq!(flattened, pending);
    }

    #[test]
    fn from_json_reports_the_missing_field_by_name_rather_than_a_generic_parse_error() {
        let broken = "{\"attempt_id\": 1, \"windows\": []}";
        let error = PendingTerminalBundle::from_json(broken).unwrap_err();
        assert!(error.contains("completed_at_nanos"), "{error}");
    }

    // --- write sequence: the four crash points --------------------------

    #[test]
    fn crash_before_rename_leaves_nothing_discoverable() {
        let scratch = ScratchDir::new();
        let bundle = sample_bundle(1);

        // Crash point 1: temp file written, never fsynced or renamed.
        let (_file, _temp_path) = write_temp_file(scratch.path(), &bundle).unwrap();
        let report = drain_pending(&mut migrated_conn(&scratch), scratch.path()).unwrap();
        assert_eq!(report.applied, 0);
        assert_eq!(report.already_applied, 0);
        assert_eq!(report.quarantined, 0);
        assert!(
            matches!(
                report.publication,
                crate::projection::Publication::Published { .. }
            ),
            "an unrenamed temp file must never be mistaken for a durable pending record, \
             and the pass still refreshes the projection"
        );
    }

    #[test]
    fn crash_after_fsync_before_rename_still_leaves_nothing_discoverable() {
        let scratch = ScratchDir::new();
        let bundle = sample_bundle(1);

        // Crash point 2: temp file fsynced, never renamed.
        let (file, temp_path) = write_temp_file(scratch.path(), &bundle).unwrap();
        fsync_temp_file(&file, &temp_path).unwrap();
        let report = drain_pending(&mut migrated_conn(&scratch), scratch.path()).unwrap();
        assert_eq!(report.applied, 0);
        assert_eq!(report.already_applied, 0);
        assert_eq!(report.quarantined, 0);
    }

    #[test]
    fn crash_after_rename_before_directory_fsync_is_recoverable() {
        let scratch = ScratchDir::new();
        let mut conn = migrated_conn(&scratch);
        let account = seed_account(&conn);
        let attempt_id = seed_attempt(&conn, account);
        let bundle = sample_bundle(attempt_id);

        // Crash point 3: renamed into place (evidence has reached the spool),
        // directory fsync never ran.
        let (file, temp_path) = write_temp_file(scratch.path(), &bundle).unwrap();
        fsync_temp_file(&file, &temp_path).unwrap();
        drop(file);
        rename_into_place(scratch.path(), bundle.attempt_id, &temp_path).unwrap();

        let report = drain_pending(&mut conn, scratch.path()).unwrap();
        assert_eq!(report.applied, 1, "the renamed record must be recoverable");
        assert!(evidence_exists_for_attempt(&conn, attempt_id).unwrap());
    }

    #[test]
    fn crash_after_directory_fsync_before_sqlite_commit_is_recoverable() {
        let scratch = ScratchDir::new();
        let mut conn = migrated_conn(&scratch);
        let account = seed_account(&conn);
        let attempt_id = seed_attempt(&conn, account);
        let bundle = sample_bundle(attempt_id);

        // Crash point 4: the full write sequence completed (spool_pending
        // itself), but the caller crashes before ever calling drain/commit.
        spool_pending(scratch.path(), &bundle).unwrap();

        let report = drain_pending(&mut conn, scratch.path()).unwrap();
        assert_eq!(report.applied, 1);
        assert!(evidence_exists_for_attempt(&conn, attempt_id).unwrap());
    }

    // --- idempotent replay ------------------------------------------------

    #[test]
    fn replaying_an_already_applied_record_is_a_no_op_and_produces_exactly_one_observation() {
        let scratch = ScratchDir::new();
        let mut conn = migrated_conn(&scratch);
        let account = seed_account(&conn);
        let attempt_id = seed_attempt(&conn, account);
        let bundle = sample_bundle(attempt_id);

        spool_pending(scratch.path(), &bundle).unwrap();
        let first = drain_pending(&mut conn, scratch.path()).unwrap();
        assert_eq!(first.applied, 1);

        // Crash point (post-commit, pre-delete) replay: recreate the exact
        // same pending file the caller would still see if it crashed after
        // SQLite's commit but before the pending file was removed.
        spool_pending(scratch.path(), &bundle).unwrap();
        let second = drain_pending(&mut conn, scratch.path()).unwrap();
        assert_eq!(second.applied, 0);
        assert_eq!(second.already_applied, 1);

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM meter_observation WHERE attempt_id = ?1",
                params![attempt_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "replaying the same record twice must produce exactly one observation"
        );
    }

    // --- quarantine ---------------------------------------------------

    #[test]
    fn a_malformed_pending_record_is_quarantined_with_its_reason_and_does_not_block_others() {
        let scratch = ScratchDir::new();
        let mut conn = migrated_conn(&scratch);
        let account = seed_account(&conn);
        let good_attempt = seed_attempt(&conn, account);

        let dir = pending_dir(scratch.path());
        ensure_dir_mode_0700(&dir).unwrap();
        fs::write(dir.join("attempt-999.json"), b"{ not json").unwrap();
        spool_pending(scratch.path(), &sample_bundle(good_attempt)).unwrap();

        let report = drain_pending(&mut conn, scratch.path()).unwrap();
        assert_eq!(report.quarantined, 1);
        assert_eq!(
            report.applied, 1,
            "a malformed record must not block another's drain"
        );

        let quarantined = quarantine_dir(scratch.path()).join("attempt-999.json");
        assert!(quarantined.exists());
        let reason = fs::read_to_string(format!("{}.reason", quarantined.display())).unwrap();
        assert!(reason.contains("invalid JSON"), "{reason}");
    }

    // --- SQLite unavailable ------------------------------------------

    #[test]
    fn when_sqlite_is_unavailable_the_pending_evidence_stays_durable_and_the_error_is_reported() {
        let scratch = ScratchDir::new();
        // A connection opened through the project's own connection module but
        // never migrated: no schema at all, so every insert must fail, the
        // same shape as SQLite being unable to service the write.
        let policy = PragmaPolicy {
            busy_timeout: crate::domain::time::MonotonicDuration::from_millis(1000),
        };
        let mut conn = open(
            &scratch.path().join("unmigrated.db"),
            AccessMode::ReadWrite,
            &policy,
        )
        .unwrap();
        let bundle = sample_bundle(1);
        spool_pending(scratch.path(), &bundle).unwrap();

        let error = drain_pending(&mut conn, scratch.path()).unwrap_err();
        assert!(
            matches!(error, Error::Store(_)),
            "an infrastructure failure must be the store-failure class: {error:?}"
        );
        assert!(
            pending_file_path(scratch.path(), 1).exists(),
            "the pending evidence must stay durable when it could not be applied"
        );
    }

    // --- drain-first ordering, via the startup funnel --------------------

    #[test]
    fn run_after_state_check_and_drain_drains_before_running_then() {
        let scratch = ScratchDir::new();
        let mut conn = migrated_conn(&scratch);
        let account = seed_account(&conn);
        let attempt_id = seed_attempt(&conn, account);
        spool_pending(scratch.path(), &sample_bundle(attempt_id)).unwrap();
        let pending_path = pending_file_path(scratch.path(), attempt_id);
        assert!(
            pending_path.exists(),
            "the record must be seeded before the call"
        );

        // `then` observes the filesystem, not `conn` (which the funnel holds
        // `&mut` for the drain): if the drain ran after `then` instead of
        // before it, the pending file would still be here when `then` looks.
        let mounts = crate::store::startup::FakeMountTable::new();
        let pending_file_still_present_during_then =
            crate::store::startup::run_after_state_check_and_drain(
                scratch.path(),
                &mounts,
                &mut conn,
                || pending_path.exists(),
            )
            .unwrap();

        assert!(
            !pending_file_still_present_during_then,
            "the pending record must already be drained by the time `then` runs"
        );
        assert!(evidence_exists_for_attempt(&conn, attempt_id).unwrap());
    }

    // --- the meter evidence cycle: spool, commit, delete ----------------

    /// The happy cycle: the commit succeeds, the pending record is deleted, and
    /// exactly one observation exists. This is PLAN.md section 13's steps 5 to
    /// 7 completing, driven through the one boundary the live sampling flow
    /// uses.
    #[test]
    fn spool_then_commit_lands_the_bundle_and_removes_the_pending_record() {
        let scratch = ScratchDir::new();
        let conn = migrated_conn(&scratch);
        let account = seed_account(&conn);
        let attempt_id = seed_attempt(&conn, account);
        let bundle = reconstruct(&sample_bundle(attempt_id)).unwrap();
        let repository = crate::store::repository::Repository::new(
            scratch.path().join("spool-test.db"),
            crate::store::connection::PragmaPolicy {
                busy_timeout: crate::domain::time::MonotonicDuration::from_millis(1_000),
            },
        );

        let outcome = spool_then_commit(
            &repository,
            &bundle,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .unwrap();
        let ids = match outcome {
            SpoolCycleOutcome::Committed { ids, .. } => ids,
            SpoolCycleOutcome::LeftPending { error, .. } => {
                panic!("the commit must succeed with no competing writer: {error}")
            }
        };
        assert!(evidence_exists_for_attempt(&conn, attempt_id).unwrap());
        assert!(
            !pending_file_path(scratch.path(), attempt_id).exists(),
            "a committed bundle must not leave its pending record behind"
        );
        let observation: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM meter_observation WHERE attempt_id = ?1",
                params![attempt_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(observation, 1);
        let _ = ids;
    }

    /// A commit that cannot take the writer slot leaves the record durably
    /// spooled, and the next drain applies it exactly once. The planted
    /// negative is the discard-on-busy path: no row may be lost because SQLite
    /// was busy, and no second observation may appear after the drain.
    #[test]
    fn a_commit_refused_by_a_held_writer_slot_spools_and_drains_exactly_once() {
        let scratch = ScratchDir::new();
        let db_path = scratch.path().join("spool-test.db");
        let mut conn = migrated_conn(&scratch);
        let account = seed_account(&conn);
        let attempt_id = seed_attempt(&conn, account);
        let bundle = reconstruct(&sample_bundle(attempt_id)).unwrap();
        let repository = crate::store::repository::Repository::new(
            &db_path,
            crate::store::connection::PragmaPolicy {
                busy_timeout: crate::domain::time::MonotonicDuration::from_millis(150),
            },
        );

        // Another writer holds the slot past the repository's busy bound.
        let mut holder = crate::store::connection::open(
            &db_path,
            crate::store::connection::AccessMode::ReadWrite,
            &crate::store::connection::PragmaPolicy {
                busy_timeout: crate::domain::time::MonotonicDuration::from_millis(150),
            },
        )
        .unwrap();
        let held = holder
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();

        // A real clock, because the property under test is a wait duration the
        // fake clock cannot produce: the refusal must be preceded by waiting.
        let outcome =
            spool_then_commit(&repository, &bundle, &crate::domain::time::RealClock::new())
                .unwrap();
        let error = match outcome {
            SpoolCycleOutcome::LeftPending { error, commit_wait } => {
                assert!(
                    commit_wait.as_nanos() > 0,
                    "the refused commit must have waited before giving up"
                );
                error
            }
            SpoolCycleOutcome::Committed { .. } => {
                panic!("a held writer slot must refuse the commit, not pass through")
            }
        };
        assert!(
            error.to_string().contains("busy") || error.to_string().contains("locked"),
            "the refusal must be the SQLite busy class: {error}"
        );
        assert!(
            pending_file_path(scratch.path(), attempt_id).exists(),
            "the refused evidence must remain durably spooled"
        );
        drop(held);
        drop(holder);

        let report = drain_pending(&mut conn, scratch.path()).unwrap();
        assert_eq!(report.applied, 1);
        let observation: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM meter_observation WHERE attempt_id = ?1",
                params![attempt_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(observation, 1, "draining must produce exactly one bundle");
    }
}
