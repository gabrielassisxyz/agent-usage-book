//! The `sampling_policy_snapshot` table: the immutable, non-secret resolved
//! sampling policy in force for one account at one instant (PLAN.md 12.2).
//!
//! This table exists because `coverage` is otherwise a stronger claim than its
//! inputs can support. If cadence changes from five minutes to fifteen next
//! month, last month's coverage denominator is not today's configuration; if a
//! `Retry-After` postponed the next attempt, that interval was not a missed
//! opportunity. A configuration fingerprint (`sample_run`) identifies that the
//! policy differed; it does not say what it was, which is why the resolved
//! policy is persisted here in full.

use rusqlite::{OptionalExtension, params};

use crate::domain::time::{MonotonicDuration, UtcTimestamp};
use crate::error::Error;
use crate::store::account::AccountId;

/// A `sampling_policy_snapshot` row's identity: its SQLite rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SamplingPolicySnapshotId(i64);

impl SamplingPolicySnapshotId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

/// The resolved sampling policy itself, independent of which account or
/// instant it applies to. Two snapshots carrying an equal policy are the same
/// policy in force twice, which is exactly the comparison
/// [`resolve_policy_snapshot`] uses to decide whether to reuse the most recent
/// snapshot or record a new one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSamplingPolicy {
    pub ordinary_cadence: MonotonicDuration,
    pub freshness_horizon: MonotonicDuration,
    pub reset_edge_policy: String,
    pub retry_backoff_policy: String,
    pub command_budget: MonotonicDuration,
    pub policy_algorithm_version: String,
}

/// One immutable, effective-dated resolved policy for one account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamplingPolicySnapshot {
    id: SamplingPolicySnapshotId,
    account_id: AccountId,
    effective_at: UtcTimestamp,
    policy: ResolvedSamplingPolicy,
}

impl SamplingPolicySnapshot {
    pub fn id(&self) -> SamplingPolicySnapshotId {
        self.id
    }

    pub fn account_id(&self) -> AccountId {
        self.account_id
    }

    pub fn effective_at(&self) -> UtcTimestamp {
        self.effective_at
    }

    pub fn policy(&self) -> &ResolvedSamplingPolicy {
        &self.policy
    }
}

fn row_to_snapshot(row: &rusqlite::Row<'_>) -> rusqlite::Result<SamplingPolicySnapshot> {
    Ok(SamplingPolicySnapshot {
        id: SamplingPolicySnapshotId::new(row.get(0)?),
        account_id: AccountId::new(row.get(1)?),
        effective_at: UtcTimestamp::from_unix_nanos(row.get(2)?),
        policy: ResolvedSamplingPolicy {
            ordinary_cadence: MonotonicDuration::from_nanos(row.get(3)?),
            freshness_horizon: MonotonicDuration::from_nanos(row.get(4)?),
            reset_edge_policy: row.get(5)?,
            retry_backoff_policy: row.get(6)?,
            command_budget: MonotonicDuration::from_nanos(row.get(7)?),
            policy_algorithm_version: row.get(8)?,
        },
    })
}

const SELECT_COLUMNS: &str = "id, account_id, effective_at, ordinary_cadence_nanos, \
     freshness_horizon_nanos, reset_edge_policy, retry_backoff_policy, command_budget_nanos, \
     policy_algorithm_version";

/// The most recently effective snapshot for `account_id`, regardless of
/// instant, or `None` if the account has never had a policy resolved for it.
fn most_recent_snapshot(
    conn: &rusqlite::Connection,
    account_id: AccountId,
) -> Result<Option<SamplingPolicySnapshot>, Error> {
    conn.query_row(
        &format!(
            "SELECT {SELECT_COLUMNS} FROM sampling_policy_snapshot \
             WHERE account_id = ?1 ORDER BY effective_at DESC LIMIT 1"
        ),
        params![account_id.value()],
        row_to_snapshot,
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot read the most recent policy snapshot: {e}")))
}

fn insert_snapshot(
    conn: &rusqlite::Connection,
    account_id: AccountId,
    effective_at: UtcTimestamp,
    policy: &ResolvedSamplingPolicy,
) -> Result<SamplingPolicySnapshotId, Error> {
    conn.query_row(
        "INSERT INTO sampling_policy_snapshot
             (account_id, effective_at, ordinary_cadence_nanos, freshness_horizon_nanos,
              reset_edge_policy, retry_backoff_policy, command_budget_nanos,
              policy_algorithm_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         RETURNING id",
        params![
            account_id.value(),
            effective_at.unix_nanos(),
            policy.ordinary_cadence.as_nanos(),
            policy.freshness_horizon.as_nanos(),
            policy.reset_edge_policy,
            policy.retry_backoff_policy,
            policy.command_budget.as_nanos(),
            policy.policy_algorithm_version,
        ],
        |row| row.get(0),
    )
    .map(SamplingPolicySnapshotId::new)
    .map_err(|e| Error::Store(format!("cannot insert policy snapshot: {e}")))
}

/// Records the policy in force for `account_id` as of `effective_at`: reuses
/// the most recent snapshot for that account when the resolved policy is
/// unchanged, and writes a new immutable snapshot otherwise (PLAN.md 12.2, "a
/// policy snapshot is written whenever the resolved policy differs ... and
/// reused otherwise").
pub fn resolve_policy_snapshot(
    conn: &rusqlite::Connection,
    account_id: AccountId,
    effective_at: UtcTimestamp,
    policy: &ResolvedSamplingPolicy,
) -> Result<SamplingPolicySnapshotId, Error> {
    if let Some(most_recent) = most_recent_snapshot(conn, account_id)?
        && &most_recent.policy == policy
    {
        return Ok(most_recent.id);
    }
    insert_snapshot(conn, account_id, effective_at, policy)
}

/// The policy in force for `account_id` at instant `at`: the snapshot with the
/// latest `effective_at` at or before `at`. `None` means the policy is not
/// known for that instant, which callers report as "policy unknown" rather
/// than substituting the currently configured policy for a past interval.
pub fn effective_policy_at(
    conn: &rusqlite::Connection,
    account_id: AccountId,
    at: UtcTimestamp,
) -> Result<Option<SamplingPolicySnapshot>, Error> {
    conn.query_row(
        &format!(
            "SELECT {SELECT_COLUMNS} FROM sampling_policy_snapshot \
             WHERE account_id = ?1 AND effective_at <= ?2 ORDER BY effective_at DESC LIMIT 1"
        ),
        params![account_id.value(), at.unix_nanos()],
        row_to_snapshot,
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot read the effective policy snapshot: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::account::observe_account;
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-store-policy-snapshot-test-{}-{suffix}",
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
        let db_path = scratch.path().join("meter.db");
        let policy = PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1000),
        };
        let mut conn = open(&db_path, AccessMode::ReadWrite, &policy).unwrap();
        crate::store::migrate::run_migrations(
            &mut conn,
            &crate::store::migrations::registry(),
            None,
            &crate::domain::time::FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .unwrap();
        (scratch, conn)
    }

    fn policy_5m() -> ResolvedSamplingPolicy {
        ResolvedSamplingPolicy {
            ordinary_cadence: MonotonicDuration::from_seconds(300),
            freshness_horizon: MonotonicDuration::from_seconds(720),
            reset_edge_policy: "lead-120s".into(),
            retry_backoff_policy: "exponential-3".into(),
            command_budget: MonotonicDuration::from_seconds(8),
            policy_algorithm_version: "v1".into(),
        }
    }

    fn policy_15m() -> ResolvedSamplingPolicy {
        ResolvedSamplingPolicy {
            ordinary_cadence: MonotonicDuration::from_seconds(900),
            ..policy_5m()
        }
    }

    /// An unchanged resolved policy reuses the most recent snapshot; a changed
    /// one writes a new immutable row.
    #[test]
    fn an_unchanged_policy_is_reused_and_a_changed_one_creates_a_new_snapshot() {
        let (_scratch, conn) = fixture_conn();
        let account =
            observe_account(&conn, "anthropic", "work", UtcTimestamp::from_unix_nanos(0)).unwrap();

        let first = resolve_policy_snapshot(
            &conn,
            account,
            UtcTimestamp::from_unix_nanos(1_000),
            &policy_5m(),
        )
        .unwrap();
        let reused = resolve_policy_snapshot(
            &conn,
            account,
            UtcTimestamp::from_unix_nanos(2_000),
            &policy_5m(),
        )
        .unwrap();
        assert_eq!(first, reused, "an unchanged policy must reuse the snapshot");

        let changed = resolve_policy_snapshot(
            &conn,
            account,
            UtcTimestamp::from_unix_nanos(3_000),
            &policy_15m(),
        )
        .unwrap();
        assert_ne!(
            changed, first,
            "a changed policy must create a new snapshot"
        );
    }

    /// Reconstructing the expected-opportunity denominator across a cadence
    /// change reads the snapshot that was in force at the queried instant, not
    /// today's configuration; an instant before any snapshot existed reports
    /// the policy as unknown rather than substituting the earliest one.
    #[test]
    fn the_effective_policy_at_a_past_instant_follows_the_historical_policy() {
        let (_scratch, conn) = fixture_conn();
        let account =
            observe_account(&conn, "anthropic", "work", UtcTimestamp::from_unix_nanos(0)).unwrap();

        resolve_policy_snapshot(
            &conn,
            account,
            UtcTimestamp::from_unix_nanos(1_000),
            &policy_5m(),
        )
        .unwrap();
        resolve_policy_snapshot(
            &conn,
            account,
            UtcTimestamp::from_unix_nanos(5_000),
            &policy_15m(),
        )
        .unwrap();

        let before_any_snapshot =
            effective_policy_at(&conn, account, UtcTimestamp::from_unix_nanos(500)).unwrap();
        assert!(
            before_any_snapshot.is_none(),
            "an interval with no snapshot must report policy unknown, not a substituted default"
        );

        let during_5m = effective_policy_at(&conn, account, UtcTimestamp::from_unix_nanos(3_000))
            .unwrap()
            .unwrap();
        assert_eq!(
            during_5m.policy().ordinary_cadence,
            MonotonicDuration::from_seconds(300)
        );

        let during_15m = effective_policy_at(&conn, account, UtcTimestamp::from_unix_nanos(9_000))
            .unwrap()
            .unwrap();
        assert_eq!(
            during_15m.policy().ordinary_cadence,
            MonotonicDuration::from_seconds(900)
        );
    }

    /// A snapshot naming an account that does not exist fails at the database:
    /// the foreign key from `sampling_policy_snapshot` to `account` is
    /// enforced, not merely documented.
    #[test]
    fn a_snapshot_for_a_nonexistent_account_is_refused_by_the_foreign_key() {
        let (_scratch, conn) = fixture_conn();
        let orphan_account = AccountId::new(9_999);
        let err = insert_snapshot(
            &conn,
            orphan_account,
            UtcTimestamp::from_unix_nanos(0),
            &policy_5m(),
        )
        .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("foreign key"),
            "expected a foreign key violation: {err}"
        );
    }
}
