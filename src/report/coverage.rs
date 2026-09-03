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
    CoverageAccount, CoverageBreach, CoverageBreachDimension, CoverageReport, CoverageReset,
    CoverageThreshold, IngestionGeneration, LedgerGeneration, ReportMetadata,
};
use crate::report::provenance::{ProvenanceNode, ValueArithmetic};
use crate::store::{
    account, ingestion_generation, ledger_generation, meter_attempt, meter_evidence, sample_run,
    sampling_policy_snapshot,
};

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
                | crate::domain::failure::FailureClass::MissingRequiredField => {
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

        let inputs = CoverageInputs {
            interval_start: since,
            interval_end: until,
            policy_snapshots: snapshots
                .iter()
                .map(|snapshot| coverage::PolicySnapshot {
                    effective_at: snapshot.effective_at(),
                    ordinary_cadence: snapshot.policy().ordinary_cadence,
                })
                .collect(),
            attempts: attempts
                .iter()
                .map(|attempt| coverage::AttemptRecord {
                    started_at: attempt.started_at,
                    result: attempt.terminal.as_ref().map(|terminal| {
                        coverage::AttemptResultRecord {
                            finished_at: terminal.finished_at,
                            retry_after: terminal.retry_after,
                        }
                    }),
                })
                .collect(),
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

        accounts.push(CoverageAccount {
            name: LogicalName::new(recorded.logical_name().to_string()),
            engine,
            failures,
            resets_in_gaps,
            legacy_evidence_present: legacy_observations > 0,
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
fn verdict(accounts: &[CoverageAccount], floors: CoverageFloors) -> CoverageThreshold {
    let mut breaches = Vec::<CoverageBreach>::new();
    for account in accounts {
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
}
