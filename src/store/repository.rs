//! The database-owning repository seam and the meter transaction boundaries.
//!
//! Table modules own individual statements. This module owns how those statements
//! compose: an attempt start commits before the caller performs external work, while
//! the terminal result, response evidence, interpretation, windows, and initial
//! preference selector commit together. Repository reads open a read-only connection
//! for one short snapshot and return typed values rather than exposing SQLite rows.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::domain::attempt::{AttemptId, AttemptResult, AttemptStarted};
use crate::domain::ids::{AdapterVersion, MeterSemanticsId, ProviderContractId};
use crate::domain::time::{MeasurementBasis, MonotonicDuration, UtcTimestamp};
use crate::domain::window::MeterWindow;
use crate::error::Error;
use crate::projection::{self, Publication};

use super::account::{self, AccountId};
use super::connection::{self, AccessMode, PragmaPolicy};
use super::ledger_generation;
use super::meter_attempt::{self, MeterAttemptRowId, NewMeterAttempt, NewMeterAttemptResult};
use super::meter_evidence::{
    self, EvidenceRowId, NewMeterObservation, NewMeterResponseEvidence, NewMeterWindow,
    ObservationRowId,
};
use super::sample_run::{self, SampleRunId, Trigger};
use super::sampling_lease::{self, AccountName, LeaseHolder, LeaseOutcome};
use super::sampling_policy_snapshot::{ResolvedSamplingPolicy, SamplingPolicySnapshotId};

/// Opens repository operations against one ledger database under one pragma policy.
#[derive(Debug, Clone)]
pub struct Repository {
    database_path: PathBuf,
    policy: PragmaPolicy,
}

impl Repository {
    pub fn new(database_path: impl Into<PathBuf>, policy: PragmaPolicy) -> Self {
        Self {
            database_path: database_path.into(),
            policy,
        }
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// The projection file this repository publishes: the state directory the
    /// ledger lives in, named by the one constant the projection module owns.
    /// Every projection-relevant commit below publishes there, so a caller
    /// never derives the path itself.
    pub fn projection_path(&self) -> PathBuf {
        self.database_path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(projection::PROJECTION_FILE_NAME)
    }

    /// Records the account identity if it has never been sampled, and advances
    /// its last-sight timestamp if it has. The identity is the configured
    /// `(provider_key, logical_name)` pair, the same pair the sampling lease
    /// keys on, so the first-ever sample of a configured account creates the
    /// row the attempt needs before the attempt is started.
    pub fn ensure_account(
        &self,
        provider_key: &str,
        logical_name: &str,
        observed_at: UtcTimestamp,
    ) -> Result<AccountId, Error> {
        let conn = self.open_write()?;
        account::observe_account(&conn, provider_key, logical_name, observed_at)
    }

    /// Reads one account's evidence snapshot for the due decision in one
    /// read-only connection, so the decision is a function of one moment of
    /// the database: whether the account has a row at all, its latest attempt
    /// with the terminal result it ever reached, and the reset instants the
    /// newest observation carries.
    pub fn due_evidence_snapshot(
        &self,
        account_id: AccountId,
    ) -> Result<DueEvidenceSnapshot, Error> {
        self.with_read_connection(|conn| {
            let latest_attempt = meter_attempt::latest_attempt_for_account(conn, account_id)?;
            let latest_result = match &latest_attempt {
                Some(stored) => meter_attempt::result_by_attempt_id(conn, stored.row_id)?,
                None => None,
            };
            let known_resets =
                match meter_evidence::newest_observation_for_account(conn, account_id)? {
                    Some(observation) => {
                        meter_evidence::windows_by_observation(conn, observation.row_id)?
                            .into_iter()
                            .map(|window| window.resets_at)
                            .collect::<BTreeSet<_>>()
                            .into_iter()
                            .collect()
                    }
                    None => Vec::new(),
                };
            let latest_attempt_started = match latest_attempt {
                Some(stored) => {
                    let id = stored.row_id.as_attempt_id()?;
                    Some(AttemptStarted::new(id, stored.request_started_at))
                }
                None => None,
            };
            let latest_result_typed = match latest_result {
                Some(stored) => {
                    let id = stored.attempt_id.as_attempt_id()?;
                    Some(AttemptResult::new(
                        id,
                        stored.completed_at,
                        stored.elapsed,
                        stored.outcome,
                    ))
                }
                None => None,
            };
            Ok(DueEvidenceSnapshot {
                latest_attempt: latest_attempt_started,
                latest_result: latest_result_typed,
                known_resets,
            })
        })
    }

    /// Records the policy in force for `account_id` as of `effective_at`,
    /// reusing the most recent snapshot when the resolved policy is unchanged.
    pub fn resolve_policy_snapshot(
        &self,
        account_id: AccountId,
        effective_at: UtcTimestamp,
        policy: &ResolvedSamplingPolicy,
    ) -> Result<SamplingPolicySnapshotId, Error> {
        let conn = self.open_write()?;
        super::sampling_policy_snapshot::resolve_policy_snapshot(
            &conn,
            account_id,
            effective_at,
            policy,
        )
    }

    /// Opens the sampling batch: one durable `sample_run` row before any
    /// account in the batch is sampled, so every attempt in the batch names
    /// the invocation they had in common.
    pub fn start_sample_run(
        &self,
        trigger: Trigger,
        started_at: UtcTimestamp,
        configuration_fingerprint: &str,
    ) -> Result<SampleRunId, Error> {
        let conn = self.open_write()?;
        sample_run::start_sample_run(&conn, trigger, started_at, configuration_fingerprint)
    }

    /// Acquires the per-account sampling lease through one short immediate
    /// transaction, timed by the caller's clock. The outcome names the live
    /// lease when another holder has it, so a skipped account is reported
    /// with who holds it, never as silence.
    pub fn acquire_sampling_lease(
        &self,
        account: &AccountName,
        holder: &LeaseHolder,
        ttl: MonotonicDuration,
        clock: &dyn crate::domain::time::Clock,
    ) -> Result<LeaseOutcome, Error> {
        let mut conn = self.open_write()?;
        sampling_lease::acquire(&mut conn, account, holder, ttl, clock)
    }

    /// Releases the per-account sampling lease if this invocation still holds
    /// it. A lease that expired and was taken over by another holder is not
    /// released; the boolean reports whether a row was removed.
    pub fn release_sampling_lease(
        &self,
        account: &AccountName,
        holder: &LeaseHolder,
    ) -> Result<bool, Error> {
        let conn = self.open_write()?;
        sampling_lease::release(&conn, account, holder)
    }

    fn open_write(&self) -> Result<Connection, Error> {
        connection::open(&self.database_path, AccessMode::ReadWrite, &self.policy)
    }

    /// Publishes the projection over one fresh read-only snapshot, so the
    /// generation and the state the file describes are read as one moment.
    fn publish_projection(&self) -> Publication {
        self.publish_with(projection::publish)
    }

    fn with_read_connection<T>(
        &self,
        read: impl FnOnce(&Connection) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let conn = connection::open(&self.database_path, AccessMode::ReadOnly, &self.policy)?;
        read(&conn)
    }

    /// Commits an attempt start in its own transaction, together with the
    /// ledger generation advance (a started attempt is a freshness input, so
    /// the start is projection-relevant durable meter state), and publishes
    /// the projection once the commit is durable. Returns only after both are
    /// done, so a caller that performs external work afterwards does so
    /// against a database and a projection that agree.
    pub fn start_meter_attempt(&self, attempt: &NewMeterAttempt) -> Result<AttemptStarted, Error> {
        let mut conn = self.open_write()?;
        // IMMEDIATE, not deferred: a write transaction that starts as a read
        // and upgrades at its first insert fails instantly with SQLITE_BUSY
        // when another writer committed since its snapshot (WAL's
        // BUSY_SNAPSHOT), and the busy timeout never runs. The meter's start
        // must wait for the slot like any writer, so it takes the slot
        // upfront.
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| {
                Error::Store(format!(
                    "cannot open the attempt-start transaction: {error}"
                ))
            })?;
        let row_id = meter_attempt::start_meter_attempt(&tx, attempt)?;
        ledger_generation::advance(&tx)?;
        tx.commit()
            .map_err(|error| Error::Store(format!("cannot commit the attempt start: {error}")))?;
        drop(conn);
        // A deferred publication here is healed by the terminal publication
        // that always follows the same attempt, or by any later one; the
        // attempt start itself is already durable either way.
        self.publish_projection();
        Ok(AttemptStarted::new(
            row_id.as_attempt_id()?,
            attempt.request_started_at,
        ))
    }

    /// Commits the attempt start, closes the write operation, and only then invokes
    /// `after_commit`. Provider calls use this boundary so no database transaction can
    /// accidentally span network work.
    pub fn start_meter_attempt_then<T>(
        &self,
        attempt: &NewMeterAttempt,
        after_commit: impl FnOnce(AttemptStarted) -> T,
    ) -> Result<T, Error> {
        let started = self.start_meter_attempt(attempt)?;
        Ok(after_commit(started))
    }

    /// Commits one complete terminal fact in one short write transaction, then
    /// publishes the projection from the committed state. The commit precedes
    /// publication in every path through this method, so a crash between them
    /// can only leave the projection older, never ahead.
    pub fn commit_terminal_bundle(
        &self,
        bundle: &TerminalMeterBundle,
    ) -> Result<TerminalBundleCommit, Error> {
        self.commit_terminal_bundle_publishing_with(bundle, |reader, target| {
            projection::publish(reader, target)
        })
    }

    /// [`Self::commit_terminal_bundle`] with the publication step injected, the
    /// crash-injection seam for the kill-between-commit-and-publication proof
    /// (`__projection-crash-hook`): the bundle commit is fully durable before
    /// `publish` is invoked, and `publish` is the next thing this method does.
    pub(crate) fn commit_terminal_bundle_publishing_with(
        &self,
        bundle: &TerminalMeterBundle,
        publish: impl FnOnce(&Connection, &Path) -> Publication,
    ) -> Result<TerminalBundleCommit, Error> {
        let ids = self.commit_terminal_bundle_only(bundle)?;
        let publication = self.publish_with(publish);
        Ok(TerminalBundleCommit { ids, publication })
    }

    fn commit_terminal_bundle_only(
        &self,
        bundle: &TerminalMeterBundle,
    ) -> Result<TerminalBundleIds, Error> {
        let mut conn = self.open_write()?;
        commit_terminal_bundle_on_connection(&mut conn, bundle, || Ok(()))
    }

    /// Commits one terminal result alone, in one short write transaction: the
    /// failure-only batch path, where an attempt ended in authentication or
    /// transport failure and has no response evidence to record. The ledger
    /// generation advances inside the same transaction, and the projection is
    /// published from the committed state, because a batch that produced only
    /// failures changes what the status line must say just as a successful one
    /// does.
    pub fn commit_terminal_result(
        &self,
        result: &NewMeterAttemptResult,
    ) -> Result<Publication, Error> {
        let mut conn = self.open_write()?;
        // IMMEDIATE for the same reason as every meter write below: a deferred
        // start would race another writer's commit at its first insert and
        // fail without waiting, which would spool evidence SQLite could have
        // taken the slot for.
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| {
                Error::Store(format!(
                    "cannot open the terminal-result transaction: {error}"
                ))
            })?;
        meter_attempt::record_meter_attempt_result(&tx, result)?;
        ledger_generation::advance(&tx)?;
        tx.commit()
            .map_err(|error| Error::Store(format!("cannot commit the terminal result: {error}")))?;
        drop(conn);
        Ok(self.publish_projection())
    }

    /// Publishes through an injected publisher over one fresh read snapshot.
    fn publish_with(&self, publish: impl FnOnce(&Connection, &Path) -> Publication) -> Publication {
        match connection::open(&self.database_path, AccessMode::ReadOnly, &self.policy) {
            Ok(reader) => publish(&reader, &self.projection_path()),
            Err(error) => Publication::Deferred {
                reason: error.to_string(),
            },
        }
    }

    /// Reads one durable attempt start through a read-only, single-statement snapshot.
    pub fn attempt_started(&self, attempt_id: AttemptId) -> Result<Option<AttemptStarted>, Error> {
        let row_id = meter_attempt_row_id(attempt_id)?;
        self.with_read_connection(|conn| {
            meter_attempt::attempt_by_row_id(conn, row_id).map(|attempt| {
                attempt.map(|stored| AttemptStarted::new(attempt_id, stored.request_started_at))
            })
        })
    }

    /// Reads one terminal result through a read-only, single-statement snapshot.
    pub fn attempt_result(&self, attempt_id: AttemptId) -> Result<Option<AttemptResult>, Error> {
        let row_id = meter_attempt_row_id(attempt_id)?;
        self.with_read_connection(|conn| {
            meter_attempt::result_by_attempt_id(conn, row_id).map(|result| {
                result.map(|stored| {
                    AttemptResult::new(
                        attempt_id,
                        stored.completed_at,
                        stored.elapsed,
                        stored.outcome,
                    )
                })
            })
        })
    }
}

fn meter_attempt_row_id(attempt_id: AttemptId) -> Result<MeterAttemptRowId, Error> {
    i64::try_from(attempt_id.value())
        .map(MeterAttemptRowId::new)
        .map_err(|_| {
            Error::Store(format!(
                "attempt id {} exceeds SQLite INTEGER",
                attempt_id.value()
            ))
        })
}

/// The interpretation fields whose generated evidence and observation identifiers do
/// not exist until the terminal transaction runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMeterInterpretation {
    pub account_id: AccountId,
    pub provider: String,
    pub provider_observed_at: Option<UtcTimestamp>,
    pub received_at: UtcTimestamp,
    pub measurement_basis: MeasurementBasis,
    pub observed_plan: Option<String>,
    pub observed_tier: Option<String>,
    pub adapter_version: AdapterVersion,
    pub provider_contract_id: ProviderContractId,
    pub meter_semantics_id: MeterSemanticsId,
    pub normalized_fingerprint: String,
}

/// One complete terminal meter fact, expressed without generated child row IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalMeterBundle {
    result: NewMeterAttemptResult,
    evidence: NewMeterResponseEvidence,
    interpretation: NewMeterInterpretation,
    windows: Vec<MeterWindow>,
}

impl TerminalMeterBundle {
    pub fn new(
        result: NewMeterAttemptResult,
        evidence: NewMeterResponseEvidence,
        interpretation: NewMeterInterpretation,
        windows: Vec<MeterWindow>,
    ) -> Result<Self, Error> {
        if result.attempt_id != evidence.attempt_id {
            return Err(Error::Store(format!(
                "terminal bundle attempt mismatch: result {} and evidence {}",
                result.attempt_id.value(),
                evidence.attempt_id.value(),
            )));
        }
        Ok(Self {
            result,
            evidence,
            interpretation,
            windows,
        })
    }

    pub fn result(&self) -> &NewMeterAttemptResult {
        &self.result
    }

    pub fn evidence(&self) -> &NewMeterResponseEvidence {
        &self.evidence
    }

    pub fn interpretation(&self) -> &NewMeterInterpretation {
        &self.interpretation
    }

    pub fn windows(&self) -> &[MeterWindow] {
        &self.windows
    }
}

/// The generated identities of one committed terminal bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalBundleIds {
    pub evidence_id: EvidenceRowId,
    pub observation_id: ObservationRowId,
}

/// One committed terminal bundle together with the publication that followed
/// its commit. The commit precedes the publication unconditionally; the
/// publication outcome is carried so a caller can see a deferral instead of
/// reading silence as a refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalBundleCommit {
    pub ids: TerminalBundleIds,
    pub publication: Publication,
}

/// One account's evidence for the due decision, read as one snapshot: whether
/// the account has ever been observed, its latest attempt with the terminal
/// result it ever reached, and the reset instants the newest observation
/// carries. `None` attempt means the account has never been sampled, which is
/// itself a due answer (an empty history is due on ordinary cadence).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueEvidenceSnapshot {
    pub latest_attempt: Option<AttemptStarted>,
    pub latest_result: Option<AttemptResult>,
    pub known_resets: Vec<UtcTimestamp>,
}

pub(crate) fn commit_terminal_bundle_on_connection(
    conn: &mut Connection,
    bundle: &TerminalMeterBundle,
    before_commit: impl FnOnce() -> Result<(), Error>,
) -> Result<TerminalBundleIds, Error> {
    // IMMEDIATE, not deferred: this is a write transaction, and a deferred
    // start would take a read snapshot and then fail instantly with
    // SQLITE_BUSY at the first insert whenever another writer committed in
    // between (WAL's BUSY_SNAPSHOT path), spooling meter evidence that only
    // needed to wait for the slot. Taking the write slot upfront makes the
    // bounded busy timeout the real wait it is meant to be.
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| {
            Error::Store(format!(
                "cannot open the terminal-bundle transaction: {error}"
            ))
        })?;

    meter_attempt::record_meter_attempt_result(&tx, bundle.result())?;
    let evidence_id = meter_evidence::insert_response_evidence(&tx, bundle.evidence())?;
    let interpretation = bundle.interpretation();
    let observation_id = meter_evidence::insert_observation(
        &tx,
        &NewMeterObservation {
            attempt_id: bundle.result().attempt_id,
            evidence_id,
            account_id: interpretation.account_id,
            provider: interpretation.provider.clone(),
            provider_observed_at: interpretation.provider_observed_at,
            received_at: interpretation.received_at,
            measurement_basis: interpretation.measurement_basis,
            observed_plan: interpretation.observed_plan.clone(),
            observed_tier: interpretation.observed_tier.clone(),
            adapter_version: interpretation.adapter_version.clone(),
            provider_contract_id: interpretation.provider_contract_id.clone(),
            meter_semantics_id: interpretation.meter_semantics_id.clone(),
            normalized_fingerprint: interpretation.normalized_fingerprint.clone(),
        },
    )?;
    for window in bundle.windows() {
        meter_evidence::insert_window(
            &tx,
            &NewMeterWindow {
                observation_id,
                semantic_key: window.semantic_key().clone(),
                scope: window.scope().clone(),
                quota_used: window.quota_used(),
                reported_resolution: window.reported_resolution(),
                quantization: window.quantization(),
                resets_at: window.resets_at(),
                nominal_duration: window.nominal_duration(),
            },
        )?;
    }

    // The terminal fact is projection-relevant durable meter state, so its
    // generation advance belongs to this same transaction (PLAN.md section
    // 11.6): a rollback of the bundle rolls the generation back, and a commit
    // advances both together. Every caller of this boundary, live sampling and
    // spool recovery alike, gets the bump here rather than by remembering it.
    ledger_generation::advance(&tx)?;

    before_commit()?;
    tx.commit().map_err(|error| {
        Error::Store(format!(
            "cannot commit the terminal-bundle transaction: {error}"
        ))
    })?;
    Ok(TerminalBundleIds {
        evidence_id,
        observation_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attempt::AttemptOutcome;
    use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
    use crate::domain::time::{FakeClock, MonotonicDuration};
    use crate::domain::window::{
        NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
        WindowSemanticKey,
    };
    use crate::meter::adapter::HttpTransport;
    use crate::meter::transport::{CommandBudget, HttpRequest, HttpResponse, RequestTimeoutConfig};
    use crate::store::meter_attempt::DueReason;
    use crate::store::sample_run::{Trigger, start_sample_run};
    use crate::store::sampling_policy_snapshot::{ResolvedSamplingPolicy, resolve_policy_snapshot};
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-store-repository-test-{}-{suffix}",
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

    struct Fixture {
        _scratch: ScratchDir,
        repository: Repository,
        attempt: NewMeterAttempt,
        account_id: AccountId,
    }

    fn policy() -> PragmaPolicy {
        PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(500),
        }
    }

    fn fixture() -> Fixture {
        let scratch = ScratchDir::new();
        let database_path = scratch.path().join("repository.db");
        let mut conn = connection::open(&database_path, AccessMode::ReadWrite, &policy()).unwrap();
        crate::store::migrate::run_migrations(
            &mut conn,
            &crate::store::migrations::registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(1_000)),
        )
        .unwrap();
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
        drop(conn);

        Fixture {
            _scratch: scratch,
            repository: Repository::new(database_path, policy()),
            attempt: NewMeterAttempt {
                run_id,
                account_id,
                provider: "anthropic".into(),
                request_started_at: UtcTimestamp::from_unix_nanos(3_000),
                credential_context_id: Some("credential-context-v1".into()),
                policy_snapshot_id,
                due_at: UtcTimestamp::from_unix_nanos(2_500),
                due_reason: DueReason::ForcedOrManual,
                due_basis: None,
                provider_contract_id: "contract-v1".into(),
                meter_semantics_id: "semantics-v1".into(),
            },
            account_id,
        }
    }

    fn terminal_bundle(attempt_id: AttemptId, account_id: AccountId) -> TerminalMeterBundle {
        let attempt_row_id = meter_attempt_row_id(attempt_id).unwrap();
        let fraction = |value| QuotaFractionPpm::new(value).unwrap();
        let window = |key: &str, used: i32| {
            MeterWindow::new(
                WindowSemanticKey::new(key),
                WindowScope::AccountWide,
                QuotaUsed::new(fraction(used)),
                ReportedResolution::new(fraction(10_000)).unwrap(),
                QuantizationSemantics::RoundedToNearest,
                UtcTimestamp::from_unix_nanos(9_000),
                NominalWindowDuration::from_nanos(18_000_000_000_000),
            )
        };
        TerminalMeterBundle::new(
            NewMeterAttemptResult {
                attempt_id: attempt_row_id,
                completed_at: UtcTimestamp::from_unix_nanos(4_000),
                elapsed: MonotonicDuration::from_nanos(1_000),
                outcome: AttemptOutcome::Success,
                sanitized_error_classification: None,
                retry_index: None,
                clock_anomaly: false,
            },
            NewMeterResponseEvidence {
                attempt_id: attempt_row_id,
                response_classification: "success".into(),
                received_at: UtcTimestamp::from_unix_nanos(4_000),
                provider_observed_at_original: Some("1970-01-01T00:00:00Z".into()),
                evidence_capsule: "{\"five_hour\":\"25.0\"}".into(),
                capsule_schema_version: "capsule-v1".into(),
                sanitizer_version: "sanitizer-v1".into(),
                capture_truncated: false,
            },
            NewMeterInterpretation {
                account_id,
                provider: "anthropic".into(),
                provider_observed_at: Some(UtcTimestamp::from_unix_nanos(3_900)),
                received_at: UtcTimestamp::from_unix_nanos(4_000),
                measurement_basis: MeasurementBasis::ProviderObserved,
                observed_plan: Some("max".into()),
                observed_tier: None,
                adapter_version: AdapterVersion::new("adapter-v1"),
                provider_contract_id: ProviderContractId::new("contract-v1"),
                meter_semantics_id: MeterSemanticsId::new("semantics-v1"),
                normalized_fingerprint: "fingerprint-v1".into(),
            },
            vec![window("five_hour", 250_000), window("seven_day", 400_000)],
        )
        .unwrap()
    }

    fn table_count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    #[test]
    fn a_failure_before_terminal_commit_rolls_back_the_whole_bundle_but_not_its_start() {
        let fixture = fixture();
        let started = fixture
            .repository
            .start_meter_attempt(&fixture.attempt)
            .unwrap();
        let bundle = terminal_bundle(started.attempt_id(), fixture.account_id);
        let mut conn = fixture.repository.open_write().unwrap();

        let error = commit_terminal_bundle_on_connection(&mut conn, &bundle, || {
            Err(Error::Store("injected before terminal commit".into()))
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("injected before terminal commit")
        );
        assert_eq!(table_count(&conn, "meter_attempt"), 1);
        for table in [
            "meter_attempt_result",
            "meter_response_evidence",
            "meter_observation",
            "meter_window",
            "meter_observation_preference",
        ] {
            assert_eq!(
                table_count(&conn, table),
                0,
                "partial row remained in {table}"
            );
        }
        assert_eq!(
            fixture
                .repository
                .attempt_started(started.attempt_id())
                .unwrap(),
            Some(started)
        );
    }

    struct PanicsIfTransactionIsOpen {
        database_path: PathBuf,
        policy: PragmaPolicy,
    }

    impl HttpTransport for PanicsIfTransactionIsOpen {
        fn send(
            &self,
            _request: &HttpRequest,
            _budget: &CommandBudget,
            _clock: &impl crate::domain::time::Clock,
        ) -> Result<HttpResponse, crate::domain::failure::FailureClass> {
            let mut probe =
                connection::open(&self.database_path, AccessMode::ReadWrite, &self.policy)
                    .unwrap_or_else(|error| {
                        panic!("network invoked while a transaction is open: {error}")
                    });
            let transaction = probe
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap_or_else(|error| {
                    panic!("network invoked while a transaction is open: {error}")
                });
            transaction.rollback().unwrap();
            Ok(HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: b"{}".to_vec(),
            })
        }
    }

    #[test]
    fn the_external_action_runs_after_the_attempt_start_transaction_commits() {
        let fixture = fixture();
        let transport = PanicsIfTransactionIsOpen {
            database_path: fixture.repository.database_path().to_owned(),
            policy: policy(),
        };
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(3_000));
        let budget = CommandBudget::new(MonotonicDuration::from_seconds(1), &clock);
        let request = HttpRequest::get(
            "http://unused.invalid",
            RequestTimeoutConfig::new(
                MonotonicDuration::from_millis(10),
                MonotonicDuration::from_millis(10),
                None,
            ),
        );

        let response = fixture
            .repository
            .start_meter_attempt_then(&fixture.attempt, |started| {
                assert_eq!(
                    fixture
                        .repository
                        .attempt_started(started.attempt_id())
                        .unwrap(),
                    Some(started),
                    "the attempt start must already be visible to a read-only connection"
                );
                transport.send(&request, &budget, &clock)
            })
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), 200);
    }

    #[test]
    fn repository_reads_open_read_only_connections_and_return_domain_values() {
        let fixture = fixture();
        let started = fixture
            .repository
            .start_meter_attempt(&fixture.attempt)
            .unwrap();
        fixture
            .repository
            .with_read_connection(|conn| {
                let error = conn
                    .execute("CREATE TABLE forbidden_write (id INTEGER)", [])
                    .unwrap_err();
                assert!(
                    error.to_string().contains("readonly"),
                    "read path accepted a write or reported the wrong reason: {error}"
                );
                Ok(())
            })
            .unwrap();
        let read: Option<AttemptStarted> = fixture
            .repository
            .attempt_started(started.attempt_id())
            .unwrap();
        assert_eq!(read, Some(started));
    }

    #[test]
    fn a_long_analytical_snapshot_and_terminal_bundle_write_do_not_block_each_other() {
        let fixture = fixture();
        let started = fixture
            .repository
            .start_meter_attempt(&fixture.attempt)
            .unwrap();
        let bundle = terminal_bundle(started.attempt_id(), fixture.account_id);
        let mut reader = connection::open(
            fixture.repository.database_path(),
            AccessMode::ReadOnly,
            &policy(),
        )
        .unwrap();
        let snapshot = reader.transaction().unwrap();
        assert_eq!(table_count(&snapshot, "meter_attempt_result"), 0);

        let writer_repository = fixture.repository.clone();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let started_at = Instant::now();
                let result = writer_repository.commit_terminal_bundle(&bundle);
                finished_tx.send((started_at.elapsed(), result)).unwrap();
            });

            // The production write path commits and then publishes, so the
            // window covers the busy bound plus the publication step's local
            // file I/O. What this bound refuses is lock waiting: a write that
            // blocked behind the reader would fail with busy after the busy
            // timeout or hang here, and neither fits this window.
            let allowed = Duration::from_nanos(policy().busy_timeout.as_nanos() + 2_000_000_000);
            let (elapsed, result) = finished_rx
                .recv_timeout(allowed)
                .expect("terminal write blocked behind a WAL reader beyond the busy bound");
            result.unwrap();
            assert!(
                elapsed <= allowed,
                "terminal write took {}ms, bound is {}ms",
                elapsed.as_millis(),
                allowed.as_millis()
            );
            assert_eq!(
                table_count(&snapshot, "meter_attempt_result"),
                0,
                "the long reader must keep its original complete snapshot"
            );
        });
        snapshot.commit().unwrap();

        let stored = fixture
            .repository
            .attempt_result(started.attempt_id())
            .unwrap()
            .expect("the complete terminal bundle must be visible after the snapshot closes");
        assert_eq!(stored.outcome(), AttemptOutcome::Success);
    }

    /// The due-evidence snapshot reads the account's latest attempt, the
    /// terminal result that attempt reached, and the reset instants the newest
    /// observation carries, and reports an empty snapshot for an account that
    /// has never been sampled.
    #[test]
    fn due_evidence_snapshot_reads_history_and_resets_as_one_snapshot() {
        let fixture = fixture();

        // An account never sampled: no attempt, no result, no resets.
        let fresh = fixture
            .repository
            .due_evidence_snapshot(fixture.account_id)
            .unwrap();
        assert_eq!(fresh.latest_attempt, None);
        assert_eq!(fresh.latest_result, None);
        assert_eq!(fresh.known_resets, Vec::new());

        // After one sampled-and-committed attempt, the snapshot carries the
        // attempt, its success result, and the windows' reset instants.
        let started = fixture
            .repository
            .start_meter_attempt(&fixture.attempt)
            .unwrap();
        let bundle = terminal_bundle(started.attempt_id(), fixture.account_id);
        fixture.repository.commit_terminal_bundle(&bundle).unwrap();

        let sampled = fixture
            .repository
            .due_evidence_snapshot(fixture.account_id)
            .unwrap();
        assert_eq!(sampled.latest_attempt, Some(started));
        let result = sampled
            .latest_result
            .expect("the committed bundle's result must be visible to the snapshot");
        assert_eq!(result.attempt_id(), started.attempt_id());
        assert_eq!(result.outcome(), AttemptOutcome::Success);
        assert_eq!(
            sampled.known_resets,
            vec![UtcTimestamp::from_unix_nanos(9_000)],
            "both fixture windows reset at the same instant, deduplicated to one"
        );
    }
}
