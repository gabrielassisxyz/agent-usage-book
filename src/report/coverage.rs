//! Assembly of the coverage report from the ledger's sampling evidence.
//!
//! The store owns the SQL; this module reads the evidence through it, hands
//! each account's records to the coverage engine, and wraps the engine's
//! output in the report model together with the two facts the engine does not
//! carry: which failure classes the interval's terminal attempts fell into,
//! and whether the configured floors are met. Nothing here computes a
//! coverage number: the engine's output is taken whole, including its
//! refusals.
//!
//! May not depend on:
//! - presentation
//! - the HTTP transport or any provider adapter

use rusqlite::Connection;

use crate::config::CoverageFloor;
use crate::coverage::{self, CoverageInputs};
use crate::domain::attempt::AttemptOutcome;
use crate::domain::provenance::{EvidenceId, QuerySemantics, WitnessId};
use crate::domain::time::UtcTimestamp;
use crate::error::Error;
use crate::logging::LogicalName;
use crate::report::models::{
    CoverageAccount, CoverageBreach, CoverageBreachDimension, CoverageErrorClassification,
    CoverageReport, CoverageReset, CoverageThreshold, IngestionGeneration, LedgerGeneration,
    ReportMetadata,
};
use crate::report::provenance::{ProvenanceNode, ValueArithmetic};
use crate::store::{
    account, ingestion_generation, ledger_generation, meter_attempt, meter_evidence, sample_run,
    sampling_policy_snapshot,
};

pub use crate::store::account::AccountIdentity;

/// The four classes PLAN.md section 15 distinguishes in measurement coverage:
/// authentication outage, rate limiting, provider outage, and parser or
/// API-schema breakage. One group per detail line, so the report names what
/// happened instead of printing a bare percentage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CoverageFailureGroup {
    Authentication,
    RateLimited,
    ProviderUnreachable,
    ResponseUnusable,
}

impl CoverageFailureGroup {
    /// The group of one terminal outcome, or `None` for a success.
    ///
    /// The match is exhaustive with no wildcard arm: a new `FailureClass`
    /// variant fails compilation here until it is placed into one of the four
    /// groups, which is what keeps the detail vocabulary and the failure
    /// taxonomy from drifting apart.
    pub fn of(outcome: &AttemptOutcome) -> Option<Self> {
        match outcome {
            AttemptOutcome::Success => None,
            AttemptOutcome::AuthRequired => Some(Self::Authentication),
            AttemptOutcome::Unreachable(class) => match class {
                crate::domain::failure::FailureClass::RateLimited { .. } => Some(Self::RateLimited),
                crate::domain::failure::FailureClass::MalformedBody
                | crate::domain::failure::FailureClass::MissingRequiredField
                | crate::domain::failure::FailureClass::SchemaDrift
                | crate::domain::failure::FailureClass::SubscriptionChanged => {
                    Some(Self::ResponseUnusable)
                }
                crate::domain::failure::FailureClass::DnsFailure
                | crate::domain::failure::FailureClass::ConnectTimeout
                | crate::domain::failure::FailureClass::ReadTimeout
                | crate::domain::failure::FailureClass::TotalBudgetExpired
                | crate::domain::failure::FailureClass::HttpStatus(_) => {
                    Some(Self::ProviderUnreachable)
                }
            },
        }
    }

    /// The stable JSON key of this group.
    pub fn key(self) -> &'static str {
        match self {
            Self::Authentication => "authentication",
            Self::RateLimited => "rate_limited",
            Self::ProviderUnreachable => "provider_unreachable",
            Self::ResponseUnusable => "response_unusable",
        }
    }

    /// The verb phrase the detail block renders as "{n} attempt(s) {phrase}".
    pub fn phrase(self) -> &'static str {
        match self {
            Self::Authentication => "required authentication",
            Self::RateLimited => "were rate limited",
            Self::ProviderUnreachable => "hit an unreachable provider",
            Self::ResponseUnusable => "returned an unusable response",
        }
    }
}

/// Terminal failures of one account's interval, grouped by
/// [`CoverageFailureGroup`]. Each count is exactly what the attempt table
/// recorded; the account's engine report carries the coverage numbers the
/// tally explains, which is why the counts are not individually qualified
/// values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CoverageFailureTally {
    pub authentication: u64,
    pub rate_limited: u64,
    pub provider_unreachable: u64,
    pub response_unusable: u64,
}

impl CoverageFailureTally {
    /// The non-zero counts, largest first, as the detail block renders them.
    pub fn nonzero(self) -> Vec<(CoverageFailureGroup, u64)> {
        let counts = [
            (CoverageFailureGroup::Authentication, self.authentication),
            (CoverageFailureGroup::RateLimited, self.rate_limited),
            (
                CoverageFailureGroup::ProviderUnreachable,
                self.provider_unreachable,
            ),
            (
                CoverageFailureGroup::ResponseUnusable,
                self.response_unusable,
            ),
        ];
        let mut groups: Vec<(CoverageFailureGroup, u64)> =
            counts.into_iter().filter(|(_, count)| *count > 0).collect();
        groups.sort_by_key(|(group, count)| (u64::MAX - *count, *group as u8));
        groups
    }
}

fn tally(outcomes: impl Iterator<Item = AttemptOutcome>) -> CoverageFailureTally {
    let mut tally = CoverageFailureTally::default();
    for outcome in outcomes {
        match CoverageFailureGroup::of(&outcome) {
            None => {}
            Some(CoverageFailureGroup::Authentication) => tally.authentication += 1,
            Some(CoverageFailureGroup::RateLimited) => tally.rate_limited += 1,
            Some(CoverageFailureGroup::ProviderUnreachable) => tally.provider_unreachable += 1,
            Some(CoverageFailureGroup::ResponseUnusable) => tally.response_unusable += 1,
        }
    }
    tally
}

/// What the command line selected, applied before the engine runs: one
/// account by logical name, or every account with a severe interval.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoverageSelector {
    pub account: Option<String>,
    pub severe_only: bool,
}

/// The configured floors the verdict judges each account against.
#[derive(Debug, Clone, Copy)]
pub struct CoverageFloors {
    pub attempt: CoverageFloor,
    pub measurement: CoverageFloor,
}

/// Builds the coverage report: every recorded account in the ledger (or the
/// one the selector named) over `[since, until)`, its engine report, its
/// failure tally, and the threshold verdict over exactly the accounts the
/// report shows.
///
/// An account the selector names but the ledger has never sampled is
/// insufficient evidence, not an empty report: coverage cannot say anything
/// about an account with no recorded history, and saying nothing would read
/// as healthy.
pub fn assemble(
    conn: &Connection,
    since: UtcTimestamp,
    until: UtcTimestamp,
    selector: &CoverageSelector,
    floors: CoverageFloors,
    now: UtcTimestamp,
    configured_accounts: &[AccountIdentity],
) -> Result<CoverageReport, Error> {
    let recorded = account::all_accounts(conn)?;
    let selected: Vec<_> = match &selector.account {
        Some(name) => {
            let matching: Vec<_> = recorded
                .iter()
                .filter(|recorded| recorded.logical_name() == name.as_str())
                .collect();
            if matching.is_empty() {
                return Err(Error::InsufficientEvidence(format!(
                    "no recorded sampling evidence for account {name} in the ledger"
                )));
            }
            matching
        }
        None => recorded.iter().collect(),
    };

    let timer_runs = sample_run::timer_run_times_between(conn, since, until)?;

    let mut accounts = Vec::<CoverageAccount>::new();
    for recorded in selected {
        let attempts = meter_attempt::attempts_with_outcomes_for_account_between(
            conn,
            recorded.id(),
            since,
            until,
        )?;
        let observations = meter_evidence::observation_times_for_account_between(
            conn,
            recorded.id(),
            since,
            until,
        )?;
        let resets =
            meter_evidence::reset_windows_for_account_between(conn, recorded.id(), since, until)?;
        let snapshots = sampling_policy_snapshot::snapshots_for_account(conn, recorded.id())?;
        let legacy_observations =
            crate::store::legacy_meter_import::legacy_observation_count_between(
                conn,
                recorded.id(),
                since,
                until,
            )?;

        // The credential-change flags are computed here, in sorted attempt
        // order (aub-x2je): an attempt whose context differs from its
        // predecessor's resets the authentication streak, the same rule the
        // scheduler applies. `None` on older rows never counts as a change.
        let mut last_context: Option<&str> = None;
        let mut attempt_records = Vec::with_capacity(attempts.len());
        for attempt in &attempts {
            let changed = match (attempt.credential_context_id.as_deref(), last_context) {
                (Some(current), Some(previous)) => current != previous,
                _ => false,
            };
            if attempt.credential_context_id.is_some() {
                last_context = attempt.credential_context_id.as_deref();
            }
            attempt_records.push(coverage::AttemptRecord {
                started_at: attempt.started_at,
                result: attempt
                    .terminal
                    .as_ref()
                    .map(|terminal| coverage::AttemptResultRecord {
                        finished_at: terminal.finished_at,
                        retry_after: terminal.retry_after,
                        is_auth_required: matches!(terminal.outcome, AttemptOutcome::AuthRequired),
                    }),
                credential_changed: changed,
            });
        }
        let inputs = CoverageInputs {
            interval_start: since,
            interval_end: until,
            policy_snapshots: snapshots
                .iter()
                .map(|snapshot| coverage::PolicySnapshot {
                    effective_at: snapshot.effective_at(),
                    ordinary_cadence: snapshot.policy().ordinary_cadence,
                    retry_backoff_policy: snapshot.policy().retry_backoff_policy.clone(),
                })
                .collect(),
            attempts: attempt_records,
            observations: observations
                .iter()
                .map(|at| coverage::ObservationRecord { at: *at })
                .collect(),
            resets: resets
                .iter()
                .map(|reset| coverage::ResetRecord { at: reset.at })
                .collect(),
            timer_runs: timer_runs
                .iter()
                .map(|at| coverage::TimerRunRecord { at: *at })
                .collect(),
        };
        let engine = coverage::compute(&inputs);

        if selector.severe_only && !engine.severe {
            continue;
        }

        let failures = tally(
            attempts
                .iter()
                .filter_map(|attempt| attempt.terminal.as_ref())
                .map(|terminal| terminal.outcome),
        );

        // The provider error classifications the interval's failed attempts
        // stored, counted per classification with the decoded classification
        // part of each stored value. Largest count first, so the detail block
        // leads with the failure that dominated the interval; ties break on
        // the classification's own order, which keeps the report stable.
        let mut classification_counts: std::collections::BTreeMap<String, u64> =
            std::collections::BTreeMap::new();
        for attempt in attempts
            .iter()
            .filter_map(|attempt| attempt.terminal.as_ref())
        {
            if matches!(
                attempt.outcome,
                AttemptOutcome::AuthRequired | AttemptOutcome::Unreachable(_)
            ) {
                // A NULL is the older rows' shape and reads as `unclassified`,
                // so the past stays visible without being rewritten.
                let stored = attempt
                    .error_classification
                    .as_deref()
                    .unwrap_or(meter_attempt::error_classification_column::UNCLASSIFIED);
                let classification =
                    meter_attempt::error_classification_column::classification_of(stored);
                *classification_counts
                    .entry(classification.to_owned())
                    .or_insert(0) += 1;
            }
        }
        let mut error_classifications: Vec<CoverageErrorClassification> = classification_counts
            .into_iter()
            .map(|(classification, count)| CoverageErrorClassification {
                classification,
                count,
            })
            .collect();
        error_classifications.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| a.classification.cmp(&b.classification))
        });

        // The resets that actually fell inside a no-attempt gap, each with the
        // window length the detail block names. A reset outside every gap is
        // not a lost peak and is deliberately left out.
        let resets_in_gaps = engine
            .reset_spanning_gaps
            .iter()
            .filter_map(|gap| {
                resets
                    .iter()
                    .filter(|reset| gap.spans(reset.at))
                    .map(|reset| CoverageReset {
                        at: reset.at,
                        window_length: reset.nominal_duration,
                    })
                    .min_by_key(|reset| reset.at.unix_nanos())
            })
            .collect::<Vec<_>>();

        let node = ProvenanceNode::new(
            [] as [EvidenceId; 0],
            [] as [WitnessId; 0],
            QuerySemantics::new(
                "coverage",
                format!("{}..{}", since.unix_nanos(), until.unix_nanos()),
            ),
            1,
            engine.attempted_opportunities + engine.successful_observations,
            ValueArithmetic::Count,
        );

        let is_configured = configured_accounts
            .iter()
            .any(|configured| configured == &recorded.identity());

        accounts.push(CoverageAccount {
            name: LogicalName::new(recorded.logical_name().to_string()),
            engine,
            failures,
            error_classifications,
            resets_in_gaps,
            legacy_evidence_present: legacy_observations > 0,
            configured: is_configured,
            provenance: node,
        });
    }

    let threshold = verdict(&accounts, floors);
    let metadata = ReportMetadata::new(
        now,
        now,
        LedgerGeneration::new(ledger_generation::current(conn)?.value()),
        Some(IngestionGeneration::new(
            ingestion_generation::current(conn)?.value(),
        )),
    );
    Ok(CoverageReport::new(
        metadata,
        since,
        until,
        selector.severe_only,
        threshold,
        accounts,
    ))
}

/// The verdict over exactly the accounts the report shows: every account
/// whose attempt or measurement coverage sits below its floor, in account
/// order. A coverage the engine refused to compute (no policy snapshot in
/// force, a zero denominator, no terminal attempt) is never judged against a
/// floor: a missing number is reported as missing, and inventing a breach or
/// a pass for it would both be guesses.
///
/// Accounts absent from the resolved configuration are excluded from the
/// threshold verdict: only accounts the sampler was told to observe can
/// breach coverage.
fn verdict(accounts: &[CoverageAccount], floors: CoverageFloors) -> CoverageThreshold {
    let mut breaches = Vec::<CoverageBreach>::new();
    for account in accounts {
        if !account.configured {
            continue;
        }
        if let Some(coverage) = account.engine.attempt_coverage
            && coverage.as_f64() < floors.attempt.get()
        {
            breaches.push(CoverageBreach {
                account: account.name.clone(),
                dimension: CoverageBreachDimension::Attempt,
                coverage,
                floor: floors.attempt,
            });
        }
        if let Some(coverage) = account.engine.measurement_coverage
            && coverage.as_f64() < floors.measurement.get()
        {
            breaches.push(CoverageBreach {
                account: account.name.clone(),
                dimension: CoverageBreachDimension::Measurement,
                coverage,
                floor: floors.measurement,
            });
        }
    }
    CoverageThreshold {
        attempt_floor: floors.attempt,
        measurement_floor: floors.measurement,
        met: breaches.is_empty(),
        breaches,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_failure_class_lands_in_exactly_one_group() {
        use crate::domain::failure::{FailureClass, HttpStatusClass};
        use crate::domain::time::MonotonicDuration;

        let classes = [
            AttemptOutcome::Success,
            AttemptOutcome::AuthRequired,
            AttemptOutcome::Unreachable(FailureClass::DnsFailure),
            AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
            AttemptOutcome::Unreachable(FailureClass::ReadTimeout),
            AttemptOutcome::Unreachable(FailureClass::TotalBudgetExpired),
            AttemptOutcome::Unreachable(FailureClass::HttpStatus(HttpStatusClass::ClientError)),
            AttemptOutcome::Unreachable(FailureClass::HttpStatus(HttpStatusClass::ServerError)),
            AttemptOutcome::Unreachable(FailureClass::RateLimited {
                retry_after: Some(MonotonicDuration::from_seconds(60)),
            }),
            AttemptOutcome::Unreachable(FailureClass::MalformedBody),
            AttemptOutcome::Unreachable(FailureClass::MissingRequiredField),
        ];
        // Every outcome maps or is a success; the tally never loses a failure.
        let counted: u64 = classes
            .iter()
            .map(|outcome| u64::from(CoverageFailureGroup::of(outcome).is_some()))
            .sum();
        assert_eq!(
            counted,
            (classes.len() - 1) as u64,
            "every non-success outcome must land in exactly one group"
        );
    }

    #[test]
    fn the_tally_counts_each_group_once_per_failure() {
        let tally = tally(
            [
                Some(AttemptOutcome::AuthRequired),
                Some(AttemptOutcome::AuthRequired),
                Some(AttemptOutcome::Unreachable(
                    crate::domain::failure::FailureClass::RateLimited { retry_after: None },
                )),
                Some(AttemptOutcome::Success),
                Some(AttemptOutcome::Unreachable(
                    crate::domain::failure::FailureClass::MalformedBody,
                )),
            ]
            .into_iter()
            .flatten(),
        );
        assert_eq!(
            tally,
            CoverageFailureTally {
                authentication: 2,
                rate_limited: 1,
                provider_unreachable: 0,
                response_unusable: 1,
            }
        );
        let rendered = tally.nonzero();
        assert_eq!(
            rendered,
            vec![
                (CoverageFailureGroup::Authentication, 2),
                (CoverageFailureGroup::RateLimited, 1),
                (CoverageFailureGroup::ResponseUnusable, 1),
            ],
            "non-zero groups are ordered by count, largest first"
        );
    }

    use crate::config::CoverageFloor;
    use crate::coverage::CoverageFraction;
    use crate::domain::ids::{AdapterVersion, MeterSemanticsId, ProviderContractId};
    use crate::domain::time::{MeasurementBasis, MonotonicDuration};
    use crate::store::connection::{self, PragmaPolicy};
    use crate::store::meter_attempt::{
        self, DueReason, NewMeterAttempt, NewMeterAttemptResult, record_pre_classification_result,
    };
    use crate::store::meter_evidence::{self, NewMeterObservation, NewMeterResponseEvidence};

    use crate::store::sample_run::{self, Trigger};
    use crate::store::sampling_policy_snapshot::{self, ResolvedSamplingPolicy};
    use test_support::StateDir;

    fn open_test_ledger(state: &StateDir) -> rusqlite::Connection {
        let path = state.path().join(connection::LEDGER_DATABASE_FILE);
        let policy = PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1000),
        };

        crate::store::test_schema::open_migrated(&path, &policy)
    }

    fn test_policy(cadence_secs: u64) -> ResolvedSamplingPolicy {
        ResolvedSamplingPolicy {
            ordinary_cadence: MonotonicDuration::from_seconds(cadence_secs),
            freshness_horizon: MonotonicDuration::from_seconds(cadence_secs * 3),
            reset_edge_policy: "lead-0s".to_string(),
            retry_backoff_policy: "none".to_string(),
            command_budget: MonotonicDuration::from_seconds(30),
            policy_algorithm_version: "v1".to_string(),
        }
    }

    fn test_floors() -> CoverageFloors {
        CoverageFloors {
            attempt: CoverageFloor::new(0.95).unwrap(),
            measurement: CoverageFloor::new(0.90).unwrap(),
        }
    }

    fn seed_test_attempt_with_observation(
        conn: &rusqlite::Connection,
        run: sample_run::SampleRunId,
        account: account::AccountId,
        snapshot: sampling_policy_snapshot::SamplingPolicySnapshotId,
        started: UtcTimestamp,
    ) {
        let row = meter_attempt::start_meter_attempt(
            conn,
            &NewMeterAttempt {
                run_id: run,
                account_id: account,
                provider: "anthropic".into(),
                request_started_at: started,
                credential_context_id: None,
                policy_snapshot_id: snapshot,
                due_at: started,
                due_reason: DueReason::OrdinaryCadence,
                due_basis: None,
                provider_contract_id: "test-contract".into(),
                meter_semantics_id: "test-semantics".into(),
            },
        )
        .expect("attempt must insert");
        let finished = UtcTimestamp::from_unix_nanos(started.unix_nanos() + 1_000_000_000);
        meter_attempt::record_meter_attempt_result(
            conn,
            &NewMeterAttemptResult {
                attempt_id: row,
                completed_at: finished,
                elapsed: MonotonicDuration::from_millis(100),
                outcome: AttemptOutcome::Success,
                sanitized_error_classification: None,
                retry_index: None,
                clock_anomaly: false,
            },
        )
        .expect("attempt result must insert");
        let evidence = meter_evidence::insert_response_evidence(
            conn,
            &NewMeterResponseEvidence {
                attempt_id: row,
                response_classification: "200".into(),
                received_at: finished,
                provider_observed_at_original: None,
                evidence_capsule: "{}".into(),
                capsule_schema_version: "capsule-v1".into(),
                sanitizer_version: "san-v1".into(),
                capture_truncated: false,
            },
        )
        .expect("evidence must insert");
        meter_evidence::insert_observation(
            conn,
            &NewMeterObservation {
                attempt_id: row,
                evidence_id: evidence,
                account_id: account,
                provider: "anthropic".into(),
                provider_observed_at: Some(finished),
                received_at: finished,
                measurement_basis: MeasurementBasis::ProviderObserved,
                observed_plan: None,
                observed_tier: None,
                adapter_version: AdapterVersion::new("adapter-v1"),
                provider_contract_id: ProviderContractId::new("test-contract"),
                meter_semantics_id: MeterSemanticsId::new("test-semantics"),
                normalized_fingerprint: format!("fp-{}", started.unix_nanos()),
            },
        )
        .expect("observation must insert");
    }

    #[test]
    fn unconfigured_account_with_observations_and_snapshot_produces_no_breach() {
        let state = StateDir::new();
        let conn = open_test_ledger(&state);
        let since = UtcTimestamp::from_unix_nanos(100_000_000_000);
        let until = UtcTimestamp::from_unix_nanos(200_000_000_000);
        let run = sample_run::start_sample_run(&conn, Trigger::Timer, since, "test-run")
            .expect("sample run must insert");
        let account = account::observe_account(&conn, "anthropic", "retired", since)
            .expect("account must insert");
        let snapshot = sampling_policy_snapshot::resolve_policy_snapshot(
            &conn,
            account,
            since,
            &test_policy(10),
        )
        .expect("policy snapshot must insert");
        seed_test_attempt_with_observation(
            &conn,
            run,
            account,
            snapshot,
            UtcTimestamp::from_unix_nanos(110_000_000_000),
        );

        let selector = CoverageSelector::default();
        let configured = [AccountIdentity::new("anthropic", "active")];
        let report = assemble(
            &conn,
            since,
            until,
            &selector,
            test_floors(),
            until,
            &configured,
        )
        .expect("report must assemble");

        assert_eq!(report.accounts.len(), 1);
        let acct = &report.accounts[0];
        assert_eq!(acct.name.as_str(), "retired");
        assert!(
            !acct.configured,
            "account absent from config must have configured=false"
        );
        assert!(
            report.threshold.met,
            "an unconfigured account must not breach coverage threshold"
        );
        assert!(
            report.threshold.breaches.is_empty(),
            "an unconfigured account must produce no breach"
        );
    }

    #[test]
    fn configured_account_below_floor_still_produces_breach() {
        let state = StateDir::new();
        let conn = open_test_ledger(&state);
        let since = UtcTimestamp::from_unix_nanos(100_000_000_000);
        let until = UtcTimestamp::from_unix_nanos(200_000_000_000);
        let run = sample_run::start_sample_run(&conn, Trigger::Timer, since, "test-run")
            .expect("sample run must insert");
        let account = account::observe_account(&conn, "anthropic", "failing", since)
            .expect("account must insert");
        let snapshot = sampling_policy_snapshot::resolve_policy_snapshot(
            &conn,
            account,
            since,
            &test_policy(10),
        )
        .expect("policy snapshot must insert");
        seed_test_attempt_with_observation(
            &conn,
            run,
            account,
            snapshot,
            UtcTimestamp::from_unix_nanos(110_000_000_000),
        );

        let selector = CoverageSelector::default();
        let configured = [AccountIdentity::new("anthropic", "failing")];
        let report = assemble(
            &conn,
            since,
            until,
            &selector,
            test_floors(),
            until,
            &configured,
        )
        .expect("report must assemble");

        assert_eq!(report.accounts.len(), 1);
        let acct = &report.accounts[0];
        assert_eq!(acct.name.as_str(), "failing");
        assert!(
            acct.configured,
            "account in config must have configured=true"
        );
        assert!(
            !report.threshold.met,
            "a configured account below floor must breach coverage threshold"
        );
        assert_eq!(report.threshold.breaches.len(), 1);
        assert_eq!(report.threshold.breaches[0].account.as_str(), "failing");
        assert_eq!(
            report.threshold.breaches[0].dimension,
            CoverageBreachDimension::Attempt
        );
    }

    #[test]
    fn configured_account_with_zero_attempts_and_covering_snapshot_produces_breach() {
        let state = StateDir::new();
        let conn = open_test_ledger(&state);
        let since = UtcTimestamp::from_unix_nanos(100_000_000_000);
        let until = UtcTimestamp::from_unix_nanos(200_000_000_000);
        let account = account::observe_account(&conn, "anthropic", "zero-attempts", since)
            .expect("account must insert");
        let _snapshot = sampling_policy_snapshot::resolve_policy_snapshot(
            &conn,
            account,
            since,
            &test_policy(10),
        )
        .expect("policy snapshot must insert");

        let selector = CoverageSelector::default();
        let configured = [AccountIdentity::new("anthropic", "zero-attempts")];
        let report = assemble(
            &conn,
            since,
            until,
            &selector,
            test_floors(),
            until,
            &configured,
        )
        .expect("report must assemble");

        assert_eq!(report.accounts.len(), 1);
        let acct = &report.accounts[0];
        assert_eq!(acct.name.as_str(), "zero-attempts");
        assert!(
            acct.configured,
            "account in config must have configured=true"
        );
        assert_eq!(acct.engine.attempted_opportunities, 0);
        assert_eq!(acct.engine.attempt_coverage, CoverageFraction::new(0, 10));
        assert!(
            !report.threshold.met,
            "a configured account with zero attempts must breach coverage threshold"
        );
        assert_eq!(report.threshold.breaches.len(), 1);
        assert_eq!(
            report.threshold.breaches[0].account.as_str(),
            "zero-attempts"
        );
        assert_eq!(
            report.threshold.breaches[0].dimension,
            CoverageBreachDimension::Attempt
        );
    }

    #[test]
    fn identity_match_requires_matching_provider_and_logical_name() {
        let state = StateDir::new();
        let conn = open_test_ledger(&state);
        let since = UtcTimestamp::from_unix_nanos(100_000_000_000);
        let until = UtcTimestamp::from_unix_nanos(200_000_000_000);
        let account = account::observe_account(&conn, "other-provider", "same-name", since)
            .expect("account must insert");
        let _snapshot = sampling_policy_snapshot::resolve_policy_snapshot(
            &conn,
            account,
            since,
            &test_policy(10),
        )
        .expect("policy snapshot must insert");

        let configured = [AccountIdentity::new("anthropic", "same-name")];
        let report = assemble(
            &conn,
            since,
            until,
            &CoverageSelector::default(),
            test_floors(),
            until,
            &configured,
        )
        .expect("report must assemble");

        assert_eq!(report.accounts.len(), 1);
        let acct = &report.accounts[0];
        assert_eq!(acct.name.as_str(), "same-name");
        assert!(
            !acct.configured,
            "account with mismatched provider must not match configured identity"
        );
        assert!(
            report.threshold.met,
            "unconfigured account must not trigger breach"
        );
    }

    /// The detail block's findings name the provider error classification
    /// with its count: the classification part of each stored value is what
    /// groups, never the message riding beside it, and a NULL reads as
    /// `unclassified`. The planted negative: two attempts carrying the same
    /// classification but different messages must count as one finding, so
    /// a reader that grouped on the whole stored value would print two.
    #[test]
    fn the_report_groups_failures_by_their_classification_not_their_message() {
        let state = StateDir::new();
        let conn = open_test_ledger(&state);
        let since = UtcTimestamp::from_unix_nanos(100_000_000_000);
        let until = UtcTimestamp::from_unix_nanos(200_000_000_000);
        let run = sample_run::start_sample_run(&conn, Trigger::Timer, since, "test-run")
            .expect("sample run must insert");
        let account = account::observe_account(&conn, "anthropic", "throttled", since)
            .expect("account must insert");
        let snapshot = sampling_policy_snapshot::resolve_policy_snapshot(
            &conn,
            account,
            since,
            &test_policy(10),
        )
        .expect("policy snapshot must insert");

        let outcomes = [
            (
                AttemptOutcome::AuthRequired,
                Some("authentication_error: Invalid authentication token provided.".to_owned()),
            ),
            (
                AttemptOutcome::AuthRequired,
                Some("authentication_error: Token rejected.".to_owned()),
            ),
            (
                AttemptOutcome::Unreachable(crate::domain::failure::FailureClass::RateLimited {
                    retry_after: None,
                }),
                Some("rate_limit_error".to_owned()),
            ),
            (
                AttemptOutcome::Unreachable(crate::domain::failure::FailureClass::ReadTimeout),
                None,
            ),
        ];
        for (index, (outcome, classification)) in outcomes.iter().enumerate() {
            let started = UtcTimestamp::from_unix_nanos(110_000_000_000 + index as i64 * 1_000);
            let row = meter_attempt::start_meter_attempt(
                &conn,
                &NewMeterAttempt {
                    run_id: run,
                    account_id: account,
                    provider: "anthropic".into(),
                    request_started_at: started,
                    credential_context_id: None,
                    policy_snapshot_id: snapshot,
                    due_at: started,
                    due_reason: DueReason::OrdinaryCadence,
                    due_basis: None,
                    provider_contract_id: "test-contract".into(),
                    meter_semantics_id: "test-semantics".into(),
                },
            )
            .expect("attempt must insert");
            let record = NewMeterAttemptResult {
                attempt_id: row,
                completed_at: UtcTimestamp::from_unix_nanos(started.unix_nanos() + 1_000),
                elapsed: MonotonicDuration::from_millis(100),
                outcome: *outcome,
                sanitized_error_classification: classification.clone(),
                retry_index: None,
                clock_anomaly: false,
            };
            // The classification-less row is the pre-column shape the ledger
            // already holds: production writers refuse it now, so it goes
            // through the test-only legacy writer that fixture exists for.
            if classification.is_none() && !matches!(outcome, AttemptOutcome::Success) {
                record_pre_classification_result(&conn, &record)
                    .expect("the legacy-shape row must insert");
            } else {
                meter_attempt::record_meter_attempt_result(&conn, &record)
                    .expect("attempt result must insert");
            }
        }

        let selector = CoverageSelector::default();
        let configured = [AccountIdentity::new("anthropic", "throttled")];
        let report = assemble(
            &conn,
            since,
            until,
            &selector,
            test_floors(),
            until,
            &configured,
        )
        .expect("report must assemble");
        assert_eq!(report.accounts.len(), 1);
        let findings = &report.accounts[0].error_classifications;
        assert_eq!(
            *findings,
            vec![
                CoverageErrorClassification {
                    classification: "authentication_error".to_owned(),
                    count: 2,
                },
                CoverageErrorClassification {
                    classification: "rate_limit_error".to_owned(),
                    count: 1,
                },
                CoverageErrorClassification {
                    classification: "unclassified".to_owned(),
                    count: 1,
                },
            ],
            "two same-classification attempts with different messages count once, \n                  largest count first, NULL reading as unclassified"
        );
    }
}
