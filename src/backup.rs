//! Consistent archival snapshots of irreplaceable state.
//!
//! May not depend on:
//! - presentation
//! - provider adapters

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::domain::time::{
    Age, Clock, ClockSkewEnvelope, MeasurementBasis, MonotonicDuration, ReceivedAt, UtcTimestamp,
    age,
};
use crate::error::Error;

pub const ARCHIVE_DATABASE_FILE: &str = "ledger.db";
pub const ARCHIVE_MANIFEST_FILE: &str = "manifest.json";
pub const ARCHIVE_CHECKSUMS_FILE: &str = "checksums.sha256";
/// Name of the newest-verified pointer file at a backup destination root.
/// The file holds the basename of the archive directory whose verification
/// passed most recently, plus a trailing newline. The name is deliberately
/// specific: a generic `latest` would collide with sibling work in this
/// package.
pub const BACKUP_SERIES_POINTER_FILE: &str = "newest-verified";
const ARCHIVE_PENDING_DIR: &str = "pending";
const ARCHIVE_FORMAT_VERSION: u32 = 1;
/// Prefix for dated archive directories under a destination root.
const BACKUP_SERIES_ARCHIVE_PREFIX: &str = "aub-backup-";
/// Prefix for the incomplete directory a series run writes before renaming
/// it to its dated name. Dot-prefixed so directory listings skip it and so
/// `backup_series_list_archives` never mistakes it for an archive.
const BACKUP_SERIES_TMP_PREFIX: &str = ".tmp-backup-";
const BACKUP_SERIES_NANOS_PER_DAY: i64 = 86_400 * 1_000_000_000;

/// The verification stage that rejected an archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationStage {
    Checksums,
    Integrity,
    ForeignKeys,
    SpoolRecords,
    Manifest,
}

impl VerificationStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Checksums => "checksums",
            Self::Integrity => "integrity",
            Self::ForeignKeys => "foreign_keys",
            Self::SpoolRecords => "spool_records",
            Self::Manifest => "manifest",
        }
    }
}

/// A completed backup or explicit verification result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupSummary {
    pub destination: PathBuf,
    pub schema_version: u32,
    pub ledger_generation: u64,
    pub pending_records: usize,
    pub drain_completed: bool,
    pub verified: bool,
}

/// Typed health fact consumed by the later doctor registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupHealth {
    Missing,
    Unverified {
        created_at: UtcTimestamp,
    },
    Verified {
        created_at: UtcTimestamp,
        age: Age,
        review_due: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArchiveFileChecksum {
    path: String,
    sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VerificationResult {
    verified: bool,
    checked_at_unix_nanos: Option<i64>,
    integrity_check: bool,
    foreign_key_check: bool,
    spool_records_validated: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArchiveManifest {
    schema_version: u32,
    aub_version: String,
    created_at_unix_nanos: i64,
    source_ledger_generation: u64,
    drain_completed: bool,
    files: Vec<ArchiveFileChecksum>,
    pending_records: Vec<String>,
    verification: VerificationResult,
}

impl ArchiveManifest {
    fn to_json(&self) -> String {
        let value = json!({
            "format_version": ARCHIVE_FORMAT_VERSION,
            "database": ARCHIVE_DATABASE_FILE,
            "checksums": ARCHIVE_CHECKSUMS_FILE,
            "schema_version": self.schema_version,
            "aub_version": self.aub_version,
            "created_at_unix_nanos": self.created_at_unix_nanos,
            "source_ledger_generation": self.source_ledger_generation,
            "drain_completed": self.drain_completed,
            "files": self.files.iter().map(|file| json!({
                "path": file.path,
                "sha256": file.sha256,
            })).collect::<Vec<_>>(),
            "pending_records": self.pending_records,
            "verification": {
                "verified": self.verification.verified,
                "checked_at_unix_nanos": self.verification.checked_at_unix_nanos,
                "integrity_check": self.verification.integrity_check,
                "foreign_key_check": self.verification.foreign_key_check,
                "spool_records_validated": self.verification.spool_records_validated,
            },
        });
        serde_json::to_string_pretty(&value).expect("archive manifest contains only JSON values")
            + "\n"
    }

    fn from_json(text: &str) -> Result<Self, Error> {
        let value: Value = serde_json::from_str(text)
            .map_err(|error| manifest_error(format!("invalid JSON: {error}")))?;
        if required_u64(&value, "format_version")? != u64::from(ARCHIVE_FORMAT_VERSION) {
            return Err(manifest_error("unsupported format_version"));
        }
        if required_str(&value, "database")? != ARCHIVE_DATABASE_FILE
            || required_str(&value, "checksums")? != ARCHIVE_CHECKSUMS_FILE
        {
            return Err(manifest_error(
                "archive file names do not match this format",
            ));
        }

        let files = required_array(&value, "files")?
            .iter()
            .map(|entry| {
                let path = required_str(entry, "path")?;
                validate_archive_relative_path(&path)?;
                let sha256 = required_str(entry, "sha256")?;
                if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(manifest_error(format!("invalid SHA-256 for {path}")));
                }
                Ok(ArchiveFileChecksum { path, sha256 })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let pending_records = required_array(&value, "pending_records")?
            .iter()
            .map(|entry| {
                let path = entry
                    .as_str()
                    .ok_or_else(|| manifest_error("pending_records contains a non-string"))?
                    .to_owned();
                validate_pending_archive_path(&path)?;
                Ok(path)
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let verification = value
            .get("verification")
            .ok_or_else(|| manifest_error("missing verification"))?;

        Ok(Self {
            schema_version: required_u64(&value, "schema_version")?
                .try_into()
                .map_err(|_| manifest_error("schema_version is out of range"))?,
            aub_version: required_str(&value, "aub_version")?,
            created_at_unix_nanos: required_i64(&value, "created_at_unix_nanos")?,
            source_ledger_generation: required_u64(&value, "source_ledger_generation")?,
            drain_completed: required_bool(&value, "drain_completed")?,
            files,
            pending_records,
            verification: VerificationResult {
                verified: required_bool(verification, "verified")?,
                checked_at_unix_nanos: optional_i64(verification, "checked_at_unix_nanos")?,
                integrity_check: required_bool(verification, "integrity_check")?,
                foreign_key_check: required_bool(verification, "foreign_key_check")?,
                spool_records_validated: required_u64(verification, "spool_records_validated")?
                    .try_into()
                    .map_err(|_| manifest_error("spool_records_validated is out of range"))?,
            },
        })
    }
}

/// Creates a complete archive and leaves it marked verified only after every
/// logical and content check succeeds.
pub fn create_archive(
    state_dir: &Path,
    destination: &Path,
    busy_timeout: MonotonicDuration,
    clock: &dyn Clock,
) -> Result<BackupSummary, Error> {
    if destination.exists() {
        return Err(Error::Store(format!(
            "backup destination {destination:?} already exists; refusing to overwrite it"
        )));
    }
    crate::store::startup::ensure_dir_mode_0700(destination)?;
    let pending_destination = destination.join(ARCHIVE_PENDING_DIR);
    crate::store::startup::ensure_dir_mode_0700(&pending_destination)?;

    let source_database = state_dir.join(crate::store::connection::LEDGER_DATABASE_FILE);
    let destination_database = destination.join(ARCHIVE_DATABASE_FILE);
    let cut = crate::store::backup::capture_backup_cut(
        state_dir,
        &source_database,
        &destination_database,
        busy_timeout,
    )?;

    let mut pending_records = Vec::with_capacity(cut.pending_records.len());
    for record in cut.pending_records {
        validate_pending_file_name(&record.file_name)?;
        let relative = format!("{ARCHIVE_PENDING_DIR}/{}", record.file_name);
        write_file(&destination.join(&relative), &record.bytes)?;
        pending_records.push(relative);
    }
    pending_records.sort();

    let mut checksum_paths = vec![ARCHIVE_DATABASE_FILE.to_owned()];
    checksum_paths.extend(pending_records.iter().cloned());
    let files = compute_checksums(destination, &checksum_paths)?;
    write_checksum_file(destination, &files)?;

    let manifest = ArchiveManifest {
        schema_version: cut.schema_version,
        aub_version: crate::build_info::crate_version().to_owned(),
        created_at_unix_nanos: clock.now().unix_nanos(),
        source_ledger_generation: cut.ledger_generation,
        drain_completed: cut.drain_completed,
        files,
        pending_records,
        verification: unverified_result(),
    };
    write_manifest(destination, &manifest)?;
    verify_archive(destination, busy_timeout, clock)
}

/// Re-runs checksum, SQLite and spool validation against an existing archive.
///
/// Reads without writing: the database is opened side-effect-free and the
/// manifest is rewritten only when the verification result actually differs
/// from what is recorded, so a healthy archive on writable media and on a
/// read-only copy both hash identically before and after (aub-2r0n). A
/// verification that legitimately changes the result still records it, and a
/// failure is still recorded as unverified when the manifest differs; when the
/// manifest cannot be written the original verification failure is returned to
/// the caller rather than swallowed by the write error.
pub fn verify_archive(
    destination: &Path,
    busy_timeout: MonotonicDuration,
    clock: &dyn Clock,
) -> Result<BackupSummary, Error> {
    let mut manifest = read_manifest(destination)?;
    let original = manifest.verification.clone();
    match run_verification_checks(destination, &manifest, busy_timeout, clock) {
        Ok(fresh) => {
            if verification_content_equal(&original, &fresh) {
                Ok(summary(destination, &manifest))
            } else {
                manifest.verification = fresh;
                write_manifest(destination, &manifest)?;
                Ok(summary(destination, &manifest))
            }
        }
        Err(verification_failure) => {
            if original != unverified_result() {
                let mut failure_manifest = manifest.clone();
                failure_manifest.verification = unverified_result();
                let _ = write_manifest(destination, &failure_manifest);
            }
            Err(verification_failure)
        }
    }
}

/// Whether two verification results carry the same outcome, ignoring
/// `checked_at_unix_nanos`. The timestamp moves on every clock tick, so
/// comparing it would make every re-verification look changed and force a
/// rewrite; the archive's health is the verified bit plus the three checks.
fn verification_content_equal(a: &VerificationResult, b: &VerificationResult) -> bool {
    a.verified == b.verified
        && a.integrity_check == b.integrity_check
        && a.foreign_key_check == b.foreign_key_check
        && a.spool_records_validated == b.spool_records_validated
}

/// Runs every check `verify_archive` promises without touching the manifest or
/// the archived database's directory. Returns the fresh verified result on
/// success, or the stage-named verification error on the first failure.
fn run_verification_checks(
    destination: &Path,
    manifest: &ArchiveManifest,
    busy_timeout: MonotonicDuration,
    clock: &dyn Clock,
) -> Result<VerificationResult, Error> {
    verify_checksums(destination, manifest)?;
    let database = destination.join(ARCHIVE_DATABASE_FILE);
    let database_result = crate::store::backup::verify_database(&database, busy_timeout)?;
    let database_result = database_result.map_err(|failure| {
        verification_error(
            match failure.stage {
                crate::store::backup::DatabaseVerificationStage::Integrity => {
                    VerificationStage::Integrity
                }
                crate::store::backup::DatabaseVerificationStage::ForeignKeys => {
                    VerificationStage::ForeignKeys
                }
            },
            failure.detail,
        )
    })?;
    let (schema_version, ledger_generation) =
        crate::store::backup::archived_database_metadata(&database, busy_timeout)?;
    if schema_version != manifest.schema_version
        || ledger_generation != manifest.source_ledger_generation
    {
        return Err(verification_error(
            VerificationStage::Manifest,
            format!(
                "metadata mismatch: manifest schema={} generation={}, database schema={} generation={}",
                manifest.schema_version,
                manifest.source_ledger_generation,
                schema_version,
                ledger_generation,
            ),
        ));
    }

    for relative in &manifest.pending_records {
        let bytes = fs::read(destination.join(relative)).map_err(|error| {
            verification_error(
                VerificationStage::SpoolRecords,
                format!("cannot read {relative}: {error}"),
            )
        })?;
        crate::store::spool::validate_pending_record(&bytes).map_err(|detail| {
            verification_error(
                VerificationStage::SpoolRecords,
                format!("{relative}: {detail}"),
            )
        })?;
    }

    Ok(VerificationResult {
        verified: true,
        checked_at_unix_nanos: Some(clock.now().unix_nanos()),
        integrity_check: database_result.integrity_check,
        foreign_key_check: database_result.foreign_key_check,
        spool_records_validated: manifest.pending_records.len(),
    })
}

/// Reads the archive's doctor fact. An unverified archive has no age by
/// construction, so it cannot satisfy backup policy merely by being recent.
///
/// The destination may be either a single archive directory (the layout every
/// release before the series change wrote, kept readable so an operator can
/// migrate on their own schedule) or a destination root holding a series of
/// dated archives plus the newest-verified pointer. When the pointer file is
/// present the root is a series and the health comes from the archive it
/// names; a series root without a pointer reports Missing, because the
/// pointer is what makes a series readable, not a directory listing.
pub fn backup_health(
    destination: &Path,
    now: UtcTimestamp,
    review_after: MonotonicDuration,
) -> Result<BackupHealth, Error> {
    if destination.join(BACKUP_SERIES_POINTER_FILE).is_file() {
        return backup_series_pointed_health(destination, now, review_after);
    }
    if !destination.join(ARCHIVE_MANIFEST_FILE).is_file() {
        return Ok(BackupHealth::Missing);
    }
    let manifest = read_manifest(destination)?;
    backup_health_from_manifest(&manifest, now, review_after)
}

fn backup_health_from_manifest(
    manifest: &ArchiveManifest,
    now: UtcTimestamp,
    review_after: MonotonicDuration,
) -> Result<BackupHealth, Error> {
    let created_at = UtcTimestamp::from_unix_nanos(manifest.created_at_unix_nanos);
    if !manifest.verification.verified {
        return Ok(BackupHealth::Unverified { created_at });
    }
    let backup_age = age(
        None,
        ReceivedAt::new(created_at),
        MeasurementBasis::LocallyReceived,
        now,
        ClockSkewEnvelope::new(MonotonicDuration::from_nanos(0)),
    )
    .map_err(|_| Error::Store("verified backup creation timestamp is in the future".into()))?;
    Ok(BackupHealth::Verified {
        created_at,
        age: backup_age,
        review_due: backup_age.as_nanos() > review_after.as_nanos(),
    })
}

fn backup_series_pointed_health(
    root: &Path,
    now: UtcTimestamp,
    review_after: MonotonicDuration,
) -> Result<BackupHealth, Error> {
    let name = backup_series_read_pointer(root)?.ok_or_else(|| {
        Error::Store(format!(
            "backup pointer {} is missing at {}",
            BACKUP_SERIES_POINTER_FILE,
            root.display()
        ))
    })?;
    let archive = root.join(&name);
    if !archive.join(ARCHIVE_MANIFEST_FILE).is_file() {
        return Err(Error::Store(format!(
            "backup pointer names {name:?} but no verified archive exists there"
        )));
    }
    let manifest = read_manifest(&archive)?;
    backup_health_from_manifest(&manifest, now, review_after)
}

/// Tiered retention counts for a backup destination root, in the manner of
/// restic: keep the latest archive of each of the last N days, weeks, months
/// and years. An archive is retained when any bucket retains it. Counts are
/// plain `usize` values owned by configuration; this struct carries them
/// without defining their defaults, so the defaults live in exactly one
/// place (`BackupConfig` resolution).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackupSeriesRetention {
    pub keep_daily: usize,
    pub keep_weekly: usize,
    pub keep_monthly: usize,
    pub keep_yearly: usize,
}

impl BackupSeriesRetention {
    pub fn new(
        keep_daily: usize,
        keep_weekly: usize,
        keep_monthly: usize,
        keep_yearly: usize,
    ) -> Self {
        Self {
            keep_daily,
            keep_weekly,
            keep_monthly,
            keep_yearly,
        }
    }
}

/// One dated archive under a destination root, as the retention selection
/// sees it. The name is the directory basename; the timestamp and generation
/// come from the archive manifest, never from parsing the name, so a renamed
/// directory cannot mislead retention about its age.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupSeriesArchiveEntry {
    pub name: String,
    pub created_at: UtcTimestamp,
    pub source_generation: u64,
    pub verified: bool,
}

impl BackupSeriesArchiveEntry {
    pub fn new(
        name: String,
        created_at: UtcTimestamp,
        source_generation: u64,
        verified: bool,
    ) -> Self {
        Self {
            name,
            created_at,
            source_generation,
            verified,
        }
    }
}

/// What a prune run kept and what it removed, both as archive basenames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupSeriesPruneReport {
    pub retained: Vec<String>,
    pub pruned: Vec<String>,
}

/// The dated directory name for a new archive. It carries the creation
/// instant in UTC calendar form with nanosecond precision plus the source
/// ledger generation, for example
/// `aub-backup-2026-09-09T12-00-00.123456789Z-g9648`. Fixed-width fields keep
/// lexicographic order equal to chronological order; nanoseconds make two
/// runs in the same second distinct.
pub fn backup_series_archive_dir_name(created_at: UtcTimestamp, source_generation: u64) -> String {
    let nanos = created_at.unix_nanos();
    let day_nanos = nanos.rem_euclid(BACKUP_SERIES_NANOS_PER_DAY);
    let hours = day_nanos / 3_600_000_000_000;
    let after_hours = day_nanos % 3_600_000_000_000;
    let minutes = after_hours / 60_000_000_000;
    let after_minutes = after_hours % 60_000_000_000;
    let seconds = after_minutes / 1_000_000_000;
    let subsecond = after_minutes % 1_000_000_000;
    format!(
        "{}{}T{:02}-{:02}-{:02}.{:09}Z-g{}",
        BACKUP_SERIES_ARCHIVE_PREFIX,
        created_at.utc_date().iso(),
        hours,
        minutes,
        seconds,
        subsecond,
        source_generation
    )
}

/// Every archive directory directly under the root that holds a manifest,
/// newest first. Dot-prefixed staging directories and plain files (including
/// the pointer) are ignored. A missing root lists as empty rather than
/// failing, so a first backup onto a fresh path and a health check on a path
/// nothing ever wrote both start from the same observation.
pub fn backup_series_list_archives(root: &Path) -> Result<Vec<BackupSeriesArchiveEntry>, Error> {
    let read = match fs::read_dir(root) {
        Ok(read) => read,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(Error::Store(format!(
                "cannot list backup destination root {}: {error}",
                root.display()
            )));
        }
    };
    let mut entries = Vec::new();
    for entry in read {
        let entry = entry.map_err(|error| {
            Error::Store(format!(
                "cannot list backup destination root {}: {error}",
                root.display()
            ))
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        if !is_dir || !path.join(ARCHIVE_MANIFEST_FILE).is_file() {
            continue;
        }
        let manifest = read_manifest(&path)?;
        entries.push(BackupSeriesArchiveEntry::new(
            name,
            UtcTimestamp::from_unix_nanos(manifest.created_at_unix_nanos),
            manifest.source_ledger_generation,
            manifest.verification.verified,
        ));
    }
    backup_series_sort_newest_first(&mut entries);
    Ok(entries)
}

fn backup_series_sort_newest_first(entries: &mut [BackupSeriesArchiveEntry]) {
    entries.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| right.source_generation.cmp(&left.source_generation))
            .then_with(|| right.name.cmp(&left.name))
    });
}

/// The basename the pointer file names, or `None` when no pointer exists.
pub fn backup_series_read_pointer(root: &Path) -> Result<Option<String>, Error> {
    let path = root.join(BACKUP_SERIES_POINTER_FILE);
    match fs::read_to_string(&path) {
        Ok(text) => {
            let name = text.trim().to_owned();
            if name.is_empty() {
                return Err(Error::Store(format!("backup pointer {path:?} is empty")));
            }
            if name.contains(['/', '\\']) {
                return Err(Error::Store(format!(
                    "backup pointer {path:?} names an unsafe archive {name:?}"
                )));
            }
            Ok(Some(name))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::Store(format!(
            "cannot read backup pointer {path:?}: {error}"
        ))),
    }
}

fn backup_series_write_pointer(root: &Path, archive_name: &str) -> Result<(), Error> {
    write_file(
        &root.join(BACKUP_SERIES_POINTER_FILE),
        format!("{archive_name}\n").as_bytes(),
    )
}

/// Which archive basenames retention keeps. The input may be unsorted; the
/// selection sorts newest first internally. Only verified archives compete
/// for bucket slots: unverified archives are never pruned, so they need no
/// slot. The named newest verified archive is always retained even when no
/// bucket wants it. An empty input retains nothing.
pub fn backup_series_select_retained(
    archives: &[BackupSeriesArchiveEntry],
    retention: &BackupSeriesRetention,
    newest_verified_name: Option<&str>,
) -> std::collections::BTreeSet<String> {
    let mut sorted: Vec<&BackupSeriesArchiveEntry> = archives.iter().collect();
    sorted.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| right.source_generation.cmp(&left.source_generation))
            .then_with(|| right.name.cmp(&left.name))
    });
    let mut retained = std::collections::BTreeSet::new();
    if let Some(name) = newest_verified_name
        && archives.iter().any(|entry| entry.name == name)
    {
        retained.insert(name.to_owned());
    }
    let verified: Vec<&&BackupSeriesArchiveEntry> =
        sorted.iter().filter(|entry| entry.verified).collect();
    backup_series_retain_bucket(
        &verified,
        retention.keep_daily,
        |entry| {
            entry
                .created_at
                .unix_nanos()
                .div_euclid(BACKUP_SERIES_NANOS_PER_DAY)
                .to_string()
        },
        &mut retained,
    );
    backup_series_retain_bucket(
        &verified,
        retention.keep_weekly,
        |entry| {
            let days = entry
                .created_at
                .unix_nanos()
                .div_euclid(BACKUP_SERIES_NANOS_PER_DAY);
            (days - (days + 3).rem_euclid(7)).to_string()
        },
        &mut retained,
    );
    backup_series_retain_bucket(
        &verified,
        retention.keep_monthly,
        |entry| entry.created_at.utc_date().iso()[..7].to_owned(),
        &mut retained,
    );
    backup_series_retain_bucket(
        &verified,
        retention.keep_yearly,
        |entry| entry.created_at.utc_date().iso()[..4].to_owned(),
        &mut retained,
    );
    retained
}

fn backup_series_retain_bucket(
    verified_newest_first: &[&&BackupSeriesArchiveEntry],
    keep: usize,
    period: impl Fn(&BackupSeriesArchiveEntry) -> String,
    retained: &mut std::collections::BTreeSet<String>,
) {
    if keep == 0 {
        return;
    }
    let mut seen = std::collections::BTreeSet::new();
    for entry in verified_newest_first {
        if seen.len() >= keep {
            break;
        }
        let key = period(entry);
        if seen.insert(key) {
            retained.insert(entry.name.clone());
        }
    }
}

/// The basename of the most recent verified archive: the pointer target when
/// it names a verified archive that exists, otherwise the newest verified
/// archive by timestamp. `None` when nothing verified exists.
fn backup_series_newest_verified_name(
    archives: &[BackupSeriesArchiveEntry],
    pointer: Option<&str>,
) -> Option<String> {
    if let Some(name) = pointer
        && archives
            .iter()
            .any(|entry| entry.name == name && entry.verified)
    {
        return Some(name.to_owned());
    }
    archives
        .iter()
        .filter(|entry| entry.verified)
        .max_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.source_generation.cmp(&right.source_generation))
                .then_with(|| left.name.cmp(&right.name))
        })
        .map(|entry| entry.name.clone())
}

/// Deletes every verified archive retention does not keep, and reports what
/// stayed and what left. Unverified archives are always left alone. Refuses
/// with an error, deleting nothing, when the root holds no verified archive
/// at all, rather than emptying it.
pub fn backup_series_prune(
    root: &Path,
    retention: &BackupSeriesRetention,
) -> Result<BackupSeriesPruneReport, Error> {
    let archives = backup_series_list_archives(root)?;
    let verified_count = archives.iter().filter(|entry| entry.verified).count();
    if verified_count == 0 {
        return Err(Error::Store(format!(
            "refusing to prune backup destination root {}: no verified archive exists",
            root.display()
        )));
    }
    let pointer = backup_series_read_pointer(root)?;
    let newest = backup_series_newest_verified_name(&archives, pointer.as_deref());
    let retained = backup_series_select_retained(&archives, retention, newest.as_deref());
    let mut report = BackupSeriesPruneReport {
        retained: Vec::new(),
        pruned: Vec::new(),
    };
    for entry in &archives {
        if !entry.verified {
            report.retained.push(entry.name.clone());
            continue;
        }
        if retained.contains(&entry.name) {
            report.retained.push(entry.name.clone());
            continue;
        }
        if Some(entry.name.as_str()) == newest.as_deref() {
            report.retained.push(entry.name.clone());
            continue;
        }
        fs::remove_dir_all(root.join(&entry.name)).map_err(|error| {
            Error::Store(format!(
                "cannot prune backup archive {}: {error}",
                root.join(&entry.name).display()
            ))
        })?;
        report.pruned.push(entry.name.clone());
    }
    report.retained.sort();
    report.pruned.sort();
    Ok(report)
}

/// Creates a new dated archive under the destination root, advances the
/// newest-verified pointer only after verification passed, then prunes under
/// retention. The root itself may exist already; each archive directory under
/// it is still never written over. A destination root that still holds a
/// single-archive layout (a manifest directly at the root) is refused with a
/// migration message rather than nested into, because writing dated archives
/// inside an archive would make both unreadable.
///
/// A run whose verification fails leaves the previous pointer and every
/// existing archive untouched and prunes nothing: the failed cut is renamed
/// to its dated name as an unverified archive for diagnosis when it got far
/// enough to write a manifest, and removed when it did not.
pub fn backup_series_create(
    state_dir: &Path,
    root: &Path,
    retention: &BackupSeriesRetention,
    busy_timeout: MonotonicDuration,
    clock: &dyn Clock,
) -> Result<BackupSummary, Error> {
    crate::store::startup::ensure_dir_mode_0700(root)?;
    if root.join(ARCHIVE_MANIFEST_FILE).is_file() {
        return Err(Error::Store(format!(
            "backup destination root {} holds a single archive; move it aside and back up again (see docs/backup.md migration)",
            root.display()
        )));
    }
    let_tmp_dir(root, clock, |tmp| {
        match create_archive(state_dir, tmp, busy_timeout, clock) {
            Err(error) => {
                backup_series_adopt_failed_tmp(root, tmp)?;
                Err(error)
            }
            Ok(tmp_summary) => {
                if !tmp_summary.verified {
                    backup_series_adopt_failed_tmp(root, tmp)?;
                    return Ok(tmp_summary);
                }
                let manifest = read_manifest(tmp)?;
                let final_name = backup_series_archive_dir_name(
                    UtcTimestamp::from_unix_nanos(manifest.created_at_unix_nanos),
                    manifest.source_ledger_generation,
                );
                let final_path = root.join(&final_name);
                if final_path.exists() {
                    let _ = fs::remove_dir_all(tmp);
                    return Err(Error::Store(format!(
                        "backup destination {final_path:?} already exists; refusing to overwrite it"
                    )));
                }
                fs::rename(tmp, &final_path).map_err(|error| {
                    Error::Store(format!(
                        "cannot move new backup archive to {final_path:?}: {error}"
                    ))
                })?;
                backup_series_write_pointer(root, &final_name)?;
                backup_series_prune(root, retention)?;
                Ok(BackupSummary {
                    destination: final_path,
                    ..tmp_summary
                })
            }
        }
    })
}

/// Runs `body` with a fresh dot-prefixed staging directory under the root.
fn let_tmp_dir<T>(
    root: &Path,
    clock: &dyn Clock,
    body: impl FnOnce(&Path) -> Result<T, Error>,
) -> Result<T, Error> {
    let base = format!(
        "{}{}-{}",
        BACKUP_SERIES_TMP_PREFIX,
        clock.now().unix_nanos(),
        std::process::id()
    );
    let mut candidate = root.join(&base);
    let mut suffix = 0u32;
    while candidate.exists() {
        suffix += 1;
        candidate = root.join(format!("{base}-{suffix}"));
    }
    body(&candidate)
}

/// Resolves an archive path that may name either a single archive directory
/// or a destination root: a path holding a manifest is itself the archive, a
/// path holding the newest-verified pointer resolves to the archive it names,
/// and anything else is returned unchanged for the caller to report with its
/// existing manifest or verification error.
pub fn backup_resolve_archive_path(path: &Path) -> Result<PathBuf, Error> {
    if path.join(ARCHIVE_MANIFEST_FILE).is_file() {
        return Ok(path.to_path_buf());
    }
    if path.join(BACKUP_SERIES_POINTER_FILE).is_file() {
        let name = backup_series_read_pointer(path)?.ok_or_else(|| {
            Error::Store(format!("backup pointer is missing at {}", path.display()))
        })?;
        let archive = path.join(&name);
        if !archive.join(ARCHIVE_MANIFEST_FILE).is_file() {
            return Err(Error::Store(format!(
                "backup pointer names {name:?} but no verified archive exists there"
            )));
        }
        return Ok(archive);
    }
    Ok(path.to_path_buf())
}

/// After a failed cut, give the staging directory its dated name when it got
/// far enough to write a manifest, so the failure stays diagnosable, and
/// remove it when it did not, so an early failure leaves no litter. Never
/// overwrites an existing dated directory.
fn backup_series_adopt_failed_tmp(root: &Path, tmp: &Path) -> Result<(), Error> {
    if !tmp.exists() {
        return Ok(());
    }
    let manifest = match read_manifest(tmp) {
        Ok(manifest) => manifest,
        Err(_) => {
            let _ = fs::remove_dir_all(tmp);
            return Ok(());
        }
    };
    let final_name = backup_series_archive_dir_name(
        UtcTimestamp::from_unix_nanos(manifest.created_at_unix_nanos),
        manifest.source_ledger_generation,
    );
    let final_path = root.join(&final_name);
    if final_path.exists() {
        let _ = fs::remove_dir_all(tmp);
        return Ok(());
    }
    fs::rename(tmp, &final_path).map_err(|error| {
        Error::Store(format!(
            "cannot move failed backup archive to {final_path:?}: {error}"
        ))
    })
}

fn summary(destination: &Path, manifest: &ArchiveManifest) -> BackupSummary {
    BackupSummary {
        destination: destination.to_path_buf(),
        schema_version: manifest.schema_version,
        ledger_generation: manifest.source_ledger_generation,
        pending_records: manifest.pending_records.len(),
        drain_completed: manifest.drain_completed,
        verified: manifest.verification.verified,
    }
}

fn unverified_result() -> VerificationResult {
    VerificationResult {
        verified: false,
        checked_at_unix_nanos: None,
        integrity_check: false,
        foreign_key_check: false,
        spool_records_validated: 0,
    }
}

fn compute_checksums(
    destination: &Path,
    relative_paths: &[String],
) -> Result<Vec<ArchiveFileChecksum>, Error> {
    relative_paths
        .iter()
        .map(|relative| {
            validate_archive_relative_path(relative)?;
            let bytes = fs::read(destination.join(relative)).map_err(|error| {
                Error::Store(format!("cannot checksum backup file {relative}: {error}"))
            })?;
            Ok(ArchiveFileChecksum {
                path: relative.clone(),
                sha256: sha256_hex(&bytes),
            })
        })
        .collect()
}

fn write_checksum_file(destination: &Path, files: &[ArchiveFileChecksum]) -> Result<(), Error> {
    let mut text = String::new();
    for file in files {
        text.push_str(&format!("{}  {}\n", file.sha256, file.path));
    }
    write_file(&destination.join(ARCHIVE_CHECKSUMS_FILE), text.as_bytes())
}

fn verify_checksums(destination: &Path, manifest: &ArchiveManifest) -> Result<(), Error> {
    let recorded = read_checksum_file(destination)?;
    let expected: BTreeMap<_, _> = manifest
        .files
        .iter()
        .map(|file| (file.path.clone(), file.sha256.clone()))
        .collect();
    if recorded != expected || recorded.len() != manifest.files.len() {
        return Err(verification_error(
            VerificationStage::Checksums,
            "manifest and checksums.sha256 disagree",
        ));
    }
    let pending: Vec<_> = manifest
        .files
        .iter()
        .filter(|file| file.path.starts_with("pending/"))
        .map(|file| file.path.clone())
        .collect();
    if pending != manifest.pending_records
        || !expected.contains_key(ARCHIVE_DATABASE_FILE)
        || expected.len() != manifest.pending_records.len() + 1
    {
        return Err(verification_error(
            VerificationStage::Manifest,
            "manifest file inventory is incomplete or inconsistent",
        ));
    }
    for (relative, expected_hash) in recorded {
        let bytes = fs::read(destination.join(&relative)).map_err(|error| {
            verification_error(
                VerificationStage::Checksums,
                format!("cannot read {relative}: {error}"),
            )
        })?;
        let observed = sha256_hex(&bytes);
        if observed != expected_hash {
            return Err(verification_error(
                VerificationStage::Checksums,
                format!("{relative}: expected {expected_hash}, observed {observed}"),
            ));
        }
    }
    Ok(())
}

fn read_checksum_file(destination: &Path) -> Result<BTreeMap<String, String>, Error> {
    let path = destination.join(ARCHIVE_CHECKSUMS_FILE);
    let text = fs::read_to_string(&path).map_err(|error| {
        verification_error(
            VerificationStage::Checksums,
            format!("cannot read {path:?}: {error}"),
        )
    })?;
    let mut entries = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let (hash, relative) = line.split_once("  ").ok_or_else(|| {
            verification_error(
                VerificationStage::Checksums,
                format!("line {} is malformed", index + 1),
            )
        })?;
        validate_archive_relative_path(relative)?;
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(verification_error(
                VerificationStage::Checksums,
                format!("line {} has an invalid SHA-256", index + 1),
            ));
        }
        if entries
            .insert(relative.to_owned(), hash.to_owned())
            .is_some()
        {
            return Err(verification_error(
                VerificationStage::Checksums,
                format!("duplicate checksum path {relative}"),
            ));
        }
    }
    Ok(entries)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn write_manifest(destination: &Path, manifest: &ArchiveManifest) -> Result<(), Error> {
    write_file(
        &destination.join(ARCHIVE_MANIFEST_FILE),
        manifest.to_json().as_bytes(),
    )
}

fn read_manifest(destination: &Path) -> Result<ArchiveManifest, Error> {
    let path = destination.join(ARCHIVE_MANIFEST_FILE);
    let text = fs::read_to_string(&path)
        .map_err(|error| manifest_error(format!("cannot read {path:?}: {error}")))?;
    ArchiveManifest::from_json(&text)
}

/// The pending records a verified archive's manifest lists, the exact file
/// inventory the verification stage validated. The restore path copies this
/// list rather than trusting a directory listing, so the restore cannot carry
/// a file the archive never verified.
pub(crate) fn archived_pending_records(destination: &Path) -> Result<Vec<String>, Error> {
    Ok(read_manifest(destination)?.pending_records)
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let mut file = crate::store::startup::create_file_mode_0600(path)?;
    file.set_len(0)
        .map_err(|error| Error::Store(format!("cannot truncate {path:?}: {error}")))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| Error::Store(format!("cannot write {path:?}: {error}")))
}

fn validate_pending_file_name(name: &str) -> Result<(), Error> {
    if name.starts_with("attempt-") && name.ends_with(".json") && !name.contains(['/', '\\']) {
        Ok(())
    } else {
        Err(manifest_error(format!(
            "invalid pending record file name {name:?}"
        )))
    }
}

fn validate_pending_archive_path(path: &str) -> Result<(), Error> {
    let Some(name) = path.strip_prefix("pending/") else {
        return Err(manifest_error(format!(
            "pending record path is outside pending/: {path:?}"
        )));
    };
    validate_pending_file_name(name)
}

fn validate_archive_relative_path(path: &str) -> Result<(), Error> {
    let candidate = Path::new(path);
    if candidate.is_absolute()
        || candidate
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(manifest_error(format!("unsafe archive path {path:?}")));
    }
    Ok(())
}

fn required_array<'a>(value: &'a Value, field: &str) -> Result<&'a Vec<Value>, Error> {
    value
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| manifest_error(format!("missing or non-array field {field:?}")))
}

fn required_str(value: &Value, field: &str) -> Result<String, Error> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| manifest_error(format!("missing or non-string field {field:?}")))
}

fn required_u64(value: &Value, field: &str) -> Result<u64, Error> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| manifest_error(format!("missing or non-integer field {field:?}")))
}

fn required_i64(value: &Value, field: &str) -> Result<i64, Error> {
    value
        .get(field)
        .and_then(Value::as_i64)
        .ok_or_else(|| manifest_error(format!("missing or non-integer field {field:?}")))
}

fn required_bool(value: &Value, field: &str) -> Result<bool, Error> {
    value
        .get(field)
        .and_then(Value::as_bool)
        .ok_or_else(|| manifest_error(format!("missing or non-boolean field {field:?}")))
}

fn optional_i64(value: &Value, field: &str) -> Result<Option<i64>, Error> {
    match value.get(field) {
        Some(Value::Null) | None => Ok(None),
        Some(value) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| manifest_error(format!("non-integer field {field:?}"))),
    }
}

fn manifest_error(detail: impl Into<String>) -> Error {
    verification_error(VerificationStage::Manifest, detail)
}

fn verification_error(stage: VerificationStage, detail: impl Into<String>) -> Error {
    Error::Store(format!(
        "backup verification {}: {}",
        stage.as_str(),
        detail.into()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::domain::time::FakeClock;
    use crate::domain::window::QuantizationSemantics;
    use crate::store::connection::PragmaPolicy;
    use crate::store::meter_evidence::{measurement_basis_sql, quantization_sql};
    use crate::store::spool::{PendingTerminalBundle, PendingWindow, spool_pending};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A fresh scratch directory under the system temp dir, removed on drop.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-backup-archive-test-{}-{suffix}",
                std::process::id()
            ));
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

    /// A scratch state directory whose ledger database is migrated to the
    /// current schema but holds no account or attempt rows: enough for a
    /// backup cut to read schema version and ledger generation, not enough
    /// for a spool record referencing them to ever apply.
    fn migrated_state_dir() -> ScratchDir {
        let scratch = ScratchDir::new();
        let db_path = scratch
            .path()
            .join(crate::store::connection::LEDGER_DATABASE_FILE);
        let policy = PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1000),
        };
        let _conn = crate::store::test_schema::open_migrated(&db_path, &policy);
        scratch
    }

    /// A structurally valid pending bundle whose account and attempt do not
    /// exist in [`migrated_state_dir`]'s ledger, so drain quarantines nothing
    /// (the JSON and domain fields are all valid) but fails outright on the
    /// foreign-key constraint the first insert hits.
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
            reset_precision_nanos: None,
            windows: vec![PendingWindow {
                semantic_key: "five_hour".into(),
                scope_kind: "account_wide".into(),
                scoped_model: None,
                quota_used_ppm: 250_000,
                reported_resolution_ppm: 10_000,
                quantization: quantization_sql::as_sql(QuantizationSemantics::Exact).to_owned(),
                resets_at_nanos: Some(5_000),
                reset_grid: None,
                nominal_duration_nanos: 18_000_000_000_000,
                is_active: true,
                severity: "unknown".into(),
            }],
        }
    }

    #[test]
    fn manifest_carries_every_documented_field() {
        let scratch = migrated_state_dir();
        spool_pending(scratch.path(), &undrainable_bundle(1)).unwrap();

        let destination = scratch.path().join("archive");
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(5_000_000_000));
        let summary = create_archive(
            scratch.path(),
            &destination,
            MonotonicDuration::from_millis(1000),
            &clock,
        )
        .unwrap();
        assert!(summary.verified, "a freshly created archive must verify");

        let manifest_text = fs::read_to_string(destination.join(ARCHIVE_MANIFEST_FILE)).unwrap();
        let manifest: Value = serde_json::from_str(&manifest_text).unwrap();
        assert_eq!(manifest["schema_version"], summary.schema_version);
        assert_eq!(manifest["aub_version"], crate::build_info::crate_version());
        assert!(manifest["created_at_unix_nanos"].as_i64().is_some());
        assert_eq!(
            manifest["source_ledger_generation"],
            summary.ledger_generation
        );
        assert_eq!(
            manifest["drain_completed"], false,
            "the drain must have failed outright against the unseeded ledger"
        );
        let files = manifest["files"].as_array().unwrap();
        assert!(
            files
                .iter()
                .any(|file| file["path"] == ARCHIVE_DATABASE_FILE)
        );
        assert!(
            files
                .iter()
                .any(|file| file["path"] == "pending/attempt-1.json")
        );
        let pending_records = manifest["pending_records"].as_array().unwrap();
        assert_eq!(pending_records.len(), 1);
        assert_eq!(pending_records[0], "pending/attempt-1.json");
        let verification = &manifest["verification"];
        assert_eq!(verification["verified"], true);
        assert_eq!(verification["integrity_check"], true);
        assert_eq!(verification["foreign_key_check"], true);
        assert_eq!(verification["spool_records_validated"], 1);
        assert!(verification["checked_at_unix_nanos"].as_i64().is_some());
    }

    #[test]
    fn an_unverified_archive_is_not_counted_as_a_backup_by_age_reporting() {
        let scratch = ScratchDir::new();
        let destination = scratch.path().join("archive");
        crate::store::startup::ensure_dir_mode_0700(&destination).unwrap();
        let manifest = ArchiveManifest {
            schema_version: 1,
            aub_version: "0.0.0-test".into(),
            created_at_unix_nanos: 0,
            source_ledger_generation: 0,
            drain_completed: true,
            files: Vec::new(),
            pending_records: Vec::new(),
            verification: unverified_result(),
        };
        write_manifest(&destination, &manifest).unwrap();

        let health = backup_health(
            &destination,
            UtcTimestamp::from_unix_nanos(1_000_000_000_000),
            MonotonicDuration::from_seconds(3600),
        )
        .unwrap();
        assert!(
            matches!(health, BackupHealth::Unverified { .. }),
            "a manifest with verification.verified = false must never report an age: {health:?}"
        );
    }

    // --- unit: conditional manifest write (aub-2r0n) --------------------------

    /// An identical verification outcome ignoring the clock needs no manifest
    /// write, while any content difference does. The clock always moves, so a
    /// comparison that included `checked_at_unix_nanos` would rewrite on every
    /// run and could never leave a healthy archive byte-identical.
    #[test]
    fn identical_verification_needs_no_write_and_differing_verification_does() {
        let recorded = VerificationResult {
            verified: true,
            checked_at_unix_nanos: Some(1),
            integrity_check: true,
            foreign_key_check: true,
            spool_records_validated: 1,
        };
        let same_content_new_clock = VerificationResult {
            checked_at_unix_nanos: Some(2),
            ..recorded.clone()
        };
        assert!(
            verification_content_equal(&recorded, &same_content_new_clock),
            "same outcome at a new clock must compare equal or every verify rewrites"
        );
        let cleared = unverified_result();
        assert!(
            !verification_content_equal(&recorded, &cleared),
            "verified versus unverified must compare different or a first verify never records"
        );
        let different_spool = VerificationResult {
            spool_records_validated: 0,
            checked_at_unix_nanos: Some(1),
            ..recorded.clone()
        };
        assert!(
            !verification_content_equal(&recorded, &different_spool),
            "a changed spool count must compare different"
        );
    }

    /// Planted negative: the same test with the timestamp included would claim
    /// the identical outcome differs, which is exactly the rewrite-every-run
    /// behaviour this bead removes.
    #[test]
    fn including_the_timestamp_would_claim_an_identical_outcome_differs() {
        let recorded = VerificationResult {
            verified: true,
            checked_at_unix_nanos: Some(1),
            integrity_check: true,
            foreign_key_check: true,
            spool_records_validated: 0,
        };
        let reticked = VerificationResult {
            checked_at_unix_nanos: Some(2),
            ..recorded.clone()
        };
        assert_ne!(
            recorded, reticked,
            "full equality includes the clock, so it cannot be the write gate"
        );
        assert!(
            verification_content_equal(&recorded, &reticked),
            "the content gate must ignore the clock"
        );
    }

    /// End to end through `verify_archive`: a second verification at a later
    /// clock leaves the manifest bytes untouched, while a first verification
    /// of an unverified archive records the change.
    #[test]
    fn reverify_at_a_later_clock_leaves_the_manifest_bytes_untouched() {
        let scratch = migrated_state_dir();
        let destination = scratch.path().join("archive");
        let first_clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1));
        let summary = create_archive(
            scratch.path(),
            &destination,
            MonotonicDuration::from_millis(1000),
            &first_clock,
        )
        .unwrap();
        assert!(summary.verified);
        let before = fs::read(destination.join(ARCHIVE_MANIFEST_FILE)).unwrap();

        let later_clock = FakeClock::new(UtcTimestamp::from_unix_nanos(2_000_000_000));
        let second = verify_archive(
            &destination,
            MonotonicDuration::from_millis(1000),
            &later_clock,
        )
        .unwrap();
        assert!(second.verified);
        let after = fs::read(destination.join(ARCHIVE_MANIFEST_FILE)).unwrap();
        assert_eq!(
            before, after,
            "a re-verification with an identical result must not rewrite the manifest"
        );
        assert!(
            !destination.join("ledger.db-shm").exists()
                && !destination.join("ledger.db-wal").exists(),
            "verification must create no sidecar inside the archive"
        );
    }

    fn backup_series_entry(
        name: &str,
        rfc3339: &str,
        generation: u64,
        verified: bool,
    ) -> BackupSeriesArchiveEntry {
        BackupSeriesArchiveEntry::new(
            name.to_owned(),
            UtcTimestamp::parse_rfc3339(rfc3339).expect("fixture timestamp must parse"),
            generation,
            verified,
        )
    }

    #[test]
    fn backup_series_archive_dir_name_carries_instant_and_generation() {
        let created_at =
            UtcTimestamp::parse_rfc3339("2026-09-09T12:00:00.123456789Z").expect("must parse");
        let name = backup_series_archive_dir_name(created_at, 9648);
        assert!(
            name.contains("2026-09-09") && name.contains("9648"),
            "the directory name must carry both the creation instant and the source generation: {name}"
        );
        assert!(
            name.starts_with(BACKUP_SERIES_ARCHIVE_PREFIX),
            "series archives share one prefix so a root listing is unambiguous: {name}"
        );
    }

    #[test]
    fn backup_series_retention_keeps_an_archive_any_bucket_wants() {
        let archives = vec![
            backup_series_entry("a-newest", "2026-09-09T12:00:00Z", 100, true),
            backup_series_entry("b-same-day", "2026-09-09T06:00:00Z", 99, true),
            backup_series_entry("c-prior-week", "2026-09-01T12:00:00Z", 90, true),
            backup_series_entry("d-prior-month", "2026-08-15T12:00:00Z", 80, true),
            backup_series_entry("e-prior-year", "2025-06-01T12:00:00Z", 70, true),
            backup_series_entry("f-ancient", "2024-01-01T12:00:00Z", 60, true),
        ];
        let retention = BackupSeriesRetention::new(1, 2, 2, 2);
        let retained = backup_series_select_retained(&archives, &retention, Some("a-newest"));
        assert!(
            retained.contains("a-newest"),
            "the newest verified archive is always retained"
        );
        assert!(
            retained.contains("c-prior-week"),
            "c is the latest of its week and nothing else keeps it: {retained:?}"
        );
        assert!(
            retained.contains("d-prior-month"),
            "d is the latest of its month and nothing else keeps it: {retained:?}"
        );
        assert!(
            retained.contains("e-prior-year"),
            "e is the latest of its year and nothing else keeps it: {retained:?}"
        );
        assert!(
            !retained.contains("b-same-day"),
            "b shares its day and week with a newer archive and no other bucket reaches it: {retained:?}"
        );
        assert!(
            !retained.contains("f-ancient"),
            "f is older than every bucket horizon: {retained:?}"
        );
    }

    #[test]
    fn backup_series_retention_always_keeps_the_newest_verified_archive() {
        let archives = vec![
            backup_series_entry("a-newest", "2026-09-09T12:00:00Z", 100, true),
            backup_series_entry("b-older", "2026-09-08T12:00:00Z", 99, true),
        ];
        let retention = BackupSeriesRetention::new(0, 0, 0, 0);
        let retained = backup_series_select_retained(&archives, &retention, Some("a-newest"));
        assert_eq!(
            retained.iter().collect::<Vec<_>>(),
            vec!["a-newest"],
            "with every bucket disabled only the newest verified archive survives: {retained:?}"
        );
    }

    #[test]
    fn backup_series_retention_over_an_empty_set_retains_nothing() {
        let retention = BackupSeriesRetention::new(7, 4, 6, 2);
        let retained = backup_series_select_retained(&[], &retention, None);
        assert!(
            retained.is_empty(),
            "an empty series retains nothing: {retained:?}"
        );
    }

    /// Pruning leaves an unverified archive alone even when no bucket would
    /// keep it: a failed verification must stay diagnosable, and only a later
    /// verified run may advance the pointer past it.
    #[test]
    fn backup_series_prune_leaves_unverified_archives_alone() {
        let scratch = ScratchDir::new();
        let root = scratch.path().join("root");
        crate::store::startup::ensure_dir_mode_0700(&root).unwrap();
        let verified = root.join("aub-backup-2026-09-09T12-00-00.000000000Z-g2");
        let unverified = root.join("aub-backup-2026-09-08T12-00-00.000000000Z-g1");
        for dir in [&verified, &unverified] {
            crate::store::startup::ensure_dir_mode_0700(dir).unwrap();
        }
        let verified_manifest = ArchiveManifest {
            schema_version: 1,
            aub_version: "0.0.0-test".into(),
            created_at_unix_nanos: UtcTimestamp::parse_rfc3339("2026-09-09T12:00:00Z")
                .unwrap()
                .unix_nanos(),
            source_ledger_generation: 2,
            drain_completed: true,
            files: Vec::new(),
            pending_records: Vec::new(),
            verification: VerificationResult {
                verified: true,
                checked_at_unix_nanos: Some(1),
                integrity_check: true,
                foreign_key_check: true,
                spool_records_validated: 0,
            },
        };
        let unverified_manifest = ArchiveManifest {
            created_at_unix_nanos: UtcTimestamp::parse_rfc3339("2026-09-08T12:00:00Z")
                .unwrap()
                .unix_nanos(),
            source_ledger_generation: 1,
            verification: unverified_result(),
            ..verified_manifest.clone()
        };
        write_manifest(&verified, &verified_manifest).unwrap();
        write_manifest(&unverified, &unverified_manifest).unwrap();
        backup_series_write_pointer(&root, "aub-backup-2026-09-09T12-00-00.000000000Z-g2").unwrap();

        let retention = BackupSeriesRetention::new(0, 0, 0, 0);
        let report = backup_series_prune(&root, &retention).unwrap();
        assert!(
            unverified.is_dir(),
            "an unverified archive must never be pruned: {report:?}"
        );
        assert!(
            report.retained.iter().any(|name| name.contains("-g1")),
            "the report must list the unverified archive as retained: {report:?}"
        );
    }

    #[test]
    fn backup_series_prune_refuses_when_no_verified_archive_exists() {
        let scratch = ScratchDir::new();
        let root = scratch.path().join("root");
        crate::store::startup::ensure_dir_mode_0700(&root).unwrap();
        let dir = root.join("aub-backup-2026-09-08T12-00-00.000000000Z-g1");
        crate::store::startup::ensure_dir_mode_0700(&dir).unwrap();
        let manifest = ArchiveManifest {
            schema_version: 1,
            aub_version: "0.0.0-test".into(),
            created_at_unix_nanos: UtcTimestamp::parse_rfc3339("2026-09-08T12:00:00Z")
                .unwrap()
                .unix_nanos(),
            source_ledger_generation: 1,
            drain_completed: true,
            files: Vec::new(),
            pending_records: Vec::new(),
            verification: unverified_result(),
        };
        write_manifest(&dir, &manifest).unwrap();

        let retention = BackupSeriesRetention::new(7, 4, 6, 2);
        let error = backup_series_prune(&root, &retention)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("no verified archive"),
            "pruning with nothing verified must refuse rather than empty the root: {error}"
        );
        assert!(dir.is_dir(), "a refused prune deletes nothing");
    }
}
