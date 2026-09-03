//! The `account` table: stable logical account identity (PLAN.md 12.1).
//!
//! Deliberately carries no credential material and no authoritative mutable
//! plan-tier column: plan and tier are time-varying evidence that belongs on
//! observations or explicit account state intervals, never rewritten in place
//! on this row.

use rusqlite::{OptionalExtension, params};

use crate::domain::time::UtcTimestamp;
use crate::error::Error;

/// An account row's identity: its SQLite rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AccountId(i64);

impl AccountId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

/// Stable logical account identity: a configured name under a provider, with
/// the span of time this binary has observed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    id: AccountId,
    logical_name: String,
    provider_key: String,
    first_observed_at: UtcTimestamp,
    last_observed_at: UtcTimestamp,
}

impl Account {
    pub fn id(&self) -> AccountId {
        self.id
    }

    pub fn logical_name(&self) -> &str {
        &self.logical_name
    }

    pub fn provider_key(&self) -> &str {
        &self.provider_key
    }

    pub fn first_observed_at(&self) -> UtcTimestamp {
        self.first_observed_at
    }

    pub fn last_observed_at(&self) -> UtcTimestamp {
        self.last_observed_at
    }
}

fn row_to_account(row: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account {
        id: AccountId::new(row.get(0)?),
        logical_name: row.get(1)?,
        provider_key: row.get(2)?,
        first_observed_at: UtcTimestamp::from_unix_nanos(row.get(3)?),
        last_observed_at: UtcTimestamp::from_unix_nanos(row.get(4)?),
    })
}

/// Records an observation of an account: creates the row on first sight, and on
/// every later sight advances `last_observed_at` without disturbing
/// `first_observed_at`. The identity key is `(provider_key, logical_name)`; the
/// same pair from the same provider is always the same account, so a caller
/// never has to look one up before recording a sighting of it.
pub fn observe_account(
    conn: &rusqlite::Connection,
    provider_key: &str,
    logical_name: &str,
    observed_at: UtcTimestamp,
) -> Result<AccountId, Error> {
    conn.query_row(
        "INSERT INTO account (logical_name, provider_key, first_observed_at, last_observed_at)
         VALUES (?1, ?2, ?3, ?3)
         ON CONFLICT (provider_key, logical_name) DO UPDATE SET
             last_observed_at = MAX(last_observed_at, excluded.last_observed_at)
         RETURNING id",
        params![logical_name, provider_key, observed_at.unix_nanos()],
        |row| row.get(0),
    )
    .map(AccountId::new)
    .map_err(|e| Error::Store(format!("cannot record account observation: {e}")))
}

/// Reads the account row id for an identity pair, or `None` when no such
/// account has ever been observed. This is the read half of the identity
/// lookup the sampler's due decision needs before any attempt exists: an
/// account with no row has no history, which is itself the due answer.
pub fn account_id_by_identity(
    conn: &rusqlite::Connection,
    provider_key: &str,
    logical_name: &str,
) -> Result<Option<AccountId>, Error> {
    conn.query_row(
        "SELECT id FROM account WHERE provider_key = ?1 AND logical_name = ?2",
        params![provider_key, logical_name],
        |row| row.get::<_, i64>(0).map(AccountId::new),
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot look up the account row: {e}")))
}

/// Reads every account row in identity order.
pub fn all_accounts(conn: &rusqlite::Connection) -> Result<Vec<Account>, Error> {
    let mut statement = conn
        .prepare(
            "SELECT id, logical_name, provider_key, first_observed_at, last_observed_at
             FROM account ORDER BY id",
        )
        .map_err(|e| Error::Store(format!("cannot list accounts: {e}")))?;
    let rows = statement
        .query_map([], row_to_account)
        .map_err(|e| Error::Store(format!("cannot list accounts: {e}")))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| Error::Store(format!("cannot read accounts: {e}")))
}

/// Reads one account by id, or `None` if no such account exists.
pub fn account_by_id(conn: &rusqlite::Connection, id: AccountId) -> Result<Option<Account>, Error> {
    conn.query_row(
        "SELECT id, logical_name, provider_key, first_observed_at, last_observed_at
         FROM account WHERE id = ?1",
        params![id.value()],
        row_to_account,
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot read account {}: {e}", id.value())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-store-account-test-{}-{suffix}",
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
            busy_timeout: crate::domain::time::MonotonicDuration::from_millis(1000),
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

    #[test]
    fn observing_an_account_twice_advances_last_observed_and_keeps_first_observed() {
        let (_scratch, conn) = fixture_conn();
        let first = observe_account(
            &conn,
            "anthropic",
            "work",
            UtcTimestamp::from_unix_nanos(1_000),
        )
        .unwrap();
        let second = observe_account(
            &conn,
            "anthropic",
            "work",
            UtcTimestamp::from_unix_nanos(5_000),
        )
        .unwrap();
        assert_eq!(
            first, second,
            "the same (provider, name) pair is one account"
        );

        let account = account_by_id(&conn, first).unwrap().unwrap();
        assert_eq!(
            account.first_observed_at(),
            UtcTimestamp::from_unix_nanos(1_000)
        );
        assert_eq!(
            account.last_observed_at(),
            UtcTimestamp::from_unix_nanos(5_000)
        );
    }

    #[test]
    fn different_provider_keys_are_different_accounts_even_with_the_same_name() {
        let (_scratch, conn) = fixture_conn();
        let a =
            observe_account(&conn, "anthropic", "work", UtcTimestamp::from_unix_nanos(0)).unwrap();
        let b = observe_account(&conn, "openai", "work", UtcTimestamp::from_unix_nanos(0)).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn identity_lookup_finds_an_observed_account_and_misses_an_unobserved_one() {
        let (_scratch, conn) = fixture_conn();
        let created =
            observe_account(&conn, "anthropic", "work", UtcTimestamp::from_unix_nanos(0)).unwrap();
        assert_eq!(
            account_id_by_identity(&conn, "anthropic", "work").unwrap(),
            Some(created),
            "the identity pair of an observed account must resolve to its row"
        );
        assert_eq!(
            account_id_by_identity(&conn, "anthropic", "never-sampled").unwrap(),
            None,
            "an account never observed has no row and no history"
        );
    }

    /// Planted negative: the account row carries no plan-tier column at all, so
    /// a write naming one fails at the database rather than silently landing on
    /// a row that would then rewrite calibration-relevant history in place.
    #[test]
    fn a_write_naming_a_plan_tier_column_is_rejected() {
        let (_scratch, conn) = fixture_conn();
        let err = conn
            .execute(
                "INSERT INTO account (logical_name, provider_key, first_observed_at, last_observed_at, plan_tier)
                 VALUES ('work', 'anthropic', 0, 0, 'pro')",
                [],
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("plan_tier"),
            "expected the failure to name the rejected column: {err}"
        );
    }
}
