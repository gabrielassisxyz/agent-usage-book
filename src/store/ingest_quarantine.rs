//! The `ingest_quarantine` table: captures source records that could not be
//! normalized (`aub-lqe.6`, PLAN.md 12.11, 29, 36, 37), and the heuristic dedup
//! collision pairs the identity framework quarantined (`aub-lqe.10`, PLAN.md
//! 12.10, 18). Both are material that never became a canonical usage event, and
//! a count of either that stays invisible reads as a smaller, correct ledger.
//!
//! Stored by excerpt hash rather than excerpt text by default (aub-2r3 retention
//! policy), with opt-in bounded redacted excerpt only under explicit diagnostic
//! policy. Recurring parse failures update the last-observed timestamp rather
//! than creating duplicate rows.
//!
//! May not depend on:
//! - presentation
//! - provider adapters

use rusqlite::{Connection, Row, params};

use crate::domain::time::UtcTimestamp;
use crate::error::Error;
use crate::transcripts::parser::QuarantineRecord;

/// The failure class recorded for a heuristic dedup collision (aub-lqe.10,
/// PLAN.md 12.10, 18): two occurrences that shared one heuristic key but
/// normalize to materially different canonical payloads. Recorded here rather
/// than in the parser's `QuarantineClass` because a collision is an identity
/// failure the dedup framework detects, not a record a parser failed to read.
pub const DEDUP_COLLISION_FAILURE_CLASS: &str = "dedup_collision";

/// One stored quarantine record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineRow {
    id: i64,
    source_file: String,
    byte_offset: Option<u64>,
    line_number: Option<u64>,
    parser: String,
    failure_class: String,
    excerpt_hash: String,
    excerpt: Option<String>,
    first_observed: UtcTimestamp,
    last_observed: UtcTimestamp,
}

impl QuarantineRow {
    pub fn id(&self) -> i64 {
        self.id
    }

    pub fn source_file(&self) -> &str {
        &self.source_file
    }

    pub fn byte_offset(&self) -> Option<u64> {
        self.byte_offset
    }

    pub fn line_number(&self) -> Option<u64> {
        self.line_number
    }

    pub fn parser(&self) -> &str {
        &self.parser
    }

    pub fn failure_class(&self) -> &str {
        &self.failure_class
    }

    pub fn excerpt_hash(&self) -> &str {
        &self.excerpt_hash
    }

    pub fn excerpt(&self) -> Option<&str> {
        self.excerpt.as_deref()
    }

    pub fn first_observed(&self) -> UtcTimestamp {
        self.first_observed
    }

    pub fn last_observed(&self) -> UtcTimestamp {
        self.last_observed
    }
}

/// A new quarantine item to be recorded or merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewQuarantineItem {
    pub source_file: String,
    pub byte_offset: Option<u64>,
    pub line_number: Option<u64>,
    pub parser: String,
    pub failure_class: String,
    pub excerpt_hash: String,
    pub excerpt: Option<String>,
    pub observed_at: UtcTimestamp,
}

impl NewQuarantineItem {
    pub fn from_record(record: &QuarantineRecord, observed_at: UtcTimestamp) -> Self {
        Self {
            source_file: record.location().file().to_string(),
            byte_offset: record.byte_offset(),
            line_number: Some(record.location().line()),
            parser: record.parser_version().as_str().to_string(),
            failure_class: record.class().name().to_string(),
            excerpt_hash: record.excerpt_hash().to_string(),
            excerpt: record.excerpt().map(str::to_string),
            observed_at,
        }
    }
}

/// The outcome of recording a quarantine item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuarantineRecordOutcome {
    Inserted,
    UpdatedExisting,
}

/// Aggregated quarantine summary group for `doctor` and health diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineSummaryGroup {
    pub parser: String,
    pub failure_class: String,
    pub count: u64,
    pub first_observed: UtcTimestamp,
    pub last_observed: UtcTimestamp,
}

fn row_to_quarantine(row: &Row<'_>) -> rusqlite::Result<QuarantineRow> {
    let id: i64 = row.get(0)?;
    let source_file: String = row.get(1)?;
    let byte_offset: Option<i64> = row.get(2)?;
    let line_number: Option<i64> = row.get(3)?;
    let parser: String = row.get(4)?;
    let failure_class: String = row.get(5)?;
    let excerpt_hash: String = row.get(6)?;
    let excerpt: Option<String> = row.get(7)?;
    let first_observed_nanos: i64 = row.get(8)?;
    let last_observed_nanos: i64 = row.get(9)?;

    Ok(QuarantineRow {
        id,
        source_file,
        byte_offset: byte_offset.map(|b| b as u64),
        line_number: line_number.map(|l| l as u64),
        parser,
        failure_class,
        excerpt_hash,
        excerpt,
        first_observed: UtcTimestamp::from_unix_nanos(first_observed_nanos),
        last_observed: UtcTimestamp::from_unix_nanos(last_observed_nanos),
    })
}

/// Records or merges a quarantine item into the store.
///
/// If a failure with identical (source_file, parser, failure_class, excerpt_hash)
/// already exists, its `last_observed` time is updated and no duplicate row is
/// inserted.
pub fn record_quarantine(
    conn: &Connection,
    item: &NewQuarantineItem,
) -> Result<QuarantineRecordOutcome, Error> {
    let observed_nanos = item.observed_at.unix_nanos();
    let updated = conn
        .execute(
            "UPDATE ingest_quarantine
             SET last_observed = MAX(last_observed, ?1),
                 excerpt = COALESCE(excerpt, ?2),
                 byte_offset = COALESCE(byte_offset, ?3),
                 line_number = COALESCE(line_number, ?4)
             WHERE source_file = ?5
               AND parser = ?6
               AND failure_class = ?7
               AND excerpt_hash = ?8",
            params![
                observed_nanos,
                item.excerpt.as_deref(),
                item.byte_offset.map(|b| b as i64),
                item.line_number.map(|l| l as i64),
                item.source_file.as_str(),
                item.parser.as_str(),
                item.failure_class.as_str(),
                item.excerpt_hash.as_str(),
            ],
        )
        .map_err(|e| Error::Store(format!("cannot update recurring quarantine item: {e}")))?;

    if updated > 0 {
        return Ok(QuarantineRecordOutcome::UpdatedExisting);
    }

    conn.execute(
        "INSERT INTO ingest_quarantine (
            source_file, byte_offset, line_number, parser, failure_class,
            excerpt_hash, excerpt, first_observed, last_observed
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            item.source_file.as_str(),
            item.byte_offset.map(|b| b as i64),
            item.line_number.map(|l| l as i64),
            item.parser.as_str(),
            item.failure_class.as_str(),
            item.excerpt_hash.as_str(),
            item.excerpt.as_deref(),
            observed_nanos,
            observed_nanos,
        ],
    )
    .map_err(|e| Error::Store(format!("cannot insert quarantine record: {e}")))?;

    Ok(QuarantineRecordOutcome::Inserted)
}

/// Loads all quarantined items in identity order.
pub fn load_all_quarantine(conn: &Connection) -> Result<Vec<QuarantineRow>, Error> {
    let mut stmt = conn
        .prepare(
            "SELECT id, source_file, byte_offset, line_number, parser, failure_class,
                    excerpt_hash, excerpt, first_observed, last_observed
             FROM ingest_quarantine
             ORDER BY id ASC",
        )
        .map_err(|e| Error::Store(format!("cannot prepare load_all_quarantine: {e}")))?;

    let rows = stmt
        .query_map([], row_to_quarantine)
        .map_err(|e| Error::Store(format!("cannot query load_all_quarantine: {e}")))?;

    let mut result = Vec::new();
    for r in rows {
        result.push(r.map_err(|e| Error::Store(format!("cannot read quarantine row: {e}")))?);
    }
    Ok(result)
}

/// Returns the total count of quarantined failure records.
pub fn count_quarantined_records(conn: &Connection) -> Result<u64, Error> {
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM ingest_quarantine", [], |row| {
            row.get(0)
        })
        .map_err(|e| Error::Store(format!("cannot count quarantine records: {e}")))?;
    Ok(count as u64)
}

/// Computes the doctor summary of quarantine records grouped by parser and failure class.
pub fn quarantine_summary(conn: &Connection) -> Result<Vec<QuarantineSummaryGroup>, Error> {
    let mut stmt = conn
        .prepare(
            "SELECT parser, failure_class, COUNT(*), MIN(first_observed), MAX(last_observed)
             FROM ingest_quarantine
             GROUP BY parser, failure_class
             ORDER BY parser ASC, failure_class ASC",
        )
        .map_err(|e| Error::Store(format!("cannot prepare quarantine_summary: {e}")))?;

    let rows = stmt
        .query_map([], |row| {
            let parser: String = row.get(0)?;
            let failure_class: String = row.get(1)?;
            let count: i64 = row.get(2)?;
            let first_nanos: i64 = row.get(3)?;
            let last_nanos: i64 = row.get(4)?;
            Ok(QuarantineSummaryGroup {
                parser,
                failure_class,
                count: count as u64,
                first_observed: UtcTimestamp::from_unix_nanos(first_nanos),
                last_observed: UtcTimestamp::from_unix_nanos(last_nanos),
            })
        })
        .map_err(|e| Error::Store(format!("cannot query quarantine_summary: {e}")))?;

    let mut result = Vec::new();
    for r in rows {
        result.push(r.map_err(|e| Error::Store(format!("cannot read summary group: {e}")))?);
    }
    Ok(result)
}

/// One dedup collision pair, in the plain shapes the quarantine stores. The
/// dedup layer owns detection and stays store-free; the ingest path translates
/// the typed `crate::dedup::HeuristicKeyCollision` into this descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedupCollisionDescriptor {
    /// The parser whose heuristic key both occurrences shared.
    pub parser: String,
    /// The shared key, as stored in `usage_occurrence.heuristic_key`.
    pub heuristic_key: String,
    /// The file the first occurrence was seen in. A pair spanning two files
    /// names the first-seen one: the collision's identity is the key and the
    /// two payload digests, which the excerpt hash carries.
    pub first_file: String,
    /// The canonical payload digest of the first occurrence.
    pub first_payload_digest: String,
    /// The canonical payload digest of the second occurrence.
    pub second_payload_digest: String,
    /// When the collision was recorded.
    pub observed_at: UtcTimestamp,
}

/// Records a dedup collision pair as one quarantine row whose failure class is
/// [`DEDUP_COLLISION_FAILURE_CLASS`]. The excerpt hash hashes the collision's
/// identity (parser, key, both payload digests) rather than any raw excerpt:
/// there is no excerpt, and the hash is what recognises the same collision
/// recurring, which merges into the existing row like any other quarantine
/// item. Neither occurrence produces a usage occurrence row; the pair is
/// quarantined, never merged and never selected.
pub fn record_dedup_collision(
    conn: &Connection,
    collision: &DedupCollisionDescriptor,
) -> Result<QuarantineRecordOutcome, Error> {
    use sha2::{Digest, Sha256};
    let identity = format!(
        "{}|{}|{}|{}",
        collision.parser,
        collision.heuristic_key,
        collision.first_payload_digest,
        collision.second_payload_digest,
    );
    let item = NewQuarantineItem {
        source_file: collision.first_file.clone(),
        byte_offset: None,
        line_number: None,
        parser: collision.parser.clone(),
        failure_class: DEDUP_COLLISION_FAILURE_CLASS.to_string(),
        excerpt_hash: format!("{:x}", Sha256::digest(identity.as_bytes())),
        excerpt: None,
        observed_at: collision.observed_at,
    };
    record_quarantine(conn, &item)
}

/// Clears all quarantine records on whole-index rebuild.
pub fn clear_all_quarantine(conn: &Connection) -> Result<usize, Error> {
    conn.execute("DELETE FROM ingest_quarantine", [])
        .map_err(|e| Error::Store(format!("cannot clear ingest_quarantine table: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::{FakeClock, MonotonicDuration};
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::transcripts::parser::{
        ParserVersion, QuarantineClass, QuarantineDiagnosticPolicy, SourceLocation,
    };
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-quarantine-test-{}-{suffix}",
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

    fn fixture_conn() -> (ScratchDir, Connection) {
        let scratch = ScratchDir::new();
        let db_path = scratch.path().join("quarantine.db");
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
        (scratch, conn)
    }

    fn ts(nanos: i64) -> UtcTimestamp {
        UtcTimestamp::from_unix_nanos(nanos)
    }

    #[test]
    fn hash_only_storage_by_default_and_opt_in_excerpt() {
        let (_scratch, conn) = fixture_conn();
        let default_policy = QuarantineDiagnosticPolicy::HashOnly;
        let opt_in_policy = QuarantineDiagnosticPolicy::BoundedRedactedExcerpt { max_bytes: 30 };

        let record_default = QuarantineRecord::with_raw_content(
            SourceLocation::new("session-1.jsonl", 10),
            ParserVersion::new("native-v1"),
            QuarantineClass::WrongFieldType,
            Some(1024),
            "{\"usage\": \"corrupted_string_here_not_number\"}",
            &default_policy,
        );

        let record_opt_in = QuarantineRecord::with_raw_content(
            SourceLocation::new("session-2.jsonl", 20),
            ParserVersion::new("native-v1"),
            QuarantineClass::MissingRequiredField,
            Some(2048),
            "{\"partial\": true, \"secret_field\": \"secret_token\"}",
            &opt_in_policy,
        );

        let item1 = NewQuarantineItem::from_record(&record_default, ts(10_000));
        let item2 = NewQuarantineItem::from_record(&record_opt_in, ts(20_000));

        assert_eq!(record_default.excerpt(), None);
        assert!(record_opt_in.excerpt().is_some());

        record_quarantine(&conn, &item1).unwrap();
        record_quarantine(&conn, &item2).unwrap();

        let rows = load_all_quarantine(&conn).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].excerpt(),
            None,
            "default policy must have no excerpt text"
        );
        assert!(
            rows[1].excerpt().is_some(),
            "opt-in policy must store bounded excerpt"
        );
        assert!(rows[1].excerpt().unwrap().len() <= 30);
    }

    #[test]
    fn recurring_failure_updates_last_observed_not_duplicate_row() {
        let (_scratch, conn) = fixture_conn();
        let record = QuarantineRecord::new(
            SourceLocation::new("transcript-a.jsonl", 42),
            ParserVersion::new("native-v1"),
            QuarantineClass::TruncatedStructure,
        );

        let item_first = NewQuarantineItem::from_record(&record, ts(100));
        let outcome1 = record_quarantine(&conn, &item_first).unwrap();
        assert_eq!(outcome1, QuarantineRecordOutcome::Inserted);

        let count = count_quarantined_records(&conn).unwrap();
        assert_eq!(count, 1);

        // Same failure recurring at later timestamp:
        let item_recur = NewQuarantineItem::from_record(&record, ts(500));
        let outcome2 = record_quarantine(&conn, &item_recur).unwrap();
        assert_eq!(outcome2, QuarantineRecordOutcome::UpdatedExisting);

        let rows = load_all_quarantine(&conn).unwrap();
        assert_eq!(rows.len(), 1, "must not insert duplicate row");
        assert_eq!(rows[0].first_observed(), ts(100));
        assert_eq!(rows[0].last_observed(), ts(500));
    }

    #[test]
    fn doctor_reporting_quarantine_counts_grouped_by_parser_and_failure_class() {
        let (_scratch, conn) = fixture_conn();

        let r1 = QuarantineRecord::new(
            SourceLocation::new("f1.jsonl", 1),
            ParserVersion::new("parser-a"),
            QuarantineClass::MissingRequiredField,
        );
        let r2 = QuarantineRecord::new(
            SourceLocation::new("f2.jsonl", 2),
            ParserVersion::new("parser-a"),
            QuarantineClass::MissingRequiredField,
        );
        let r3 = QuarantineRecord::new(
            SourceLocation::new("f3.jsonl", 3),
            ParserVersion::new("parser-a"),
            QuarantineClass::WrongFieldType,
        );
        let r4 = QuarantineRecord::new(
            SourceLocation::new("f4.jsonl", 4),
            ParserVersion::new("parser-b"),
            QuarantineClass::TruncatedStructure,
        );

        record_quarantine(&conn, &NewQuarantineItem::from_record(&r1, ts(100))).unwrap();
        record_quarantine(&conn, &NewQuarantineItem::from_record(&r2, ts(200))).unwrap();
        record_quarantine(&conn, &NewQuarantineItem::from_record(&r3, ts(300))).unwrap();
        record_quarantine(&conn, &NewQuarantineItem::from_record(&r4, ts(400))).unwrap();

        let summary = quarantine_summary(&conn).unwrap();
        assert_eq!(summary.len(), 3);

        assert_eq!(summary[0].parser, "parser-a");
        assert_eq!(summary[0].failure_class, "missing_required_field");
        assert_eq!(summary[0].count, 2);

        assert_eq!(summary[1].parser, "parser-a");
        assert_eq!(summary[1].failure_class, "wrong_field_type");
        assert_eq!(summary[1].count, 1);

        assert_eq!(summary[2].parser, "parser-b");
        assert_eq!(summary[2].failure_class, "truncated_structure");
        assert_eq!(summary[2].count, 1);
    }

    #[test]
    fn clear_all_quarantine_cleans_table() {
        let (_scratch, conn) = fixture_conn();
        let r = QuarantineRecord::new(
            SourceLocation::new("test.jsonl", 1),
            ParserVersion::new("parser-a"),
            QuarantineClass::MissingRequiredField,
        );
        record_quarantine(&conn, &NewQuarantineItem::from_record(&r, ts(100))).unwrap();
        assert_eq!(count_quarantined_records(&conn).unwrap(), 1);

        clear_all_quarantine(&conn).unwrap();
        assert_eq!(count_quarantined_records(&conn).unwrap(), 0);
    }

    fn collision(
        parser: &str,
        key: &str,
        first_digest: &str,
        second_digest: &str,
    ) -> DedupCollisionDescriptor {
        DedupCollisionDescriptor {
            parser: parser.to_string(),
            heuristic_key: key.to_string(),
            first_file: "first.jsonl".to_string(),
            first_payload_digest: first_digest.to_string(),
            second_payload_digest: second_digest.to_string(),
            observed_at: ts(1_000),
        }
    }

    /// A dedup collision pair records as one quarantine row whose failure class
    /// is the collision class, not one of the parser's parse-failure classes:
    /// the pair never failed to parse, it failed to agree.
    #[test]
    fn a_dedup_collision_records_one_row_with_the_collision_class() {
        let (_scratch, conn) = fixture_conn();
        let outcome = record_dedup_collision(
            &conn,
            &collision("claude-code-1", "t:1000|s1|10:0:0", "aa", "bb"),
        )
        .unwrap();
        assert_eq!(outcome, QuarantineRecordOutcome::Inserted);

        let rows = load_all_quarantine(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].failure_class(), DEDUP_COLLISION_FAILURE_CLASS);
        assert_eq!(rows[0].parser(), "claude-code-1");
        assert_eq!(rows[0].source_file(), "first.jsonl");
        assert_eq!(rows[0].excerpt(), None);
    }

    /// The planted negative: a parse-failure quarantine and a collision
    /// quarantine are different rows with different classes, never merged into
    /// one count, because the doctor distinguishes what failed to parse from
    /// what collided after parsing.
    #[test]
    fn a_parse_failure_and_a_collision_are_distinct_quarantine_rows() {
        let (_scratch, conn) = fixture_conn();
        let parse_failure = QuarantineRecord::new(
            SourceLocation::new("broken.jsonl", 7),
            ParserVersion::new("claude-code-1"),
            QuarantineClass::WrongFieldType,
        );
        record_quarantine(
            &conn,
            &NewQuarantineItem::from_record(&parse_failure, ts(100)),
        )
        .unwrap();
        record_dedup_collision(
            &conn,
            &collision("claude-code-1", "t:1000|s1|10:0:0", "aa", "bb"),
        )
        .unwrap();

        let rows = load_all_quarantine(&conn).unwrap();
        assert_eq!(rows.len(), 2);
        let classes: std::collections::BTreeSet<&str> =
            rows.iter().map(|row| row.failure_class()).collect();
        assert_eq!(classes.len(), 2, "the two classes must stay distinct");
    }

    /// The same collision recurring (the same key and payload digests, re-ingest)
    /// merges into the existing row rather than duplicating it.
    #[test]
    fn the_same_collision_recurring_merges_instead_of_duplicating() {
        let (_scratch, conn) = fixture_conn();
        let first = collision("p1", "t:1000|s1|10:0:0", "aa", "bb");
        let mut recurring = collision("p1", "t:1000|s1|10:0:0", "aa", "bb");
        recurring.observed_at = ts(9_000);
        record_dedup_collision(&conn, &first).unwrap();
        assert_eq!(
            record_dedup_collision(&conn, &recurring).unwrap(),
            QuarantineRecordOutcome::UpdatedExisting
        );

        let rows = load_all_quarantine(&conn).unwrap();
        assert_eq!(rows.len(), 1, "a recurring collision must not duplicate");
        assert_eq!(rows[0].first_observed(), ts(1_000));
        assert_eq!(rows[0].last_observed(), ts(9_000));
    }

    /// A different collision under the same parser (different key) is its own
    /// row: the hash carries the collision's identity, so the doctor's per-key
    /// evidence stays separable.
    #[test]
    fn a_different_collision_is_its_own_row() {
        let (_scratch, conn) = fixture_conn();
        record_dedup_collision(&conn, &collision("p1", "t:1000|s1|10:0:0", "aa", "bb")).unwrap();
        record_dedup_collision(&conn, &collision("p1", "t:2000|s1|10:0:0", "aa", "bb")).unwrap();
        assert_eq!(count_quarantined_records(&conn).unwrap(), 2);
    }
}
