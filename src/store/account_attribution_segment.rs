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

use crate::attribution::account_segment::AccountSegmentationResult;
use crate::domain::time::UtcTimestamp;
use crate::domain::tokens::KnownTokenVector;
use crate::error::Error;

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

    for (logical_account, usage) in result.accounts() {
        insert_row(
            &tx,
            session_id,
            "account",
            Some(logical_account),
            *usage,
            computed_at,
        )?;
    }
    if let Some(usage) = result.unknown_account_usage() {
        insert_row(&tx, session_id, "unknown_account", None, usage, computed_at)?;
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
    usage: KnownTokenVector,
    computed_at: UtcTimestamp,
) -> Result<(), Error> {
    conn.execute(
        "INSERT INTO account_attribution_segment (
            session_id, target_kind, logical_account,
            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, computed_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            session_id,
            target_kind,
            logical_account,
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
            markers: vec![AccountMarkerBoundary {
                logical_account: "account-a".into(),
                observed_at: UtcTimestamp::from_unix_nanos(0),
                source_ordering_key: None,
            }],
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

        let rows: Vec<(String, Option<String>, i64)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT target_kind, logical_account, input_tokens FROM account_attribution_segment \
                     WHERE session_id = 'session-1'",
                )
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(
            rows.len(),
            1,
            "the stale account row must be gone after rebuild"
        );
        assert_eq!(rows[0], ("unknown_account".to_owned(), None, 100));
    }
}
