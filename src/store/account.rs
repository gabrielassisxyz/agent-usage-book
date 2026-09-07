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

    pub fn identity(&self) -> AccountIdentity {
        AccountIdentity::new(&self.provider_key, &self.logical_name)
    }
}

/// The logical identity of an account: (provider_key, logical_name).
///
/// The database schema enforces `UNIQUE (provider_key, logical_name)`, and
/// the sampler matches configured accounts to database rows through this
/// exact identity pair.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AccountIdentity {
    provider_key: String,
    logical_name: String,
}

impl AccountIdentity {
    pub fn new(provider_key: impl Into<String>, logical_name: impl Into<String>) -> Self {
        Self {
            provider_key: provider_key.into(),
            logical_name: logical_name.into(),
        }
    }

    pub fn provider_key(&self) -> &str {
        &self.provider_key
    }

    pub fn logical_name(&self) -> &str {
        &self.logical_name
    }
}

impl From<&crate::config::AccountConfig> for AccountIdentity {
    fn from(config: &crate::config::AccountConfig) -> Self {
        Self::new(&config.provider, &config.name)
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

/// Reads one account id by its structured identity pair.
pub fn account_id_by_account_identity(
    conn: &rusqlite::Connection,
    identity: &AccountIdentity,
) -> Result<Option<AccountId>, Error> {
    account_id_by_identity(conn, identity.provider_key(), identity.logical_name())
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

/// Renames a configured account's logical name everywhere the ledger stores
/// it as text, in one transaction: the `account` row itself and the three
/// side tables that carry the name outside it rather than through
/// [`AccountId`] (`sampling_lease.account_name`,
/// `session_account_marker.logical_account`,
/// `account_attribution_segment.logical_account`). Evidence tables
/// (`meter_observation`, `meter_window`, `meter_attempt`, `usage_*`) are
/// never touched: they key on the account id, which a rename does not
/// change, and their own triggers forbid rewriting them regardless.
///
/// None of the three side tables carries `provider_key`, so the text rewrite
/// on them matches by name alone; the configured naming convention this bead
/// was written against keeps names distinct across providers by construction
/// (`codex-primary` vs `primary`, `k1`/`k2`/`k3`), and nothing here assumes
/// otherwise beyond matching what is actually stored.
///
/// Refuses with [`Error::Usage`] when `(provider_key, new_name)` already
/// names another row, when `(provider_key, old_name)` names no row, or when
/// the sampler holds a live lease on `old_name`: a lease in flight is an
/// attempt already under way against the name this call is about to retire.
/// The caller checks the fourth refusal, that the configuration still names
/// `old_name`, before this call: the store has no configuration to read.
pub fn rename_account(
    conn: &mut rusqlite::Connection,
    provider_key: &str,
    old_name: &str,
    new_name: &str,
    now: UtcTimestamp,
) -> Result<AccountId, Error> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| Error::Store(format!("cannot start the account rename transaction: {e}")))?;

    let account_id: i64 = tx
        .query_row(
            "SELECT id FROM account WHERE provider_key = ?1 AND logical_name = ?2",
            params![provider_key, old_name],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| Error::Store(format!("cannot look up account '{old_name}': {e}")))?
        .ok_or_else(|| {
            Error::Usage(format!(
                "no account named '{old_name}' is recorded for provider '{provider_key}'"
            ))
        })?;

    let clash: Option<i64> = tx
        .query_row(
            "SELECT id FROM account WHERE provider_key = ?1 AND logical_name = ?2",
            params![provider_key, new_name],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| Error::Store(format!("cannot look up account '{new_name}': {e}")))?;
    if let Some(existing_id) = clash {
        return Err(Error::Usage(format!(
            "account '{new_name}' already exists for provider '{provider_key}' (row {existing_id}); rename that row out of the way first"
        )));
    }

    let live_lease_holder: Option<String> = tx
        .query_row(
            "SELECT holder FROM sampling_lease WHERE account_name = ?1 AND expires_at > ?2",
            params![old_name, now.unix_nanos()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| {
            Error::Store(format!(
                "cannot check the sampling lease on '{old_name}': {e}"
            ))
        })?;
    if let Some(holder) = live_lease_holder {
        return Err(Error::Usage(format!(
            "the sampler holds a live lease on '{old_name}' (held by {holder}); wait for it to expire or release it before renaming"
        )));
    }

    tx.execute(
        "UPDATE account SET logical_name = ?1 WHERE provider_key = ?2 AND logical_name = ?3",
        params![new_name, provider_key, old_name],
    )
    .map_err(|e| Error::Store(format!("cannot rename the account row: {e}")))?;
    tx.execute(
        "UPDATE sampling_lease SET account_name = ?1 WHERE account_name = ?2",
        params![new_name, old_name],
    )
    .map_err(|e| Error::Store(format!("cannot rename sampling_lease rows: {e}")))?;
    tx.execute(
        "UPDATE session_account_marker SET logical_account = ?1 WHERE logical_account = ?2",
        params![new_name, old_name],
    )
    .map_err(|e| Error::Store(format!("cannot rename session_account_marker rows: {e}")))?;
    tx.execute(
        "UPDATE account_attribution_segment SET logical_account = ?1 WHERE logical_account = ?2",
        params![new_name, old_name],
    )
    .map_err(|e| {
        Error::Store(format!(
            "cannot rename account_attribution_segment rows: {e}"
        ))
    })?;

    crate::store::ledger_generation::advance(&tx)?;

    tx.commit()
        .map_err(|e| Error::Store(format!("cannot commit the account rename: {e}")))?;

    Ok(AccountId::new(account_id))
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
        let id = AccountIdentity::new("anthropic", "work");
        assert_eq!(
            account_id_by_account_identity(&conn, &id).unwrap(),
            Some(created),
            "the structured identity must resolve to its row"
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

    /// Seeds one row in each of the three text-keyed side tables the rename
    /// must also rewrite, all under `account_name`/`logical_account` = 'max'.
    fn seed_max_side_rows(conn: &rusqlite::Connection, resolved_account_id: i64) {
        conn.execute(
            "INSERT INTO sampling_lease (account_name, holder, acquired_at, expires_at)
             VALUES ('max', 'timer-1', 100, 200)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_account_marker
                (session_source, session_native, observed_at, source_ordering_key,
                 logical_account, resolved_account_id, marker_source, run_source,
                 run_native, evidence_designation)
             VALUES ('claude-code', 'sess-1', 1500, NULL, 'max', ?1, 'hook', NULL, NULL, 'launcher_or_hook')",
            params![resolved_account_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO account_attribution_segment
                (session_id, target_kind, logical_account, input_tokens, output_tokens,
                 cache_read_tokens, cache_write_tokens, computed_at)
             VALUES ('sess-1', 'account', 'max', 10, 20, 0, 0, 1600)",
            [],
        )
        .unwrap();
    }

    #[test]
    fn rename_account_rewrites_the_account_row_and_the_three_text_keyed_tables_and_advances_the_generation()
     {
        let (_scratch, mut conn) = fixture_conn();
        let created = observe_account(
            &conn,
            "anthropic",
            "max",
            UtcTimestamp::from_unix_nanos(1_000),
        )
        .unwrap();
        seed_max_side_rows(&conn, created.value());

        let before_generation = crate::store::ledger_generation::current(&conn).unwrap();

        let renamed_id = rename_account(
            &mut conn,
            "anthropic",
            "max",
            "primary",
            UtcTimestamp::from_unix_nanos(2_000),
        )
        .unwrap();
        assert_eq!(
            renamed_id, created,
            "the rename must not change the account's row identity"
        );

        assert_eq!(
            account_id_by_identity(&conn, "anthropic", "primary").unwrap(),
            Some(created)
        );
        assert_eq!(
            account_id_by_identity(&conn, "anthropic", "max").unwrap(),
            None,
            "the retired name must resolve to nothing once renamed"
        );

        let lease_account: String = conn
            .query_row(
                "SELECT account_name FROM sampling_lease WHERE holder = 'timer-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(lease_account, "primary");

        let marker_account: String = conn
            .query_row(
                "SELECT logical_account FROM session_account_marker WHERE session_native = 'sess-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marker_account, "primary");

        let segment_account: String = conn
            .query_row(
                "SELECT logical_account FROM account_attribution_segment WHERE session_id = 'sess-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(segment_account, "primary");

        let after_generation = crate::store::ledger_generation::current(&conn).unwrap();
        assert_eq!(after_generation.value(), before_generation.value() + 1);
    }

    /// Planted negative: a refusal must leave the retired row exactly where it
    /// was, not partially renamed.
    #[test]
    fn rename_account_refuses_when_the_new_name_already_exists_for_that_provider() {
        let (_scratch, mut conn) = fixture_conn();
        observe_account(
            &conn,
            "anthropic",
            "primary",
            UtcTimestamp::from_unix_nanos(0),
        )
        .unwrap();
        observe_account(&conn, "anthropic", "max", UtcTimestamp::from_unix_nanos(0)).unwrap();

        let err = rename_account(
            &mut conn,
            "anthropic",
            "max",
            "primary",
            UtcTimestamp::from_unix_nanos(1_000),
        )
        .expect_err("renaming into an already-taken name must be refused");
        assert!(matches!(err, Error::Usage(_)), "{err:?}");
        assert!(err.to_string().contains("primary"), "{err}");

        assert!(
            account_id_by_identity(&conn, "anthropic", "max")
                .unwrap()
                .is_some(),
            "a refused rename must leave the old row in place"
        );
    }

    #[test]
    fn rename_account_refuses_when_the_old_name_does_not_exist() {
        let (_scratch, mut conn) = fixture_conn();
        let err = rename_account(
            &mut conn,
            "anthropic",
            "ghost",
            "primary",
            UtcTimestamp::from_unix_nanos(0),
        )
        .expect_err("renaming an account that was never observed must be refused");
        assert!(matches!(err, Error::Usage(_)), "{err:?}");
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    /// Planted negative: one nanosecond before the lease's own expiry the
    /// rename is still refused, and only past it does it succeed; this pins
    /// the rename to the same `expires_at > now` boundary
    /// [`crate::store::sampling_lease::acquire`] uses, rather than an
    /// off-by-one of its own.
    #[test]
    fn rename_account_refuses_while_a_live_lease_holds_the_old_name() {
        let (_scratch, mut conn) = fixture_conn();
        observe_account(&conn, "anthropic", "max", UtcTimestamp::from_unix_nanos(0)).unwrap();
        conn.execute(
            "INSERT INTO sampling_lease (account_name, holder, acquired_at, expires_at)
             VALUES ('max', 'timer-1', 1000, 5000)",
            [],
        )
        .unwrap();

        let err = rename_account(
            &mut conn,
            "anthropic",
            "max",
            "primary",
            UtcTimestamp::from_unix_nanos(4_999),
        )
        .expect_err("a live lease must block the rename");
        assert!(matches!(err, Error::Usage(_)), "{err:?}");
        assert!(err.to_string().contains("timer-1"), "{err}");

        rename_account(
            &mut conn,
            "anthropic",
            "max",
            "primary",
            UtcTimestamp::from_unix_nanos(5_000),
        )
        .expect("an expired lease must not block the rename");
    }
}
