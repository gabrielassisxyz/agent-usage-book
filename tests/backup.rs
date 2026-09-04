//! Integration tests for backup archive creation and verification
//! (`aub-sth.12`, PLAN.md sections 11.5, 27, 36, 38).
//!
//! May not depend on:
//! - presentation
//! - provider adapters

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use agent_usage_book::backup::{
    ARCHIVE_CHECKSUMS_FILE, ARCHIVE_DATABASE_FILE, ARCHIVE_MANIFEST_FILE, create_archive,
    verify_archive,
};
use agent_usage_book::domain::time::{
    FakeClock, MeasurementBasis, MonotonicDuration, UtcTimestamp,
};
use agent_usage_book::domain::window::QuantizationSemantics;
use agent_usage_book::store::account::observe_account;
use agent_usage_book::store::backup::verify_database;
use agent_usage_book::store::connection::{AccessMode, LEDGER_DATABASE_FILE, PragmaPolicy, open};
use agent_usage_book::store::meter_evidence::{measurement_basis_sql, quantization_sql};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use agent_usage_book::store::spool::{PendingTerminalBundle, PendingWindow, spool_pending};
use sha2::{Digest, Sha256};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh scratch directory under the system temp dir, removed on drop.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Self {
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aub-backup-integration-{}-{suffix}",
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

fn busy_timeout() -> MonotonicDuration {
    MonotonicDuration::from_millis(1000)
}

/// A scratch state directory whose ledger database is migrated to the
/// current schema.
fn migrated_state_dir() -> ScratchDir {
    let scratch = ScratchDir::new();
    let db_path = scratch.path().join(LEDGER_DATABASE_FILE);
    let mut conn = open(
        &db_path,
        AccessMode::ReadWrite,
        &PragmaPolicy {
            busy_timeout: busy_timeout(),
        },
    )
    .unwrap();
    run_migrations(
        &mut conn,
        &registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
    )
    .unwrap();
    scratch
}

/// A structurally valid pending bundle whose account and attempt do not
/// exist, so it survives an attempted drain into an otherwise-empty ledger
/// (the drain fails outright on the first foreign-key constraint) and ends
/// up in the archive under `pending/`.
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
        measurement_basis: measurement_basis_sql::as_sql(MeasurementBasis::ProviderObserved)
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
        }],
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Rewrites a file's checksum entries in both `checksums.sha256` and
/// `manifest.json`'s `files` array to match the digest the file actually has
/// right now. Every corruption case below mutates exactly one archive file
/// and then calls this, so the checksum stage never fires for a change the
/// case means a later stage to catch.
fn recompute_checksum_for(destination: &Path, relative: &str) {
    let bytes = std::fs::read(destination.join(relative)).unwrap();
    let digest = sha256_hex(&bytes);

    let checksums_path = destination.join(ARCHIVE_CHECKSUMS_FILE);
    let checksums_text = std::fs::read_to_string(&checksums_path).unwrap();
    let rewritten: String = checksums_text
        .lines()
        .map(|line| {
            let (hash, path) = line.split_once("  ").unwrap();
            if path == relative {
                format!("{digest}  {path}\n")
            } else {
                format!("{hash}  {path}\n")
            }
        })
        .collect();
    std::fs::write(&checksums_path, rewritten).unwrap();

    let manifest_path = destination.join(ARCHIVE_MANIFEST_FILE);
    let manifest_text = std::fs::read_to_string(&manifest_path).unwrap();
    let mut manifest: serde_json::Value = serde_json::from_str(&manifest_text).unwrap();
    for file in manifest["files"].as_array_mut().unwrap() {
        if file["path"] == relative {
            file["sha256"] = serde_json::Value::String(digest.clone());
        }
    }
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).unwrap() + "\n",
    )
    .unwrap();
}

#[test]
fn a_backup_taken_while_a_writer_is_active_verifies_with_both_checks_passing() {
    let scratch = migrated_state_dir();
    let source_database = scratch.path().join(LEDGER_DATABASE_FILE);

    let writer_source = source_database.clone();
    let writer = thread::spawn(move || {
        for i in 0..60i64 {
            let conn = open(
                &writer_source,
                AccessMode::ReadWrite,
                &PragmaPolicy {
                    busy_timeout: busy_timeout(),
                },
            )
            .unwrap();
            observe_account(
                &conn,
                "anthropic",
                &format!("work-{i}"),
                UtcTimestamp::from_unix_nanos(i),
            )
            .unwrap();
        }
    });

    let destination = scratch.path().join("archive");
    let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1_000_000_000));
    let summary = create_archive(scratch.path(), &destination, busy_timeout(), &clock).unwrap();

    writer.join().unwrap();

    assert!(
        summary.verified,
        "the archive must verify despite a concurrent writer"
    );

    // Confirm both database checks independently, on the archived copy, not
    // just through the summary bit create_archive already set.
    let database = destination.join(ARCHIVE_DATABASE_FILE);
    let result = verify_database(&database, busy_timeout())
        .unwrap()
        .expect("both database checks must pass against a backup taken from a live writer");
    assert!(result.integrity_check);
    assert!(result.foreign_key_check);
}

#[test]
fn a_backup_is_not_a_blind_file_copy_of_a_live_wal_database() {
    // A blind `fs::copy` of a WAL-mode database file, taken mid-write, omits
    // whatever the writer has not yet checkpointed from the main file: a
    // real snapshot of "one committed write, mid-flight" must still show
    // that write. The SQLite backup API guarantees this by construction; a
    // naive `std::fs::copy(source, destination)` in its place would not.
    let scratch = migrated_state_dir();
    let source_database = scratch.path().join(LEDGER_DATABASE_FILE);
    let conn = open(
        &source_database,
        AccessMode::ReadWrite,
        &PragmaPolicy {
            busy_timeout: busy_timeout(),
        },
    )
    .unwrap();
    observe_account(
        &conn,
        "anthropic",
        "committed-before-backup",
        UtcTimestamp::from_unix_nanos(1),
    )
    .unwrap();
    drop(conn);

    let destination = scratch.path().join("archive");
    let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(2));
    create_archive(scratch.path(), &destination, busy_timeout(), &clock).unwrap();

    let archived = open(
        &destination.join(ARCHIVE_DATABASE_FILE),
        AccessMode::ReadOnly,
        &PragmaPolicy {
            busy_timeout: busy_timeout(),
        },
    )
    .unwrap();
    let count: i64 = archived
        .query_row(
            "SELECT COUNT(*) FROM account WHERE logical_name = 'committed-before-backup'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 1,
        "a committed write made before the backup was taken must be present in the archive"
    );
}

#[test]
fn a_checksum_mismatch_fails_verification_at_the_checksums_stage() {
    let scratch = migrated_state_dir();
    let destination = scratch.path().join("archive");
    let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1));
    let summary = create_archive(scratch.path(), &destination, busy_timeout(), &clock).unwrap();
    assert!(summary.verified);

    // Mutate the database's bytes without touching checksums.sha256 or the
    // manifest, so the mismatch itself is what verification must catch.
    let database = destination.join(ARCHIVE_DATABASE_FILE);
    let mut bytes = std::fs::read(&database).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    std::fs::write(&database, &bytes).unwrap();

    let error = verify_archive(&destination, busy_timeout(), &clock)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("backup verification checksums"),
        "expected a checksums-stage failure, got: {error}"
    );
}

#[test]
fn a_corrupted_database_fails_verification_at_the_integrity_stage() {
    let scratch = migrated_state_dir();
    // This schema's own definition (15 migrations' worth of tables, indices
    // and triggers, all stored as SQL text in sqlite_master) is large enough
    // that a blind byte flip anywhere in the file's early pages lands inside
    // the schema b-tree itself, which every connection has to parse just to
    // open: the failure would then come from `open` or from `PRAGMA
    // journal_mode`, never reaching `integrity_check`. Seeding real rows and
    // then corrupting exactly the `account` table's own root page (found
    // through `sqlite_master`, never guessed) hits a plain data page instead.
    let conn = open(
        &scratch.path().join(LEDGER_DATABASE_FILE),
        AccessMode::ReadWrite,
        &PragmaPolicy {
            busy_timeout: busy_timeout(),
        },
    )
    .unwrap();
    for i in 0..50i64 {
        observe_account(
            &conn,
            "anthropic",
            &format!("seed-{i}"),
            UtcTimestamp::from_unix_nanos(i),
        )
        .unwrap();
    }
    drop(conn);

    let destination = scratch.path().join("archive");
    let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1));
    let summary = create_archive(scratch.path(), &destination, busy_timeout(), &clock).unwrap();
    assert!(summary.verified);

    let database = destination.join(ARCHIVE_DATABASE_FILE);
    let raw = rusqlite::Connection::open(&database).unwrap();
    let page_size: u64 = raw
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .unwrap();
    let root_page: u64 = raw
        .query_row(
            "SELECT rootpage FROM sqlite_master WHERE type = 'table' AND name = 'account'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(raw);

    let mut bytes = std::fs::read(&database).unwrap();
    let page_start = ((root_page - 1) * page_size) as usize;
    let page_end = page_start + page_size as usize;
    // Flip a handful of bytes near the end of the page: far enough past the
    // 8-byte b-tree header and the cell-pointer array that follows it to
    // land inside a row's own payload rather than the page's structural
    // bookkeeping. Corrupting the structure itself (attempted first) made
    // SQLite raise a hard "disk image is malformed" error from within the
    // integrity_check query itself rather than reporting a row of text, so
    // `PRAGMA integrity_check` never got to return the graceful,
    // non-"ok" result this stage is meant to catch.
    for byte in bytes[page_end - 50..page_end - 46].iter_mut() {
        *byte ^= 0xFF;
    }
    std::fs::write(&database, &bytes).unwrap();
    recompute_checksum_for(&destination, ARCHIVE_DATABASE_FILE);

    let error = verify_archive(&destination, busy_timeout(), &clock)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("backup verification integrity"),
        "expected an integrity-stage failure, got: {error}"
    );
}

#[test]
fn a_foreign_key_violation_fails_verification_at_the_foreign_keys_stage() {
    let scratch = migrated_state_dir();
    let destination = scratch.path().join("archive");
    let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1));
    let summary = create_archive(scratch.path(), &destination, busy_timeout(), &clock).unwrap();
    assert!(summary.verified);

    // Insert a row that violates a foreign key directly, through a raw
    // connection with enforcement off: the row content check has to be
    // independent of enforcement at write time, or a single write path
    // bypassing this project's own connection wrapper would leave a
    // violation invisible to backup verification.
    let database = destination.join(ARCHIVE_DATABASE_FILE);
    let raw = rusqlite::Connection::open(&database).unwrap();
    raw.execute_batch(
        "PRAGMA foreign_keys = OFF;
         INSERT INTO meter_response_evidence
             (id, attempt_id, response_classification, received_at, evidence_capsule,
              capsule_schema_version, sanitizer_version, content_hash, capture_truncated)
         VALUES (1, 999999, 'success', 0, '{}', 'v1', 'v1', 'hash', 0);",
    )
    .unwrap();
    drop(raw);
    recompute_checksum_for(&destination, ARCHIVE_DATABASE_FILE);

    let error = verify_archive(&destination, busy_timeout(), &clock)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("backup verification foreign_keys"),
        "expected a foreign-keys-stage failure, got: {error}"
    );
}

#[test]
fn a_corrupted_pending_record_fails_verification_at_the_spool_records_stage() {
    let scratch = migrated_state_dir();
    spool_pending(scratch.path(), &undrainable_bundle(1)).unwrap();

    let destination = scratch.path().join("archive");
    let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1));
    let summary = create_archive(scratch.path(), &destination, busy_timeout(), &clock).unwrap();
    assert!(summary.verified);

    // Corrupt only the pending record, leaving the database and its
    // checksum untouched, so checksums, integrity and foreign keys all pass
    // and spool-record validation is what fires.
    let pending_relative = "pending/attempt-1.json";
    std::fs::write(destination.join(pending_relative), b"{ not json").unwrap();
    recompute_checksum_for(&destination, pending_relative);

    let error = verify_archive(&destination, busy_timeout(), &clock)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("backup verification spool_records"),
        "expected a spool_records-stage failure, got: {error}"
    );
}
