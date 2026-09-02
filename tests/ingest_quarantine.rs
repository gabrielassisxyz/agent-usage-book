//! Integration and property tests for ingest quarantine (`aub-lqe.6`, PLAN.md
//! 12.11).
//!
//! May not depend on:
//! - presentation
//! - provider adapters
//! - HTTP or terminal formatting

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::ingest_quarantine::{
    NewQuarantineItem, clear_all_quarantine, count_quarantined_records, load_all_quarantine,
    quarantine_summary, record_quarantine,
};
use agent_usage_book::transcripts::parser::{
    ParserVersion, QuarantineClass, QuarantineDiagnosticPolicy, QuarantineRecord, SourceLocation,
};
use proptest::prelude::*;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Self {
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aub-quarantine-integration-{}-{suffix}",
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
    let db_path = scratch.path().join("quarantine_integration.db");
    let policy = PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(1000),
    };
    let mut conn = open(&db_path, AccessMode::ReadWrite, &policy).unwrap();
    agent_usage_book::store::migrate::run_migrations(
        &mut conn,
        &agent_usage_book::store::migrations::registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
    )
    .unwrap();
    (scratch, conn)
}

fn ts(nanos: i64) -> UtcTimestamp {
    UtcTimestamp::from_unix_nanos(nanos)
}

#[test]
fn integration_quarantine_counts_tracked_and_doctor_summary() {
    let (_scratch, conn) = fixture_conn();

    let r1 = QuarantineRecord::new(
        SourceLocation::new("transcripts/corrupt_1.jsonl", 12),
        ParserVersion::new("native-v1"),
        QuarantineClass::TruncatedStructure,
    );
    let r2 = QuarantineRecord::new(
        SourceLocation::new("transcripts/corrupt_2.jsonl", 45),
        ParserVersion::new("native-v1"),
        QuarantineClass::MissingRequiredField,
    );

    record_quarantine(&conn, &NewQuarantineItem::from_record(&r1, ts(1000))).unwrap();
    record_quarantine(&conn, &NewQuarantineItem::from_record(&r2, ts(2000))).unwrap();

    let total = count_quarantined_records(&conn).unwrap();
    assert_eq!(total, 2);

    let summary = quarantine_summary(&conn).unwrap();
    assert_eq!(summary.len(), 2);
}

#[test]
fn default_policy_stores_no_transcript_text() {
    let (_scratch, conn) = fixture_conn();
    let default_policy = QuarantineDiagnosticPolicy::default();

    let malformed_raw =
        "{\"user_prompt\": \"super secret personal data that must never be stored\"}";
    let record = QuarantineRecord::with_raw_content(
        SourceLocation::new("sensitive.jsonl", 1),
        ParserVersion::new("native-v1"),
        QuarantineClass::WrongFieldType,
        Some(100),
        malformed_raw,
        &default_policy,
    );

    assert_eq!(record.excerpt(), None);
    record_quarantine(&conn, &NewQuarantineItem::from_record(&record, ts(1000))).unwrap();

    let rows = load_all_quarantine(&conn).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].excerpt(), None);
    assert!(!rows[0].excerpt_hash().is_empty());

    let cleared = clear_all_quarantine(&conn).unwrap();
    assert_eq!(cleared, 1);
    assert_eq!(count_quarantined_records(&conn).unwrap(), 0);
}

proptest! {
    #[test]
    fn prop_default_policy_never_stores_excerpt_text(
        raw in "\\PC{1,200}",
        line in 1u64..10000u64,
        offset in 0u64..1000000u64
    ) {
        let policy = QuarantineDiagnosticPolicy::HashOnly;
        let record = QuarantineRecord::with_raw_content(
            SourceLocation::new("f.jsonl", line),
            ParserVersion::new("v1"),
            QuarantineClass::TruncatedStructure,
            Some(offset),
            &raw,
            &policy,
        );

        prop_assert_eq!(record.excerpt(), None);
        prop_assert!(!record.excerpt_hash().is_empty());
    }
}
