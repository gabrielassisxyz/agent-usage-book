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
const ARCHIVE_PENDING_DIR: &str = "pending";
const ARCHIVE_FORMAT_VERSION: u32 = 1;

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
/// The verified bit is cleared before any check and restored only after all
/// checks pass.
pub fn verify_archive(
    destination: &Path,
    busy_timeout: MonotonicDuration,
    clock: &dyn Clock,
) -> Result<BackupSummary, Error> {
    let mut manifest = read_manifest(destination)?;
    manifest.verification = unverified_result();
    write_manifest(destination, &manifest)?;

    verify_checksums(destination, &manifest)?;
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

    manifest.verification = VerificationResult {
        verified: true,
        checked_at_unix_nanos: Some(clock.now().unix_nanos()),
        integrity_check: database_result.integrity_check,
        foreign_key_check: database_result.foreign_key_check,
        spool_records_validated: manifest.pending_records.len(),
    };
    write_manifest(destination, &manifest)?;
    Ok(summary(destination, &manifest))
}

/// Reads the archive's doctor fact. An unverified archive has no age by
/// construction, so it cannot satisfy backup policy merely by being recent.
pub fn backup_health(
    destination: &Path,
    now: UtcTimestamp,
    review_after: MonotonicDuration,
) -> Result<BackupHealth, Error> {
    if !destination.join(ARCHIVE_MANIFEST_FILE).is_file() {
        return Ok(BackupHealth::Missing);
    }
    let manifest = read_manifest(destination)?;
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
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
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
        let mut conn = open(&db_path, AccessMode::ReadWrite, &policy).unwrap();
        crate::store::migrate::run_migrations(
            &mut conn,
            &crate::store::migrations::registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .unwrap();
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
            windows: vec![PendingWindow {
                semantic_key: "five_hour".into(),
                scope_kind: "account_wide".into(),
                scoped_model: None,
                quota_used_ppm: 250_000,
                reported_resolution_ppm: 10_000,
                quantization: quantization_sql::as_sql(QuantizationSemantics::Exact).to_owned(),
                resets_at_nanos: 5_000,
                nominal_duration_nanos: 18_000_000_000_000,
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
}
