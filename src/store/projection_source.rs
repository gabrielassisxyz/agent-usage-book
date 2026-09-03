//! The durable-state reads the status projection is built from (`aub-me5.5`).
//!
//! The projection is a one-way derived file, so every field it carries is read
//! back out of the ledger tables here, on one connection inside one read
//! snapshot, and nothing else is supplied: which fields those are is the
//! design's list (PLAN.md section 16.1), asserted over a produced file by the
//! schema test in `src/projection.rs`. The ledger generation is read on the
//! same connection by the publisher, so the generation the file records names
//! exactly the state these rows return.
//!
//! "The last successful observation" is defined here, once: the newest attempt
//! of the account whose terminal outcome is success, and for that attempt's
//! evidence the interpretation the preference selector names current under the
//! attempt's own semantics version. A corrected adapter supersedes an older
//! interpretation through that selector, so the projection always describes
//! the interpretation a database reader would itself be given.
//!
//! May not depend on:
//! - HTTP or provider semantics
//! - presentation

use rusqlite::Connection;

use crate::domain::ids::MeterSemanticsId;
use crate::error::Error;

use super::account::{Account, AccountId, all_accounts};
use super::meter_attempt::{self, StoredMeterAttempt, StoredMeterAttemptResult};
use super::meter_evidence::{self, EvidenceRowId, StoredMeterObservation, StoredMeterWindow};

/// The last successful observation of one account and the windows it reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuccessfulObservation {
    pub observation: StoredMeterObservation,
    pub windows: Vec<StoredMeterWindow>,
}

/// The newest attempt of an account and its terminal result, when one exists.
/// A `None` result is the started-with-no-terminal-outcome fact the design
/// names: collector interruption, never an absent attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestAttemptState {
    pub attempt: StoredMeterAttempt,
    pub result: Option<StoredMeterAttemptResult>,
}

/// Everything the projection carries about one account, read as one snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountMeterState {
    pub account: Account,
    pub last_success: Option<SuccessfulObservation>,
    pub latest_attempt: Option<LatestAttemptState>,
}

/// Reads every account's meter state in account-identity order.
///
/// The caller owns the read transaction that defines the snapshot: every read
/// happens on `conn`, so the ledger generation the publisher reads alongside
/// these rows describes exactly the state they return.
pub fn account_meter_states(conn: &Connection) -> Result<Vec<AccountMeterState>, Error> {
    let mut states = Vec::new();
    for account in all_accounts(conn)? {
        let account_id = account.id();
        let latest_attempt = latest_attempt_with_result(conn, account_id)?;
        let last_success = last_successful_observation(conn, account_id)?;
        states.push(AccountMeterState {
            account,
            last_success,
            latest_attempt,
        });
    }
    Ok(states)
}

fn latest_attempt_with_result(
    conn: &Connection,
    account_id: AccountId,
) -> Result<Option<LatestAttemptState>, Error> {
    let Some(attempt) = meter_attempt::latest_attempt_for_account(conn, account_id)? else {
        return Ok(None);
    };
    let result = meter_attempt::result_by_attempt_id(conn, attempt.row_id)?;
    Ok(Some(LatestAttemptState { attempt, result }))
}

fn last_successful_observation(
    conn: &Connection,
    account_id: AccountId,
) -> Result<Option<SuccessfulObservation>, Error> {
    let Some(attempt) = meter_attempt::newest_successful_attempt_for_account(conn, account_id)?
    else {
        return Ok(None);
    };
    let Some(evidence_id) = meter_evidence::newest_evidence_for_attempt(conn, attempt.row_id)?
    else {
        return Ok(None);
    };
    let Some(observation) = current_observation(
        conn,
        evidence_id,
        &MeterSemanticsId::new(attempt.meter_semantics_id.clone()),
    )?
    else {
        return Ok(None);
    };
    let windows = meter_evidence::windows_by_observation(conn, observation.row_id)?;
    Ok(Some(SuccessfulObservation {
        observation,
        windows,
    }))
}

/// The interpretation the preference selector names current for one evidence
/// row under one semantics version. The write path always names one, so
/// absence is a durable anomaly; the projection reports no last-good
/// observation rather than guessing at a substitute interpretation.
fn current_observation(
    conn: &Connection,
    evidence_id: EvidenceRowId,
    semantics: &MeterSemanticsId,
) -> Result<Option<StoredMeterObservation>, Error> {
    let Some(row_id) = meter_evidence::current_observation_id(conn, evidence_id, semantics)? else {
        return Ok(None);
    };
    meter_evidence::observation_by_row_id(conn, row_id)
}

/// Test support shared by this module's tests and by the projection module's
/// mapping tests: a seeded scratch database with one account, one run and one
/// policy snapshot, plus the helpers to run attempts and terminal bundles
/// through the real write path. It lives in the store because seeding runs
/// migrations, and the status path's own module must never reference the
/// migration framework.
#[cfg(test)]
pub(crate) mod test_support {
    use rusqlite::Connection;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::domain::attempt::AttemptOutcome;
    use crate::domain::failure::FailureClass;
    use crate::domain::ids::AdapterVersion;
    use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
    use crate::domain::time::Clock as _;
    use crate::domain::time::{FakeClock, MeasurementBasis, MonotonicDuration, UtcTimestamp};
    use crate::domain::window::{
        MeterWindow, NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
        WindowSemanticKey,
    };
    use crate::store::account::{self, AccountId};
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::store::meter_attempt::{
        DueReason, MeterAttemptRowId, NewMeterAttempt, NewMeterAttemptResult,
        record_meter_attempt_result, start_meter_attempt,
    };
    use crate::store::meter_evidence::NewMeterResponseEvidence;
    use crate::store::migrate::run_migrations;
    use crate::store::repository::{
        NewMeterInterpretation, TerminalMeterBundle, commit_terminal_bundle_on_connection,
    };
    use crate::store::sample_run::{SampleRunId, Trigger, start_sample_run};
    use crate::store::sampling_policy_snapshot::{
        ResolvedSamplingPolicy, SamplingPolicySnapshotId, resolve_policy_snapshot,
    };

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    pub(crate) struct ScratchDir(PathBuf);

    impl ScratchDir {
        pub(crate) fn new(tag: &str) -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-store-projection-source-test-{tag}-{}-{suffix}",
                std::process::id()
            ));
            std::fs::create_dir(&path).expect("scratch dir must be creatable");
            Self(path)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) struct Fixture {
        pub(crate) _scratch: ScratchDir,
        pub(crate) conn: Connection,
        pub(crate) account_id: AccountId,
        pub(crate) run_id: SampleRunId,
        pub(crate) policy_snapshot_id: SamplingPolicySnapshotId,
        pub(crate) clock: FakeClock,
    }

    pub(crate) fn fixture_policy() -> PragmaPolicy {
        PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(2000),
        }
    }

    /// A migrated scratch database with one account, one run and one policy
    /// snapshot, at a clock advanced past the seeding writes.
    pub(crate) fn fixture(tag: &str) -> Fixture {
        let scratch = ScratchDir::new(tag);
        let mut conn = open(
            &scratch.path().join("source.db"),
            AccessMode::ReadWrite,
            &fixture_policy(),
        )
        .unwrap();
        run_migrations(
            &mut conn,
            &crate::store::migrations::registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(1_000)),
        )
        .unwrap();
        let account_id = account::observe_account(
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
            &resolved_policy(),
        )
        .unwrap();
        Fixture {
            _scratch: scratch,
            conn,
            account_id,
            run_id,
            policy_snapshot_id,
            clock: FakeClock::new(UtcTimestamp::from_unix_nanos(3_000)),
        }
    }

    pub(crate) fn resolved_policy() -> ResolvedSamplingPolicy {
        ResolvedSamplingPolicy {
            ordinary_cadence: MonotonicDuration::from_seconds(300),
            freshness_horizon: MonotonicDuration::from_seconds(900),
            reset_edge_policy: "fixture".into(),
            retry_backoff_policy: "fixture".into(),
            command_budget: MonotonicDuration::from_seconds(30),
            policy_algorithm_version: "v1".into(),
        }
    }

    impl Fixture {
        pub(crate) fn database_path(&self) -> PathBuf {
            self._scratch.path().join("source.db")
        }

        pub(crate) fn projection_path(&self) -> PathBuf {
            self._scratch
                .path()
                .join(crate::projection::PROJECTION_FILE_NAME)
        }

        /// Adds a deliberately large population in one fixture transaction.
        /// The benchmark is measuring read shape, not the cost of synchronously
        /// committing thousands of independent account observations.
        pub(crate) fn seed_additional_accounts(&mut self, count: usize) {
            let transaction = self.conn.transaction().unwrap();
            for index in 0..count {
                transaction
                    .execute(
                        "INSERT INTO account (logical_name, provider_key, first_observed_at, last_observed_at) \
                         VALUES (?1, ?2, ?3, ?3)",
                        rusqlite::params![
                            format!("large-{index:04}"),
                            "benchmark",
                            self.clock.now().unix_nanos(),
                        ],
                    )
                    .unwrap();
            }
            transaction.commit().unwrap();
        }

        pub(crate) fn start_attempt(&mut self) -> MeterAttemptRowId {
            self.clock.advance(MonotonicDuration::from_seconds(10));
            let started_at = self.clock.now();
            let attempt = NewMeterAttempt {
                run_id: self.run_id,
                account_id: self.account_id,
                provider: "anthropic".into(),
                request_started_at: started_at,
                credential_context_id: Some("credential-context-v1".into()),
                policy_snapshot_id: self.policy_snapshot_id,
                due_at: started_at,
                due_reason: DueReason::OrdinaryCadence,
                due_basis: None,
                provider_contract_id: "contract-v1".into(),
                meter_semantics_id: "semantics-v1".into(),
            };
            start_meter_attempt(&self.conn, &attempt).unwrap()
        }

        /// Commits a success bundle through the same repository boundary live
        /// sampling uses, so the read side under test is fed by the real write
        /// path, including the generation advance.
        pub(crate) fn commit_success_bundle(&mut self, attempt_id: MeterAttemptRowId) {
            let bundle = self.success_bundle(attempt_id);
            commit_terminal_bundle_on_connection(&mut self.conn, &bundle, || Ok(())).unwrap();
        }

        pub(crate) fn commit_failure(
            &mut self,
            attempt_id: MeterAttemptRowId,
            class: FailureClass,
        ) {
            self.clock.advance(MonotonicDuration::from_millis(500));
            let result = NewMeterAttemptResult {
                attempt_id,
                completed_at: self.clock.now(),
                elapsed: MonotonicDuration::from_nanos(1),
                outcome: AttemptOutcome::Unreachable(class),
                sanitized_error_classification: None,
                retry_index: None,
                clock_anomaly: false,
            };
            record_meter_attempt_result(&self.conn, &result).unwrap();
        }

        pub(crate) fn success_bundle(
            &mut self,
            attempt_id: MeterAttemptRowId,
        ) -> TerminalMeterBundle {
            self.clock.advance(MonotonicDuration::from_millis(500));
            let completed_at = self.clock.now();
            let window = MeterWindow::new(
                WindowSemanticKey::new("five_hour"),
                WindowScope::AccountWide,
                QuotaUsed::new(QuotaFractionPpm::new(250_000).unwrap()),
                ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap(),
                QuantizationSemantics::RoundedToNearest,
                completed_at,
                NominalWindowDuration::from_nanos(18_000_000_000_000),
            );
            let evidence = NewMeterResponseEvidence {
                attempt_id,
                response_classification: "success".into(),
                received_at: completed_at,
                provider_observed_at_original: Some("1970-01-01T00:00:04Z".into()),
                evidence_capsule: "{\"five_hour\":\"25.0\"}".into(),
                capsule_schema_version: "capsule-v1".into(),
                sanitizer_version: "sanitizer-v1".into(),
                capture_truncated: false,
            };
            let interpretation = NewMeterInterpretation {
                account_id: self.account_id,
                provider: "anthropic".into(),
                provider_observed_at: Some(completed_at),
                received_at: completed_at,
                measurement_basis: MeasurementBasis::ProviderObserved,
                observed_plan: Some("max".into()),
                observed_tier: None,
                adapter_version: AdapterVersion::new("adapter-v1"),
                provider_contract_id: crate::domain::ids::ProviderContractId::new("contract-v1"),
                meter_semantics_id: crate::domain::ids::MeterSemanticsId::new("semantics-v1"),
                normalized_fingerprint: "fingerprint-v1".into(),
            };
            TerminalMeterBundle::new(
                NewMeterAttemptResult {
                    attempt_id,
                    completed_at,
                    elapsed: MonotonicDuration::from_nanos(1),
                    outcome: AttemptOutcome::Success,
                    sanitized_error_classification: None,
                    retry_index: None,
                    clock_anomaly: false,
                },
                evidence,
                interpretation,
                vec![window],
            )
            .unwrap()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attempt::AttemptOutcome;
    use crate::domain::failure::FailureClass;
    use crate::domain::time::{MeasurementBasis, UtcTimestamp};
    use crate::store::meter_evidence::measurement_basis_sql;
    use crate::store::projection_source::test_support::fixture;

    #[test]
    fn an_account_with_no_attempts_reads_with_neither_success_nor_latest_attempt() {
        let fixture = fixture("empty");
        let states = account_meter_states(&fixture.conn).unwrap();
        assert_eq!(states.len(), 1);
        assert!(states[0].last_success.is_none());
        assert!(states[0].latest_attempt.is_none());
        assert_eq!(
            states[0].account.provider_key(),
            "anthropic",
            "the account identity the design lists must come from the account row"
        );
    }

    #[test]
    fn a_failure_only_history_reads_the_failed_attempt_as_the_latest_attempt() {
        let mut fixture = fixture("failure-only");
        let attempt = fixture.start_attempt();
        fixture.commit_failure(attempt, FailureClass::ConnectTimeout);

        let states = account_meter_states(&fixture.conn).unwrap();
        let latest = states[0].latest_attempt.as_ref().expect("attempt exists");
        assert_eq!(latest.attempt.row_id, attempt);
        assert_eq!(
            latest
                .result
                .as_ref()
                .expect("the failure is a terminal fact")
                .outcome,
            AttemptOutcome::Unreachable(FailureClass::ReadTimeout),
            "the failure class round-trips through the store's single spelling, \
             which names the transport-timeout class"
        );
        assert!(
            states[0].last_success.is_none(),
            "a failure-only history has no last successful observation"
        );
    }

    #[test]
    fn a_started_attempt_with_no_result_reads_as_a_result_less_latest_attempt() {
        let mut fixture = fixture("started-no-result");
        fixture.start_attempt();

        let states = account_meter_states(&fixture.conn).unwrap();
        let latest = states[0].latest_attempt.as_ref().expect("attempt exists");
        assert!(
            latest.result.is_none(),
            "a started attempt with no terminal result must read as exactly that, \
             which is the collector-interruption fact the design names"
        );
    }

    #[test]
    fn after_a_failure_the_last_success_is_still_the_older_success() {
        let mut fixture = fixture("failure-after-success");
        let first = fixture.start_attempt();
        fixture.commit_success_bundle(first);
        let second = fixture.start_attempt();
        fixture.commit_failure(second, FailureClass::ConnectTimeout);

        let states = account_meter_states(&fixture.conn).unwrap();
        let success = states[0].last_success.as_ref().expect("a success exists");
        assert_eq!(
            success.observation.attempt_id, first,
            "the last success must be the successful attempt, not the newer failure"
        );
        assert_eq!(
            states[0].latest_attempt.as_ref().unwrap().attempt.row_id,
            second,
            "the latest attempt must be the newer failed one"
        );
        assert_eq!(
            states[0]
                .latest_attempt
                .as_ref()
                .unwrap()
                .result
                .as_ref()
                .unwrap()
                .outcome,
            AttemptOutcome::Unreachable(FailureClass::ReadTimeout)
        );
    }

    #[test]
    fn a_newer_success_supersedes_the_older_one() {
        let mut fixture = fixture("newer-success");
        let first = fixture.start_attempt();
        fixture.commit_success_bundle(first);
        let second = fixture.start_attempt();
        fixture.commit_success_bundle(second);

        let states = account_meter_states(&fixture.conn).unwrap();
        let success = states[0].last_success.as_ref().unwrap();
        assert_eq!(success.observation.attempt_id, second);
        assert_eq!(success.windows.len(), 1);
    }

    #[test]
    fn the_last_success_carries_the_observation_timestamps_the_freshness_machine_needs() {
        let mut fixture = fixture("success-timestamps");
        let attempt = fixture.start_attempt();
        fixture.commit_success_bundle(attempt);

        let states = account_meter_states(&fixture.conn).unwrap();
        let success = states[0].last_success.as_ref().unwrap();
        assert_eq!(
            success.observation.received_at,
            UtcTimestamp::from_unix_nanos(10_500_003_000),
            "the received timestamp is the interpretation's own"
        );
        assert_eq!(
            success.observation.provider_observed_at,
            Some(UtcTimestamp::from_unix_nanos(10_500_003_000)),
            "the provider-observed timestamp travels beside it"
        );
        assert_eq!(
            measurement_basis_sql::as_sql(success.observation.measurement_basis),
            "provider_observed"
        );
        assert_eq!(
            success.windows.len(),
            1,
            "the windows travel with the observation"
        );
        let _ = MeasurementBasis::ProviderObserved;
    }
}
