//! The per-account sampling lease: at most one sampler per account at a time.
//!
//! Three triggers can fire at once (the periodic scheduler, a shell or agent
//! hook, and somebody typing a command). Without coordination they produce
//! three attempts a second apart, which wastes the provider's rate budget and,
//! worse, writes attempt history that misrepresents the sampling cadence when
//! `coverage` reads it back later (PLAN.md sections 12.16, 14.2).
//!
//! The lease is per account, because two unrelated provider accounts have no
//! reason to serialize against each other, and it expires by wall-clock time,
//! so a holder that dies mid-attempt blocks its account for at most one TTL
//! instead of until somebody notices.
//!
//! It is operational metadata, not measurement evidence: disposable, excluded
//! from backup and rebuild targets, and safe to recreate empty. That is why
//! [`clear_expired`] exists for `doctor --fix` and why the table below carries
//! no foreign key into the evidence chain. This module exposes the typed health
//! fact and the repair capability; `aub-n27.7` owns the `doctor` command that
//! aggregates and renders them.

use crate::domain::time::{Clock, MonotonicDuration, UtcTimestamp};
use crate::error::Error;

/// How long a lease lives when the caller states no other TTL.
///
/// One sampling attempt is bounded by the command budget, whose configured
/// default is 8 seconds, so 30 seconds leaves room for process start, the
/// attempt and its commit while keeping the block a crashed holder imposes far
/// below the 5 minute default sampling interval: a dead holder costs at most
/// one skipped opportunity, never a stalled account. Callers with a different
/// budget pass their own TTL to [`acquire`]; this is the documented default,
/// defined here once and read, never copied.
pub const DEFAULT_LEASE_TTL: MonotonicDuration = MonotonicDuration::from_seconds(30);

/// The configured logical name of an account, as `accounts[].name` in the
/// configuration file spells it.
///
/// The lease keys on the configured name rather than on a store-assigned
/// account identifier because a lease must be acquirable before the account has
/// ever been sampled, which is precisely when no account row exists yet.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AccountName(String);

impl AccountName {
    pub fn new(value: impl Into<String>) -> Self {
        AccountName(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Who holds a lease: an identifier for the invocation that acquired it, such
/// as the trigger kind and process id.
///
/// It is recorded so that a refusal can name the holder rather than reporting
/// an anonymous conflict, and so that [`release`] can refuse to drop a lease
/// the caller does not own.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LeaseHolder(String);

impl LeaseHolder {
    pub fn new(value: impl Into<String>) -> Self {
        LeaseHolder(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One lease row: who holds which account, from when until when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamplingLease {
    pub account: AccountName,
    pub holder: LeaseHolder,
    pub acquired_at: UtcTimestamp,
    pub expires_at: UtcTimestamp,
}

/// What an acquisition attempt produced.
///
/// An exhaustive enum rather than an `Option`, so that a caller rendering a
/// refusal has the holder and the expiry to name and cannot silently treat
/// "somebody else is sampling this account" as "nothing happened".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseOutcome {
    /// The caller now holds the lease described here.
    Granted(SamplingLease),
    /// Another holder's lease is still live; this is that lease.
    AlreadyHeld(SamplingLease),
}

/// The health fact `doctor` reads: how many leases are live and how many have
/// expired without being released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseHealth {
    /// Leases whose expiry is still in the future.
    pub live: usize,
    /// Leases past their expiry, which [`clear_expired`] can remove.
    pub expired: usize,
}

/// Acquires the lease for `account`, or reports the live lease that blocks it.
///
/// The whole decision runs in one `BEGIN IMMEDIATE` transaction, which takes
/// SQLite's write lock before the read: two callers racing therefore serialize,
/// the second sees the first one's committed row, and exactly one of them is
/// granted. Doing the read outside the write lock would let both read an empty
/// table and both write, which is the bug this transaction shape exists to
/// prevent.
///
/// An expired lease is overwritten in place with no manual intervention: it is
/// read, found to be past its expiry, and replaced. The comparison is
/// `expires_at > now`, so a lease is live strictly before its expiry instant
/// and expired at it.
pub fn acquire(
    conn: &mut rusqlite::Connection,
    account: &AccountName,
    holder: &LeaseHolder,
    ttl: MonotonicDuration,
    clock: &dyn Clock,
) -> Result<LeaseOutcome, Error> {
    if ttl.as_nanos() == 0 {
        return Err(Error::Usage(
            "a sampling lease TTL must be greater than zero: a lease that expires when it is granted excludes nobody".to_string(),
        ));
    }

    let now = clock.now();
    let expires_at = expiry(now, ttl)?;

    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| store_error("cannot take the sampling lease write lock", e))?;

    if let Some(existing) = read_lease(&tx, account)?
        && existing.expires_at.unix_nanos() > now.unix_nanos()
    {
        drop(tx);
        return Ok(LeaseOutcome::AlreadyHeld(existing));
    }

    tx.execute(
        "INSERT OR REPLACE INTO sampling_lease (account_name, holder, acquired_at, expires_at) \
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            account.as_str(),
            holder.as_str(),
            now.unix_nanos(),
            expires_at.unix_nanos(),
        ],
    )
    .map_err(|e| store_error("cannot write the sampling lease", e))?;

    tx.commit()
        .map_err(|e| store_error("cannot commit the sampling lease", e))?;

    Ok(LeaseOutcome::Granted(SamplingLease {
        account: account.clone(),
        holder: holder.clone(),
        acquired_at: now,
        expires_at,
    }))
}

/// Releases `account`'s lease if `holder` is the one who holds it, reporting
/// whether a row was removed.
///
/// The holder is part of the condition because a lease that expired and was
/// taken over by somebody else must not be released by the previous holder
/// finishing late: that would hand the account to a third caller while the
/// current holder is still sampling it.
pub fn release(
    conn: &rusqlite::Connection,
    account: &AccountName,
    holder: &LeaseHolder,
) -> Result<bool, Error> {
    let removed = conn
        .execute(
            "DELETE FROM sampling_lease WHERE account_name = ?1 AND holder = ?2",
            rusqlite::params![account.as_str(), holder.as_str()],
        )
        .map_err(|e| store_error("cannot release the sampling lease", e))?;
    Ok(removed > 0)
}

/// Removes every lease already past its expiry, reporting how many went.
///
/// The repair half of the `doctor` integration: expired rows are the only lease
/// state that is ever safe to delete, and a live lease is never touched, since
/// deleting one would let a second sampler run against an account somebody is
/// already sampling.
pub fn clear_expired(conn: &rusqlite::Connection, clock: &dyn Clock) -> Result<usize, Error> {
    let now = clock.now().unix_nanos();
    let removed = conn
        .execute(
            "DELETE FROM sampling_lease WHERE expires_at <= ?1",
            rusqlite::params![now],
        )
        .map_err(|e| store_error("cannot clear expired sampling leases", e))?;
    Ok(removed)
}

/// The live and expired lease counts, as of `clock`.
pub fn health(conn: &rusqlite::Connection, clock: &dyn Clock) -> Result<LeaseHealth, Error> {
    let now = clock.now().unix_nanos();
    let live: i64 = conn
        .query_row(
            "SELECT count(*) FROM sampling_lease WHERE expires_at > ?1",
            rusqlite::params![now],
            |row| row.get(0),
        )
        .map_err(|e| store_error("cannot count live sampling leases", e))?;
    let expired: i64 = conn
        .query_row(
            "SELECT count(*) FROM sampling_lease WHERE expires_at <= ?1",
            rusqlite::params![now],
            |row| row.get(0),
        )
        .map_err(|e| store_error("cannot count expired sampling leases", e))?;
    Ok(LeaseHealth {
        live: live as usize,
        expired: expired as usize,
    })
}

/// The lease currently recorded for `account`, live or not.
fn read_lease(
    conn: &rusqlite::Connection,
    account: &AccountName,
) -> Result<Option<SamplingLease>, Error> {
    let mut statement = conn
        .prepare(
            "SELECT holder, acquired_at, expires_at FROM sampling_lease WHERE account_name = ?1",
        )
        .map_err(|e| store_error("cannot read the sampling lease", e))?;
    let mut rows = statement
        .query(rusqlite::params![account.as_str()])
        .map_err(|e| store_error("cannot read the sampling lease", e))?;
    let Some(row) = rows
        .next()
        .map_err(|e| store_error("cannot read the sampling lease", e))?
    else {
        return Ok(None);
    };
    let holder: String = row
        .get(0)
        .map_err(|e| store_error("cannot read the lease holder", e))?;
    let acquired_at: i64 = row
        .get(1)
        .map_err(|e| store_error("cannot read the lease acquisition time", e))?;
    let expires_at: i64 = row
        .get(2)
        .map_err(|e| store_error("cannot read the lease expiry", e))?;
    Ok(Some(SamplingLease {
        account: account.clone(),
        holder: LeaseHolder::new(holder),
        acquired_at: UtcTimestamp::from_unix_nanos(acquired_at),
        expires_at: UtcTimestamp::from_unix_nanos(expires_at),
    }))
}

/// `now` plus `ttl`, refusing an overflow rather than wrapping into the past.
///
/// A wrapped expiry would read as a lease that expired before it was acquired,
/// which the table's own check constraint rejects; refusing here names the
/// cause instead of surfacing a constraint violation.
fn expiry(now: UtcTimestamp, ttl: MonotonicDuration) -> Result<UtcTimestamp, Error> {
    let nanos = i64::try_from(ttl.as_nanos())
        .ok()
        .and_then(|ttl_nanos| now.unix_nanos().checked_add(ttl_nanos))
        .ok_or_else(|| {
            Error::Usage(format!(
                "a sampling lease TTL of {} nanoseconds does not fit in the representable time range",
                ttl.as_nanos()
            ))
        })?;
    Ok(UtcTimestamp::from_unix_nanos(nanos))
}

/// Maps a rusqlite failure to the store class, naming the lease when the
/// failure is a busy database so a contended acquisition says what it waited
/// for.
fn store_error(context: &str, err: rusqlite::Error) -> Error {
    let busy = matches!(
        &err,
        rusqlite::Error::SqliteFailure(code, _)
            if code.code == rusqlite::ErrorCode::DatabaseBusy
    );
    if busy {
        Error::Store(format!("sampling lease: database busy: {context}: {err}"))
    } else {
        Error::Store(format!("{context}: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::FakeClock;
    use crate::error::ExitClass;
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::store::migrate::run_migrations;
    use crate::store::migrations::migration_0002::migration as lease_migration;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A fresh scratch directory under the system temp dir, removed on drop.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-store-lease-test-{}-{suffix}",
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

    fn policy() -> PragmaPolicy {
        PragmaPolicy {
            busy_timeout: MonotonicDuration::from_seconds(5),
        }
    }

    fn clock_at(nanos: i64) -> FakeClock {
        FakeClock::new(UtcTimestamp::from_unix_nanos(nanos))
    }

    /// A database with the lease table in place, opened for write.
    ///
    /// The schema is applied through the migration this bead ships and the
    /// framework that runs it, so the test exercises the same table definition
    /// production gets rather than a hand-written copy of it. The registry
    /// passed here is the prefix of the real one up to this bead's version,
    /// which keeps the test independent of migrations other beads add later.
    fn migrated_db(scratch: &ScratchDir) -> rusqlite::Connection {
        let path = scratch.path().join("meter.db");
        let mut conn = open(&path, AccessMode::ReadWrite, &policy()).expect("database must open");
        let clock = clock_at(0);
        let registry = crate::store::migrations::registry();
        let up_to_lease: Vec<_> = registry
            .into_iter()
            .filter(|migration| migration.version <= lease_migration().version)
            .collect();
        run_migrations(&mut conn, &up_to_lease, None, &clock).expect("migrations must apply");
        conn
    }

    fn open_second(scratch: &ScratchDir) -> rusqlite::Connection {
        let path = scratch.path().join("meter.db");
        open(&path, AccessMode::ReadWrite, &policy()).expect("database must open")
    }

    fn granted(outcome: LeaseOutcome) -> SamplingLease {
        match outcome {
            LeaseOutcome::Granted(lease) => lease,
            LeaseOutcome::AlreadyHeld(lease) => {
                panic!("expected a grant, got a lease held by {:?}", lease.holder)
            }
        }
    }

    #[test]
    fn a_live_lease_refuses_a_second_holder_and_names_the_first() {
        let scratch = ScratchDir::new();
        let mut conn = migrated_db(&scratch);
        let account = AccountName::new("work-primary");
        let clock = clock_at(1_000);

        let first = acquire(
            &mut conn,
            &account,
            &LeaseHolder::new("timer-1"),
            DEFAULT_LEASE_TTL,
            &clock,
        )
        .expect("the first acquisition must succeed");
        assert!(matches!(first, LeaseOutcome::Granted(_)));

        let second = acquire(
            &mut conn,
            &account,
            &LeaseHolder::new("hook-2"),
            DEFAULT_LEASE_TTL,
            &clock,
        )
        .expect("the second acquisition must report rather than fail");

        match second {
            LeaseOutcome::Granted(lease) => {
                panic!(
                    "a live lease was handed to a second holder {:?}",
                    lease.holder
                )
            }
            LeaseOutcome::AlreadyHeld(lease) => {
                assert_eq!(lease.holder, LeaseHolder::new("timer-1"));
                assert_eq!(lease.acquired_at.unix_nanos(), 1_000);
            }
        }
    }

    #[test]
    fn an_expired_lease_is_acquirable_without_intervention() {
        let scratch = ScratchDir::new();
        let mut conn = migrated_db(&scratch);
        let account = AccountName::new("work-primary");
        let mut clock = clock_at(1_000);

        acquire(
            &mut conn,
            &account,
            &LeaseHolder::new("timer-1"),
            DEFAULT_LEASE_TTL,
            &clock,
        )
        .expect("the first acquisition must succeed");

        // One nanosecond before the expiry the lease is still live, which is
        // the near-identical negative of the case below: the two differ only in
        // the single dimension the expiry rule is about.
        clock.advance(MonotonicDuration::from_nanos(
            DEFAULT_LEASE_TTL.as_nanos() - 1,
        ));
        let still_live = acquire(
            &mut conn,
            &account,
            &LeaseHolder::new("hook-2"),
            DEFAULT_LEASE_TTL,
            &clock,
        )
        .expect("acquisition must report rather than fail");
        assert!(
            matches!(still_live, LeaseOutcome::AlreadyHeld(_)),
            "a lease one nanosecond short of its expiry is still live"
        );

        clock.advance(MonotonicDuration::from_nanos(1));
        let taken_over = granted(
            acquire(
                &mut conn,
                &account,
                &LeaseHolder::new("hook-2"),
                DEFAULT_LEASE_TTL,
                &clock,
            )
            .expect("acquisition must succeed once the lease expired"),
        );
        assert_eq!(taken_over.holder, LeaseHolder::new("hook-2"));
    }

    #[test]
    fn releasing_frees_the_account_for_the_next_holder() {
        let scratch = ScratchDir::new();
        let mut conn = migrated_db(&scratch);
        let account = AccountName::new("work-primary");
        let holder = LeaseHolder::new("timer-1");
        let clock = clock_at(1_000);

        acquire(&mut conn, &account, &holder, DEFAULT_LEASE_TTL, &clock)
            .expect("acquisition must succeed");
        assert!(release(&conn, &account, &holder).expect("release must succeed"));

        let next = granted(
            acquire(
                &mut conn,
                &account,
                &LeaseHolder::new("hook-2"),
                DEFAULT_LEASE_TTL,
                &clock,
            )
            .expect("acquisition must succeed after a release"),
        );
        assert_eq!(next.holder, LeaseHolder::new("hook-2"));
    }

    #[test]
    fn a_stale_holder_cannot_release_the_lease_that_replaced_its_own() {
        let scratch = ScratchDir::new();
        let mut conn = migrated_db(&scratch);
        let account = AccountName::new("work-primary");
        let stale = LeaseHolder::new("timer-1");
        let mut clock = clock_at(1_000);

        acquire(&mut conn, &account, &stale, DEFAULT_LEASE_TTL, &clock)
            .expect("acquisition must succeed");
        clock.advance(DEFAULT_LEASE_TTL);
        let current = LeaseHolder::new("hook-2");
        acquire(&mut conn, &account, &current, DEFAULT_LEASE_TTL, &clock)
            .expect("the expired lease must be acquirable");

        assert!(
            !release(&conn, &account, &stale).expect("release must report rather than fail"),
            "the previous holder finishing late must not release the current holder's lease"
        );
        let health = health(&conn, &clock).expect("health must be readable");
        assert_eq!(health.live, 1);
    }

    #[test]
    fn clear_expired_removes_expired_leases_and_leaves_live_ones() {
        let scratch = ScratchDir::new();
        let mut conn = migrated_db(&scratch);
        let mut clock = clock_at(1_000);

        acquire(
            &mut conn,
            &AccountName::new("expired-account"),
            &LeaseHolder::new("timer-1"),
            MonotonicDuration::from_seconds(1),
            &clock,
        )
        .expect("acquisition must succeed");
        clock.advance(MonotonicDuration::from_seconds(2));
        acquire(
            &mut conn,
            &AccountName::new("live-account"),
            &LeaseHolder::new("timer-2"),
            DEFAULT_LEASE_TTL,
            &clock,
        )
        .expect("acquisition must succeed");

        assert_eq!(
            health(&conn, &clock).expect("health must be readable"),
            LeaseHealth {
                live: 1,
                expired: 1
            }
        );
        assert_eq!(
            clear_expired(&conn, &clock).expect("the repair must succeed"),
            1
        );
        assert_eq!(
            health(&conn, &clock).expect("health must be readable"),
            LeaseHealth {
                live: 1,
                expired: 0
            }
        );
    }

    #[test]
    fn a_zero_ttl_is_refused_as_a_usage_failure() {
        let scratch = ScratchDir::new();
        let mut conn = migrated_db(&scratch);
        let error = acquire(
            &mut conn,
            &AccountName::new("work-primary"),
            &LeaseHolder::new("timer-1"),
            MonotonicDuration::from_nanos(0),
            &clock_at(1_000),
        )
        .expect_err("a zero TTL must be refused");
        assert_eq!(error.exit_class(), ExitClass::Usage);
    }

    /// Three concurrent acquisitions for one account grant exactly one holder.
    ///
    /// Each thread opens its own connection, so the serialization under test is
    /// SQLite's write lock taken by `BEGIN IMMEDIATE` rather than anything the
    /// test shares between the threads.
    #[test]
    fn three_concurrent_acquisitions_for_one_account_grant_exactly_one() {
        let scratch = ScratchDir::new();
        // Created and dropped so the schema exists before the threads start.
        drop(migrated_db(&scratch));
        let account = AccountName::new("work-primary");
        let clock = clock_at(1_000);

        let outcomes: Vec<LeaseOutcome> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..3)
                .map(|i| {
                    let account = account.clone();
                    let scratch = &scratch;
                    let clock = clock;
                    scope.spawn(move || {
                        let mut conn = open_second(scratch);
                        acquire(
                            &mut conn,
                            &account,
                            &LeaseHolder::new(format!("trigger-{i}")),
                            DEFAULT_LEASE_TTL,
                            &clock,
                        )
                        .expect("every acquisition must report rather than fail")
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("no thread may panic"))
                .collect()
        });

        let grants = outcomes
            .iter()
            .filter(|outcome| matches!(outcome, LeaseOutcome::Granted(_)))
            .count();
        assert_eq!(
            grants, 1,
            "exactly one of three acquisitions may be granted"
        );
    }

    /// Two unrelated accounts acquire concurrently, which is what makes the
    /// lease per account rather than global: a global lease would refuse one of
    /// these two and still pass every single-account test above.
    #[test]
    fn two_unrelated_accounts_are_sampled_concurrently() {
        let scratch = ScratchDir::new();
        drop(migrated_db(&scratch));
        let clock = clock_at(1_000);

        let outcomes: Vec<LeaseOutcome> = std::thread::scope(|scope| {
            let handles: Vec<_> = ["work-primary", "personal"]
                .into_iter()
                .map(|name| {
                    let scratch = &scratch;
                    let clock = clock;
                    scope.spawn(move || {
                        let mut conn = open_second(scratch);
                        acquire(
                            &mut conn,
                            &AccountName::new(name),
                            &LeaseHolder::new(format!("timer-{name}")),
                            DEFAULT_LEASE_TTL,
                            &clock,
                        )
                        .expect("every acquisition must report rather than fail")
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("no thread may panic"))
                .collect()
        });

        assert!(
            outcomes
                .iter()
                .all(|outcome| matches!(outcome, LeaseOutcome::Granted(_))),
            "unrelated accounts must not serialize against each other"
        );
    }
}
