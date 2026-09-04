//! The `account_attribution_segment` table: one row per (session, target)
//! the marker-interval segmentation algorithm assigned usage to
//! (`aub-mgv.1`, PLAN.md 12.6, 19.2, 34.17).
//!
//! Rebuildable, not append-only evidence: [`crate::attribution::account_segment::segment`]
//! is pure over account markers and usage events, so re-running it after the
//! marker set changes is expected to change the attribution. Writing a
//! session's segments therefore replaces its rows wholesale rather than
//! patching them, which is what [`replace_account_segments_for_session`]
//! does inside one transaction.

use rusqlite::{Connection, params};

use crate::attribution::account_segment::{AccountEvidenceClass, AccountSegmentationResult};
use crate::domain::time::UtcTimestamp;
use crate::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens,
};
use crate::error::Error;

/// A persisted account attribution segment row from `account_attribution_segment`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAccountAttributionSegment {
    pub id: i64,
    pub session_id: String,
    pub target_kind: String,
    pub logical_account: Option<String>,
    pub evidence_class: AccountEvidenceClass,
    pub usage: KnownTokenVector,
    pub computed_at: UtcTimestamp,
}

impl StoredAccountAttributionSegment {
    /// True when this segment rests on conservative temporal inference.
    ///
    /// Sufficient for `aub-c0b.7` to reject inferred attribution without
    /// reconstructing provenance.
    pub fn is_inferred(&self) -> bool {
        self.evidence_class.is_inferred()
    }
}

/// Loads all `account_attribution_segment` rows for `session_id`.
pub fn account_segments_for_session(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<StoredAccountAttributionSegment>, Error> {
    let mut stmt = conn
        .prepare(
            "SELECT id, session_id, target_kind, logical_account, evidence_class, \
                    input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, computed_at \
             FROM account_attribution_segment \
             WHERE session_id = ?1 \
             ORDER BY id ASC",
        )
        .map_err(|error| {
            Error::Store(format!(
                "cannot prepare account_attribution_segment query for session {session_id:?}: {error}"
            ))
        })?;

    let rows = stmt
        .query_map(params![session_id], map_segment_row)
        .map_err(|error| {
            Error::Store(format!(
                "cannot query account_attribution_segment rows for session {session_id:?}: {error}"
            ))
        })?;

    let mut segments = Vec::new();
    for row in rows {
        segments.push(row.map_err(|error| {
            Error::Store(format!(
                "cannot read account_attribution_segment row for session {session_id:?}: {error}"
            ))
        })?);
    }
    Ok(segments)
}

/// Loads all `account_attribution_segment` rows across all sessions.
pub fn all_account_segments(
    conn: &Connection,
) -> Result<Vec<StoredAccountAttributionSegment>, Error> {
    let mut stmt = conn
        .prepare(
            "SELECT id, session_id, target_kind, logical_account, evidence_class, \
                    input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, computed_at \
             FROM account_attribution_segment \
             ORDER BY id ASC",
        )
        .map_err(|error| {
            Error::Store(format!(
                "cannot prepare all account_attribution_segment query: {error}"
            ))
        })?;

    let rows = stmt.query_map([], map_segment_row).map_err(|error| {
        Error::Store(format!(
            "cannot query all account_attribution_segment rows: {error}"
        ))
    })?;

    let mut segments = Vec::new();
    for row in rows {
        segments.push(row.map_err(|error| {
            Error::Store(format!(
                "cannot read account_attribution_segment row: {error}"
            ))
        })?);
    }
    Ok(segments)
}

fn map_segment_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredAccountAttributionSegment> {
    let id: i64 = row.get(0)?;
    let session_id: String = row.get(1)?;
    let target_kind: String = row.get(2)?;
    let logical_account: Option<String> = row.get(3)?;
    let evidence_class_str: String = row.get(4)?;
    let input: i64 = row.get(5)?;
    let output: i64 = row.get(6)?;
    let cache_read: i64 = row.get(7)?;
    let cache_write: i64 = row.get(8)?;
    let computed_at_nanos: i64 = row.get(9)?;

    let evidence_class = AccountEvidenceClass::parse(&evidence_class_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown account evidence class: '{evidence_class_str}'"),
            )),
        )
    })?;

    Ok(StoredAccountAttributionSegment {
        id,
        session_id,
        target_kind,
        logical_account,
        evidence_class,
        usage: KnownTokenVector::new(
            InputTokens::new(input as u64),
            OutputTokens::new(output as u64),
            CacheReadTokens::new(cache_read as u64),
            CacheWriteTokens::new(cache_write as u64),
        ),
        computed_at: UtcTimestamp::from_unix_nanos(computed_at_nanos),
    })
}

/// Replaces every `account_attribution_segment` row for `session_id` with
/// the given result, inside one transaction: a reader never observes a
/// partial rebuild, either the old segmentation or the new one.
pub fn replace_account_segments_for_session(
    conn: &mut Connection,
    session_id: &str,
    result: &AccountSegmentationResult,
    computed_at: UtcTimestamp,
) -> Result<(), Error> {
    let tx = conn.transaction().map_err(|error| {
        Error::Store(format!(
            "cannot open the account_attribution_segment replace transaction: {error}"
        ))
    })?;

    tx.execute(
        "DELETE FROM account_attribution_segment WHERE session_id = ?1",
        params![session_id],
    )
    .map_err(|error| {
        Error::Store(format!(
            "cannot clear prior account_attribution_segment rows for session {session_id:?}: {error}"
        ))
    })?;

    for (logical_account, usage, evidence_class) in result.accounts_with_evidence() {
        insert_row(
            &tx,
            session_id,
            "account",
            Some(logical_account),
            evidence_class,
            *usage,
            computed_at,
        )?;
    }
    if let Some(usage) = result.unknown_account_usage() {
        insert_row(
            &tx,
            session_id,
            "unknown_account",
            None,
            result.unknown_account_evidence_class(),
            usage,
            computed_at,
        )?;
    }

    tx.commit().map_err(|error| {
        Error::Store(format!(
            "cannot commit the account_attribution_segment replace transaction: {error}"
        ))
    })
}

fn insert_row(
    conn: &Connection,
    session_id: &str,
    target_kind: &str,
    logical_account: Option<&str>,
    evidence_class: AccountEvidenceClass,
    usage: KnownTokenVector,
    computed_at: UtcTimestamp,
) -> Result<(), Error> {
    conn.execute(
        "INSERT INTO account_attribution_segment (
            session_id, target_kind, logical_account, evidence_class,
            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, computed_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            session_id,
            target_kind,
            logical_account,
            evidence_class.as_str(),
            usage.input().value() as i64,
            usage.output().value() as i64,
            usage.cache_read().value() as i64,
            usage.cache_write().value() as i64,
            computed_at.unix_nanos(),
        ],
    )
    .map_err(|error| {
        Error::Store(format!(
            "cannot insert an account_attribution_segment row: {error}"
        ))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attribution::account_segment::{
        AccountMarkerBoundary, AccountSegmentationInputs, AccountUsageEvent, segment,
    };
    use crate::domain::tokens::{CacheReadTokens, CacheWriteTokens, InputTokens, OutputTokens};
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::store::migrate::run_migrations;
    use crate::store::migrations::registry;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(std::path::PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-account-attribution-segment-test-{}-{suffix}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn migrated_conn(scratch: &ScratchDir) -> Connection {
        let policy = PragmaPolicy {
            busy_timeout: crate::domain::time::MonotonicDuration::from_millis(1000),
        };
        let mut conn = open(
            &scratch.0.join("account-segment-test.db"),
            AccessMode::ReadWrite,
            &policy,
        )
        .unwrap();
        run_migrations(
            &mut conn,
            &registry(),
            None,
            &crate::domain::time::FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .unwrap();
        conn
    }

    fn usage(input: u64) -> KnownTokenVector {
        KnownTokenVector::new(
            InputTokens::new(input),
            OutputTokens::new(0),
            CacheReadTokens::new(0),
            CacheWriteTokens::new(0),
        )
    }

    #[test]
    fn replacing_a_session_s_segments_clears_the_prior_rebuild_first() {
        let scratch = ScratchDir::new();
        let mut conn = migrated_conn(&scratch);

        let first_inputs = AccountSegmentationInputs {
            markers: vec![AccountMarkerBoundary::explicit(
                "account-a",
                UtcTimestamp::from_unix_nanos(0),
                None,
            )],
            usage: vec![AccountUsageEvent {
                occurred_at: UtcTimestamp::from_unix_nanos(1),
                usage: usage(100),
            }],
        };
        let first = segment(&first_inputs);
        replace_account_segments_for_session(
            &mut conn,
            "session-1",
            &first,
            UtcTimestamp::from_unix_nanos(10),
        )
        .unwrap();

        let count_after_first: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM account_attribution_segment WHERE session_id = 'session-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count_after_first, 1);

        // Rebuild with a marker set carrying no marker at all: the prior
        // account row must be gone, replaced wholesale by the new
        // unknown-account row.
        let second_inputs = AccountSegmentationInputs {
            markers: vec![],
            usage: vec![AccountUsageEvent {
                occurred_at: UtcTimestamp::from_unix_nanos(1),
                usage: usage(100),
            }],
        };
        let second = segment(&second_inputs);
        replace_account_segments_for_session(
            &mut conn,
            "session-1",
            &second,
            UtcTimestamp::from_unix_nanos(20),
        )
        .unwrap();

        let rows: Vec<(String, Option<String>, String, i64)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT target_kind, logical_account, evidence_class, input_tokens \
                     FROM account_attribution_segment \
                     WHERE session_id = 'session-1'",
                )
                .unwrap();
            stmt.query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
        };
        assert_eq!(
            rows.len(),
            1,
            "the stale account row must be gone after rebuild"
        );
        assert_eq!(
            rows[0],
            (
                "unknown_account".to_owned(),
                None,
                "unattributed".to_owned(),
                100
            )
        );
    }

    #[test]
    fn persisted_attribution_segments_round_trip_evidence_class_and_expose_provenance() {
        let scratch = ScratchDir::new();
        let mut conn = migrated_conn(&scratch);

        // A session with an explicit marker, usage before the marker (unattributed),
        // and usage after the marker (explicit).
        let inputs = AccountSegmentationInputs {
            markers: vec![AccountMarkerBoundary::explicit(
                "account-primary",
                UtcTimestamp::from_unix_nanos(10),
                None,
            )],
            usage: vec![
                AccountUsageEvent {
                    occurred_at: UtcTimestamp::from_unix_nanos(5),
                    usage: usage(40),
                },
                AccountUsageEvent {
                    occurred_at: UtcTimestamp::from_unix_nanos(15),
                    usage: usage(60),
                },
            ],
        };
        let result = segment(&inputs);
        replace_account_segments_for_session(
            &mut conn,
            "session-round-trip",
            &result,
            UtcTimestamp::from_unix_nanos(100),
        )
        .unwrap();

        let segments = account_segments_for_session(&conn, "session-round-trip").unwrap();
        assert_eq!(segments.len(), 2);

        let account_seg = segments
            .iter()
            .find(|s| s.target_kind == "account")
            .expect("must have account segment");
        assert_eq!(
            account_seg.logical_account.as_deref(),
            Some("account-primary")
        );
        assert_eq!(
            account_seg.evidence_class,
            AccountEvidenceClass::ExplicitLauncherOrHook
        );
        assert!(!account_seg.is_inferred());
        assert_eq!(account_seg.usage.input().value(), 60);

        let unknown_seg = segments
            .iter()
            .find(|s| s.target_kind == "unknown_account")
            .expect("must have unknown account segment");
        assert_eq!(unknown_seg.logical_account, None);
        assert_eq!(
            unknown_seg.evidence_class,
            AccountEvidenceClass::Unattributed
        );
        assert!(!unknown_seg.is_inferred());
        assert_eq!(unknown_seg.usage.input().value(), 40);

        // Also verify an inferred segment exposes is_inferred without reconstructing provenance.
        let inferred_inputs = AccountSegmentationInputs {
            markers: vec![AccountMarkerBoundary::inferred(
                "account-inferred-only",
                UtcTimestamp::from_unix_nanos(0),
                None,
            )],
            usage: vec![AccountUsageEvent {
                occurred_at: UtcTimestamp::from_unix_nanos(1),
                usage: usage(99),
            }],
        };
        let inferred_res = segment(&inferred_inputs);
        replace_account_segments_for_session(
            &mut conn,
            "session-inferred",
            &inferred_res,
            UtcTimestamp::from_unix_nanos(200),
        )
        .unwrap();

        let inferred_segs = account_segments_for_session(&conn, "session-inferred").unwrap();
        assert_eq!(inferred_segs.len(), 1);
        assert_eq!(
            inferred_segs[0].evidence_class,
            AccountEvidenceClass::ConservativeTemporalInference
        );
        assert!(
            inferred_segs[0].is_inferred(),
            "inferred segment must report is_inferred() = true"
        );
    }

    #[test]
    fn session_with_explicit_and_later_inferred_marker_attributes_by_explicit_one() {
        let scratch = ScratchDir::new();
        let mut conn = migrated_conn(&scratch);

        // Explicit marker at t=10, inferred marker arriving later at t=20.
        // Usage events at t=15 and t=25.
        // Under ordinary last-write-wins, the marker at t=20 would overwrite the explicit
        // marker for usage at t=25. Under evidence ranking, the explicit marker takes precedence.
        let inputs = AccountSegmentationInputs {
            markers: vec![
                AccountMarkerBoundary::explicit(
                    "account-explicit",
                    UtcTimestamp::from_unix_nanos(10),
                    None,
                ),
                AccountMarkerBoundary::inferred(
                    "account-inferred",
                    UtcTimestamp::from_unix_nanos(20),
                    None,
                ),
            ],
            usage: vec![
                AccountUsageEvent {
                    occurred_at: UtcTimestamp::from_unix_nanos(15),
                    usage: usage(100),
                },
                AccountUsageEvent {
                    occurred_at: UtcTimestamp::from_unix_nanos(25),
                    usage: usage(150),
                },
            ],
        };
        let result = segment(&inputs);
        replace_account_segments_for_session(
            &mut conn,
            "session-precedence",
            &result,
            UtcTimestamp::from_unix_nanos(50),
        )
        .unwrap();

        let segments = account_segments_for_session(&conn, "session-precedence").unwrap();
        assert_eq!(segments.len(), 1);
        assert_eq!(
            segments[0].logical_account.as_deref(),
            Some("account-explicit")
        );
        assert_eq!(
            segments[0].evidence_class,
            AccountEvidenceClass::ExplicitLauncherOrHook
        );
        assert_eq!(segments[0].usage.input().value(), 250);
        assert!(!segments[0].is_inferred());
    }

    #[test]
    fn segment_table_addressable_by_aub_rebuild_and_absent_from_irreplaceable_classes() {
        use crate::store::retention::{DurableClass, RebuildGroup};

        // Assert against rebuild target enum:
        // 1. AccountAttributionSegment is mapped to RebuildGroup::Attribution
        assert_eq!(
            DurableClass::AccountAttributionSegment.rebuild_group(),
            Some(RebuildGroup::Attribution),
            "account_attribution_segment must be addressable by aub rebuild attribution"
        );

        // 2. RebuildGroup::Attribution sweeps AccountAttributionSegment
        assert!(
            RebuildGroup::Attribution
                .classes()
                .contains(&DurableClass::AccountAttributionSegment),
            "RebuildGroup::Attribution must contain AccountAttributionSegment"
        );

        // 3. AccountAttributionSegment is absent from every irreplaceable class
        assert!(
            !DurableClass::AccountAttributionSegment.is_irreplaceable(),
            "account_attribution_segment is rebuildable materialization, never irreplaceable"
        );
    }
}
