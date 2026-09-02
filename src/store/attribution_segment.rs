//! The `attribution_segment` table: one row per (session, target) the
//! segmentation algorithm assigned usage to (`aub-eu7.2`, PLAN.md 21).
//!
//! Rebuildable, not append-only evidence: [`crate::attribution::segment`] is
//! pure over claim events and usage windows, so re-running it after the
//! tracker data changes is expected to change the attribution. Writing a
//! session's segments therefore replaces its rows wholesale rather than
//! patching them, which is what [`replace_segments_for_session`] does inside
//! one transaction.

use rusqlite::{Connection, params};

use crate::attribution::segment::{OverheadReason, SegmentationResult};
use crate::domain::time::UtcTimestamp;
use crate::domain::tokens::KnownTokenVector;
use crate::error::Error;

fn overhead_reason_sql(reason: OverheadReason) -> &'static str {
    match reason {
        OverheadReason::BeforeFirstClaim => "before_first_claim",
        OverheadReason::AfterReleaseWithNoNextClaim => "after_release_with_no_next_claim",
        OverheadReason::AmbiguousBoundary => "ambiguous_boundary",
        OverheadReason::MissingTimestamp => "missing_timestamp",
        OverheadReason::UnmappedSession => "unmapped_session",
        OverheadReason::TrackerUnavailable => "tracker_unavailable",
        OverheadReason::Contended => "contended",
        OverheadReason::UnclaimedSession => "unclaimed_session",
    }
}

/// Replaces every `attribution_segment` row for `session_id` with the given
/// result, inside one transaction: a reader never observes a partial
/// rebuild, either the old segmentation or the new one.
pub fn replace_segments_for_session(
    conn: &mut Connection,
    session_id: &str,
    result: &SegmentationResult,
    computed_at: UtcTimestamp,
) -> Result<(), Error> {
    let tx = conn.transaction().map_err(|error| {
        Error::Store(format!(
            "cannot open the attribution_segment replace transaction: {error}"
        ))
    })?;

    tx.execute(
        "DELETE FROM attribution_segment WHERE session_id = ?1",
        params![session_id],
    )
    .map_err(|error| {
        Error::Store(format!(
            "cannot clear prior attribution_segment rows for session {session_id:?}: {error}"
        ))
    })?;

    for (task_id, usage) in result.tasks() {
        insert_row(
            &tx,
            session_id,
            "task",
            Some(task_id.source().as_str()),
            Some(task_id.native().as_str()),
            None,
            *usage,
            computed_at,
        )?;
    }
    for (reason, usage) in result.overhead_buckets() {
        insert_row(
            &tx,
            session_id,
            "overhead",
            None,
            None,
            Some(overhead_reason_sql(reason)),
            *usage,
            computed_at,
        )?;
    }

    tx.commit().map_err(|error| {
        Error::Store(format!(
            "cannot commit the attribution_segment replace transaction: {error}"
        ))
    })
}

#[allow(clippy::too_many_arguments)]
fn insert_row(
    conn: &Connection,
    session_id: &str,
    target_kind: &str,
    task_source: Option<&str>,
    task_native: Option<&str>,
    overhead_reason: Option<&str>,
    usage: KnownTokenVector,
    computed_at: UtcTimestamp,
) -> Result<(), Error> {
    conn.execute(
        "INSERT INTO attribution_segment (
            session_id, target_kind, task_source, task_native, overhead_reason,
            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, computed_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            session_id,
            target_kind,
            task_source,
            task_native,
            overhead_reason,
            usage.input().value() as i64,
            usage.output().value() as i64,
            usage.cache_read().value() as i64,
            usage.cache_write().value() as i64,
            computed_at.unix_nanos(),
        ],
    )
    .map_err(|error| Error::Store(format!("cannot insert an attribution_segment row: {error}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attribution::TaskEventKind;
    use crate::attribution::segment::{
        ClaimBoundary, SegmentationContext, SegmentationInputs, UsageWindow, segment,
    };
    use crate::domain::ids::{NativeTaskId, SourceNamespace, TaskId};
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
                "aub-attribution-segment-test-{}-{suffix}",
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
            &scratch.0.join("segment-test.db"),
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

        let task = TaskId::new(SourceNamespace::new("github"), NativeTaskId::new("TASK-1"));
        let first_inputs = SegmentationInputs {
            context: SegmentationContext {
                session_is_mapped: true,
                tracker_available: true,
            },
            boundaries: vec![ClaimBoundary {
                task_id: task.clone(),
                occurred_at: UtcTimestamp::from_unix_nanos(0),
                kind: TaskEventKind::Claim,
            }],
            usage: vec![UsageWindow {
                start: Some(UtcTimestamp::from_unix_nanos(1)),
                end: Some(UtcTimestamp::from_unix_nanos(1)),
                usage: usage(100),
            }],
        };
        let first = segment(&first_inputs);
        replace_segments_for_session(
            &mut conn,
            "session-1",
            &first,
            UtcTimestamp::from_unix_nanos(10),
        )
        .unwrap();

        let count_after_first: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM attribution_segment WHERE session_id = 'session-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count_after_first, 1);

        // Rebuild with a session that never got claimed: the prior task row
        // must be gone, replaced wholesale by the new overhead row.
        let second_inputs = SegmentationInputs {
            context: SegmentationContext {
                session_is_mapped: true,
                tracker_available: true,
            },
            boundaries: vec![],
            usage: vec![UsageWindow {
                start: Some(UtcTimestamp::from_unix_nanos(1)),
                end: Some(UtcTimestamp::from_unix_nanos(1)),
                usage: usage(100),
            }],
        };
        let second = segment(&second_inputs);
        replace_segments_for_session(
            &mut conn,
            "session-1",
            &second,
            UtcTimestamp::from_unix_nanos(20),
        )
        .unwrap();

        let rows: Vec<(String, Option<String>, i64)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT target_kind, overhead_reason, input_tokens FROM attribution_segment \
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
            "the stale task row must be gone after rebuild"
        );
        assert_eq!(
            rows[0],
            (
                "overhead".to_owned(),
                Some("unclaimed_session".to_owned()),
                100
            )
        );
    }
}
