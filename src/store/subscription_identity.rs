//! The `meter_subscription_change` table: the typed record of an account's
//! subscription history (`aub-iwkg`).
//!
//! When the subscription behind an account's credential path changes, the
//! sampler refuses to attribute the new subscription's readings to the old
//! logical name. The refused attempt's result carries the outcome; this
//! table carries the identity pair, so an interval spanning the change can
//! be found later and excluded from calibration evidence. No meter evidence
//! is rewritten at any point: the refused reading is never stored as an
//! observation, and the rows here are irreplaceable (their triggers reject
//! every `UPDATE` and `DELETE`).
//!
//! May not depend on:
//! - HTTP or provider semantics
//! - presentation

use rusqlite::{OptionalExtension, params};

use crate::domain::time::UtcTimestamp;
use crate::error::Error;
use crate::store::account::AccountId;
use crate::store::meter_attempt::MeterAttemptRowId;
use crate::store::meter_evidence::ObservationRowId;

/// A `meter_subscription_change` row's identity: its SQLite rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SubscriptionChangeRowId(i64);

impl SubscriptionChangeRowId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

/// What one subscription-history row records: the first sighting of an
/// account's subscription, or a reading that named a different one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionChangeKind {
    Established,
    Changed,
}

impl SubscriptionChangeKind {
    fn as_sql(self) -> &'static str {
        match self {
            SubscriptionChangeKind::Established => "established",
            SubscriptionChangeKind::Changed => "changed",
        }
    }

    fn from_sql(value: &str) -> Result<Self, Error> {
        match value {
            "established" => Ok(SubscriptionChangeKind::Established),
            "changed" => Ok(SubscriptionChangeKind::Changed),
            other => Err(Error::Store(format!(
                "unknown subscription change kind stored in the database: {other:?}"
            ))),
        }
    }
}

/// One immutable subscription-history row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSubscriptionChange {
    pub row_id: SubscriptionChangeRowId,
    pub account_id: AccountId,
    pub kind: SubscriptionChangeKind,
    /// The established identity a `changed` row superseded; always `None`
    /// on an `established` row, enforced by the table CHECK.
    pub previous_identity: Option<String>,
    pub current_identity: String,
    /// The attempt whose reading surfaced this event.
    pub detecting_attempt_id: MeterAttemptRowId,
    /// The newest stored observation when a `changed` row was recorded: the
    /// interval anchor a later calibration exclusion reads. Always `None`
    /// on an `established` row, enforced by the table CHECK.
    pub previous_observation_id: Option<ObservationRowId>,
    pub detected_at: UtcTimestamp,
}

/// One subscription-history row to record, without its generated row id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSubscriptionChange {
    pub account_id: AccountId,
    pub kind: SubscriptionChangeKind,
    pub previous_identity: Option<String>,
    pub current_identity: String,
    pub detecting_attempt_id: MeterAttemptRowId,
    pub previous_observation_id: Option<ObservationRowId>,
    pub detected_at: UtcTimestamp,
}

const SELECT_COLUMNS: &str = "
    id, account_id, kind, previous_identity, current_identity,
    detecting_attempt_id, previous_observation_id, detected_at";

fn row_to_change(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredSubscriptionChange> {
    Ok(StoredSubscriptionChange {
        row_id: SubscriptionChangeRowId::new(row.get("id")?),
        account_id: AccountId::new(row.get("account_id")?),
        kind: SubscriptionChangeKind::from_sql(row.get::<_, String>("kind")?.as_str()).map_err(
            |_| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    "unknown subscription change kind".into(),
                )
            },
        )?,
        previous_identity: row.get("previous_identity")?,
        current_identity: row.get("current_identity")?,
        detecting_attempt_id: MeterAttemptRowId::new(row.get("detecting_attempt_id")?),
        previous_observation_id: row
            .get::<_, Option<i64>>("previous_observation_id")?
            .map(ObservationRowId::new),
        detected_at: UtcTimestamp::from_unix_nanos(row.get("detected_at")?),
    })
}

/// Records one subscription-history row. The table CHECK refuses an
/// `established` row naming anything previous and a `changed` row naming no
/// previous identity, so an illegal state fails here rather than later.
pub fn insert_change(
    conn: &rusqlite::Connection,
    change: &NewSubscriptionChange,
) -> Result<SubscriptionChangeRowId, Error> {
    conn.query_row(
        "INSERT INTO meter_subscription_change (
            account_id, kind, previous_identity, current_identity,
            detecting_attempt_id, previous_observation_id, detected_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) RETURNING id",
        params![
            change.account_id.value(),
            change.kind.as_sql(),
            change.previous_identity,
            change.current_identity,
            change.detecting_attempt_id.value(),
            change.previous_observation_id.map(ObservationRowId::value),
            change.detected_at.unix_nanos(),
        ],
        |row| row.get(0),
    )
    .map(SubscriptionChangeRowId::new)
    .map_err(|e| Error::Store(format!("cannot record the subscription change: {e}")))
}

/// The newest subscription-history row for one account, or `None` when the
/// account's subscription has never been sighted.
pub fn latest_for_account(
    conn: &rusqlite::Connection,
    account_id: AccountId,
) -> Result<Option<StoredSubscriptionChange>, Error> {
    conn.query_row(
        &format!(
            "SELECT {SELECT_COLUMNS} FROM meter_subscription_change
             WHERE account_id = ?1 ORDER BY id DESC LIMIT 1"
        ),
        params![account_id.value()],
        row_to_change,
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot read the subscription history: {e}")))
}

/// The identity the account's readings are attributed under: the current
/// identity of its newest `established` row, or `None` when no subscription
/// has ever been established. `changed` rows never establish: looking one
/// up through them would bless the intruding subscription.
pub fn established_identity_for_account(
    conn: &rusqlite::Connection,
    account_id: AccountId,
) -> Result<Option<String>, Error> {
    conn.query_row(
        "SELECT current_identity FROM meter_subscription_change
         WHERE account_id = ?1 AND kind = 'established' ORDER BY id DESC LIMIT 1",
        params![account_id.value()],
        |row| row.get(0),
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot read the established subscription: {e}")))
}

/// Every subscription-history row for one account, oldest first: the full
/// chain a calibration exclusion reads to mark the untrustworthy intervals.
pub fn changes_for_account(
    conn: &rusqlite::Connection,
    account_id: AccountId,
) -> Result<Vec<StoredSubscriptionChange>, Error> {
    let mut statement = conn
        .prepare(&format!(
            "SELECT {SELECT_COLUMNS} FROM meter_subscription_change
             WHERE account_id = ?1 ORDER BY id"
        ))
        .map_err(|e| Error::Store(format!("cannot read the subscription history: {e}")))?;
    statement
        .query_map(params![account_id.value()], row_to_change)
        .map_err(|e| Error::Store(format!("cannot read the subscription history: {e}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| Error::Store(format!("cannot read the subscription history: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::MonotonicDuration;
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::store::meter_attempt::{DueReason, NewMeterAttempt, start_meter_attempt};
    use crate::store::sample_run::{Trigger, start_sample_run};
    use crate::store::sampling_policy_snapshot::{ResolvedSamplingPolicy, resolve_policy_snapshot};
    use crate::store::test_schema::open_migrated;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-subscription-identity-{}-{suffix}",
                std::process::id()
            ));
            std::fs::create_dir(&path).expect("scratch dir must be creatable");
            Self(path)
        }

        fn database_path(&self) -> PathBuf {
            self.0.join("subscription.db")
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct Fixture {
        _scratch: ScratchDir,
        database_path: PathBuf,
        account_id: AccountId,
        attempt_id: MeterAttemptRowId,
    }

    fn policy() -> PragmaPolicy {
        PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(500),
        }
    }

    fn fixture() -> Fixture {
        let scratch = ScratchDir::new();
        let database_path = scratch.database_path();
        let conn = open_migrated(&database_path, &policy());
        let account_id = crate::store::account::observe_account(
            &conn,
            "anthropic",
            "work",
            UtcTimestamp::from_unix_nanos(2_000),
        )
        .unwrap();
        let run_id = start_sample_run(
            &conn,
            Trigger::Manual,
            UtcTimestamp::from_unix_nanos(2_000),
            "fixture",
        )
        .unwrap();
        let policy_snapshot_id = resolve_policy_snapshot(
            &conn,
            account_id,
            UtcTimestamp::from_unix_nanos(2_000),
            &ResolvedSamplingPolicy {
                ordinary_cadence: MonotonicDuration::from_seconds(300),
                freshness_horizon: MonotonicDuration::from_seconds(900),
                reset_edge_policy: "lead-120s".into(),
                retry_backoff_policy: "exponential-3".into(),
                command_budget: MonotonicDuration::from_seconds(30),
                policy_algorithm_version: "v1".into(),
            },
        )
        .unwrap();
        let attempt_id = start_meter_attempt(
            &conn,
            &NewMeterAttempt {
                run_id,
                account_id,
                provider: "anthropic".into(),
                request_started_at: UtcTimestamp::from_unix_nanos(3_000),
                credential_context_id: Some("ctx".into()),
                policy_snapshot_id,
                due_at: UtcTimestamp::from_unix_nanos(2_500),
                due_reason: DueReason::ForcedOrManual,
                due_basis: None,
                provider_contract_id: "contract-v1".into(),
                meter_semantics_id: "semantics-v1".into(),
            },
        )
        .unwrap();
        drop(conn);
        Fixture {
            _scratch: scratch,
            database_path,
            account_id,
            attempt_id,
        }
    }

    fn reopen(database_path: &std::path::Path) -> rusqlite::Connection {
        open(database_path, AccessMode::ReadWrite, &policy()).unwrap()
    }

    #[test]
    fn establishment_round_trips_and_becomes_the_attribution_identity() {
        let fixture = fixture();
        let conn = reopen(&fixture.database_path);
        assert_eq!(
            established_identity_for_account(&conn, fixture.account_id).unwrap(),
            None
        );

        insert_change(
            &conn,
            &NewSubscriptionChange {
                account_id: fixture.account_id,
                kind: SubscriptionChangeKind::Established,
                previous_identity: None,
                current_identity: "anthropic:max:tier".to_string(),
                detecting_attempt_id: fixture.attempt_id,
                previous_observation_id: None,
                detected_at: UtcTimestamp::from_unix_nanos(3_000),
            },
        )
        .unwrap();

        assert_eq!(
            established_identity_for_account(&conn, fixture.account_id).unwrap(),
            Some("anthropic:max:tier".to_string())
        );
        let latest = latest_for_account(&conn, fixture.account_id)
            .unwrap()
            .expect("the establishment is the latest row");
        assert_eq!(latest.kind, SubscriptionChangeKind::Established);
        assert_eq!(latest.previous_identity, None);
    }

    #[test]
    fn a_change_never_establishes_its_own_identity() {
        let fixture = fixture();
        let conn = reopen(&fixture.database_path);
        insert_change(
            &conn,
            &NewSubscriptionChange {
                account_id: fixture.account_id,
                kind: SubscriptionChangeKind::Established,
                previous_identity: None,
                current_identity: "anthropic:max:tier".to_string(),
                detecting_attempt_id: fixture.attempt_id,
                previous_observation_id: None,
                detected_at: UtcTimestamp::from_unix_nanos(3_000),
            },
        )
        .unwrap();
        insert_change(
            &conn,
            &NewSubscriptionChange {
                account_id: fixture.account_id,
                kind: SubscriptionChangeKind::Changed,
                previous_identity: Some("anthropic:max:tier".to_string()),
                current_identity: "anthropic:pro:tier".to_string(),
                detecting_attempt_id: fixture.attempt_id,
                previous_observation_id: None,
                detected_at: UtcTimestamp::from_unix_nanos(4_000),
            },
        )
        .unwrap();

        // The intruding subscription is recorded but never established.
        assert_eq!(
            established_identity_for_account(&conn, fixture.account_id).unwrap(),
            Some("anthropic:max:tier".to_string())
        );
        let history = changes_for_account(&conn, fixture.account_id).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].kind, SubscriptionChangeKind::Changed);
    }

    #[test]
    fn the_table_check_refuses_an_establishment_naming_a_previous_identity() {
        let fixture = fixture();
        let conn = reopen(&fixture.database_path);
        let refused = insert_change(
            &conn,
            &NewSubscriptionChange {
                account_id: fixture.account_id,
                kind: SubscriptionChangeKind::Established,
                previous_identity: Some("anthropic:max:tier".to_string()),
                current_identity: "anthropic:pro:tier".to_string(),
                detecting_attempt_id: fixture.attempt_id,
                previous_observation_id: None,
                detected_at: UtcTimestamp::from_unix_nanos(3_000),
            },
        );
        assert!(
            refused.is_err(),
            "an establishment with a previous identity must fail at the CHECK"
        );
    }
}
