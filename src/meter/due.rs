//! The due decision: whether the sampler owes this account an attempt right
//! now, and why (`aub-me5.3`, PLAN.md 14.1, 14.4, 14.5).
//!
//! May not depend on:
//! - SQLite (rule `03`): the decision is pure over injected inputs, and the
//!   store module that reads the attempt history feeds it.
//! - credential or configuration modules (rule `07`)
//! - presentation
//!
//! The scheduler ticks more often than the sampling interval and `aub` decides
//! what is due. That inversion is what makes reset-edge sampling possible
//! without a resident process: an account is due when its ordinary interval
//! expired, or when a known reset is approaching within the configured edge
//! lead and no sufficiently recent pre-reset sample exists, or when a
//! post-reset confirmation is owed. The reset timestamps come from the
//! previous observation, so the system learns where the edges are from the
//! evidence it already has.
//!
//! The rules, in evaluation order and documented once:
//!
//! 1. **Forced or manual** beats everything: an explicit request is due
//!    regardless of history.
//! 2. **Post-reset confirmation**: the most recent known reset has passed and
//!    no attempt started at or after it. The post-reset state has never been
//!    observed, which is a different fact from an interval that simply
//!    elapsed.
//! 3. **Reset edge**: the most recent known reset approaches within the edge
//!    lead and no pre-reset sample exists - no attempt's evidence is at least
//!    as fresh as the edge-lead instant before the reset, which is as close
//!    to the boundary as the policy ever wants evidence from. When no reset
//!    has passed yet, the freshness anchor falls back to ordinary recency:
//!    evidence within the ordinary cadence of now counts as the window's
//!    pre-reset sample.
//! 4. **Ordinary cadence**: the account's interval expired since the previous
//!    due instant. An account with no history at all is due.
//! 5. **Rate-limit postponement**: if the most recent result was rate limited
//!    with a `Retry-After`, the due instant is the refusal's instant plus the
//!    delay capped by the policy's ceiling, when that lies past the cadence
//!    boundary: the provider's instruction holds the account for every tick
//!    until the instant, not only for the remainder of the current cadence,
//!    and the interval after it resumes the ordinary grid from the
//!    postponement's end. A reset edge waited out is caught by the post-reset
//!    confirmation once the reset passes.
//!
//! The decision is a pure function of its inputs. Nothing here reads a clock,
//! a file, or a database: the caller assembles the history from the store and
//! hands the current instant in.

use crate::domain::attempt::{AttemptId, AttemptOutcome, AttemptResult, AttemptStarted, DueReason};
use crate::domain::failure::FailureClass;
use crate::domain::time::{MonotonicDuration, UtcTimestamp};

/// One entry of the account's attempt history, as the caller reads it from
/// the store: a started attempt and, when it ever terminated, its result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptHistoryEntry {
    pub attempt: AttemptStarted,
    /// The instant this attempt was owed. This is distinct from when the
    /// scheduler observed and started it, so ordinary cadence can retain its
    /// grid after a late tick.
    pub due_at: UtcTimestamp,
    pub result: Option<AttemptResult>,
}

impl AttemptHistoryEntry {
    /// The freshest evidence instant this entry carries: the result's finish
    /// when it terminated, the start otherwise.
    fn evidence_at(&self) -> UtcTimestamp {
        self.result
            .as_ref()
            .map(|result| result.finished_at())
            .unwrap_or_else(|| self.attempt.started_at())
    }

    /// The prior fact the decision was based on.
    fn basis(&self) -> DueBasisRef {
        match self.result.as_ref() {
            Some(result) => DueBasisRef::Result(result.attempt_id()),
            None => DueBasisRef::Attempt(self.attempt.attempt_id()),
        }
    }
}

/// The resolved policy values the decision reads. The snapshot records these
/// in force at the attempt (PLAN.md 12.2); the caller resolves them, so this
/// module never reads configuration and never touches storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DuePolicy {
    /// The account's ordinary sampling interval.
    pub ordinary_cadence: MonotonicDuration,
    /// How close to a known reset the sampler starts demanding pre-reset
    /// evidence (the configured edge lead).
    pub reset_edge_lead: MonotonicDuration,
    /// The longest `Retry-After` the decision honours past a refusal
    /// (aub-6w85). A delay above the ceiling is clamped to it, so a broken
    /// or malicious header cannot silence an account for a day.
    pub retry_after_cap: MonotonicDuration,
}

/// Which prior fact the decision was based on, at the domain level. The store
/// persists its row-id spelling of the same choice on the attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DueBasisRef {
    /// The decision reused an attempt's recency or its observation.
    Attempt(AttemptId),
    /// The decision was postponed by, or based on, a result.
    Result(AttemptId),
}

/// What the due decision concluded for one account at one instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DueDecision {
    /// An attempt is owed now, carrying this reason on the attempt row.
    Due {
        /// The logical instant at which this attempt became owed. The sampler
        /// persists it separately from the observed request start.
        due_at: UtcTimestamp,
        reason: DueReason,
        /// The prior attempt or result the decision was based on, persisted
        /// as the attempt's due basis.
        basis: Option<DueBasisRef>,
    },
    /// No attempt is owed yet; the earliest instant the next evaluation
    /// could find one due, and the reason it would carry then.
    NotYet {
        next_due_at: UtcTimestamp,
        reason: DueReason,
    },
}

/// The due inputs for one account: the policy in force, the account's own
/// attempt history ordered by start, the reset timestamps the previous
/// observations supplied, and now.
#[derive(Debug, Clone)]
pub struct DueInputs {
    pub policy: DuePolicy,
    pub history: Vec<AttemptHistoryEntry>,
    pub known_resets: Vec<UtcTimestamp>,
    pub now: UtcTimestamp,
    /// An explicit operator or hook request: due regardless of history.
    pub forced: bool,
}

/// Evaluates the due decision for one account.
///
/// The history must be ordered by start time; the evaluation reads only the
/// most recent entries and the reset instants, never a position.
pub fn evaluate(inputs: &DueInputs) -> DueDecision {
    let now = inputs.now.unix_nanos();
    let last = inputs.history.last();
    let last_evidence = last.as_ref().map(|entry| entry.evidence_at());

    if inputs.forced {
        return DueDecision::Due {
            due_at: inputs.now,
            reason: DueReason::ForcedOrManual,
            basis: None,
        };
    }

    let passed_reset = inputs
        .known_resets
        .iter()
        .copied()
        .filter(|reset| reset.unix_nanos() <= now)
        .max();
    let next_reset = inputs
        .known_resets
        .iter()
        .copied()
        .find(|reset| reset.unix_nanos() > now);

    // Rule 2: a passed reset no attempt has observed since. An attempt started
    // exactly at the reset instant counts: its observation carries the
    // post-reset state.
    if let Some(reset) = passed_reset
        && !last.is_some_and(|entry| entry.attempt.started_at().unix_nanos() >= reset.unix_nanos())
    {
        return DueDecision::Due {
            due_at: reset,
            reason: DueReason::PostResetConfirmation,
            basis: last.as_ref().map(|entry| entry.basis()),
        };
    }

    // Rule 3: a reset approaches within the edge lead and no pre-reset
    // evidence is as fresh as the lead instant.
    if let Some(reset) = next_reset {
        let lead_left = reset.unix_nanos() - now;
        let evidence_fresh_enough = last_evidence.is_some_and(|evidence| {
            evidence.unix_nanos()
                >= reset.unix_nanos() - inputs.policy.reset_edge_lead.as_nanos() as i64
        });
        if lead_left <= inputs.policy.reset_edge_lead.as_nanos() as i64 && !evidence_fresh_enough {
            return DueDecision::Due {
                due_at: UtcTimestamp::from_unix_nanos(
                    reset.unix_nanos() - inputs.policy.reset_edge_lead.as_nanos() as i64,
                ),
                reason: DueReason::ResetEdge,
                basis: last.as_ref().map(|entry| entry.basis()),
            };
        }
    }

    // Rule 4: the ordinary interval expired since the previous due instant,
    // unless the most recent result's capped Retry-After holds the account
    // past that boundary - the postponement governs the due instant itself,
    // not only the not-yet report, so a lockout outlasting the cadence costs
    // one refused attempt and not one per tick.
    let Some(entry) = last else {
        return DueDecision::Due {
            due_at: inputs.now,
            reason: DueReason::OrdinaryCadence,
            basis: None,
        };
    };
    let cadence_due_at = UtcTimestamp::from_unix_nanos(
        entry.due_at.unix_nanos() + inputs.policy.ordinary_cadence.as_nanos() as i64,
    );
    let effective_due_at = retry_postponement(entry, inputs.policy.retry_after_cap)
        .filter(|postponed_until| postponed_until.unix_nanos() > cadence_due_at.unix_nanos())
        .unwrap_or(cadence_due_at);
    if now < effective_due_at.unix_nanos() {
        return DueDecision::NotYet {
            next_due_at: effective_due_at,
            reason: DueReason::OrdinaryCadence,
        };
    }
    DueDecision::Due {
        due_at: effective_due_at,
        reason: DueReason::OrdinaryCadence,
        basis: Some(AttemptHistoryEntry::basis(entry)),
    }
}

/// The instant the next attempt is owed after one result, reconstructible
/// from the result plus the policy snapshot alone (PLAN.md 14.5): a
/// `Retry-After` postpones by its own value capped at the policy ceiling,
/// and never below the ordinary cadence, while every other outcome resumes
/// the ordinary cadence from the attempt's finish. This is the function
/// coverage relies on to tell a deliberately postponed interval from a
/// missed one.
pub fn next_due_after(
    result: &AttemptResult,
    ordinary_cadence: MonotonicDuration,
    retry_after_cap: MonotonicDuration,
) -> UtcTimestamp {
    let retry_after = match result.outcome() {
        AttemptOutcome::Unreachable(FailureClass::RateLimited { retry_after }) => retry_after,
        AttemptOutcome::Success | AttemptOutcome::AuthRequired => None,
        AttemptOutcome::Unreachable(
            FailureClass::DnsFailure
            | FailureClass::ConnectTimeout
            | FailureClass::ReadTimeout
            | FailureClass::TotalBudgetExpired
            | FailureClass::HttpStatus(_)
            | FailureClass::MalformedBody
            | FailureClass::MissingRequiredField
            | FailureClass::SchemaDrift,
        ) => None,
    };
    let capped = retry_after.map(|delay| delay.min(retry_after_cap));
    let postpone = capped
        .map(|delay| delay.as_nanos())
        .unwrap_or(0)
        .max(ordinary_cadence.as_nanos());
    UtcTimestamp::from_unix_nanos(result.finished_at().unix_nanos() + postpone as i64)
}

/// The instant a rate-limited result holds the account until, when it does:
/// the refusal's finish plus its `Retry-After` capped at the policy ceiling,
/// so a header cannot hold the account longer than the operator allowed.
fn retry_postponement(entry: &AttemptHistoryEntry, cap: MonotonicDuration) -> Option<UtcTimestamp> {
    let result = entry.result.as_ref()?;
    match result.outcome() {
        AttemptOutcome::Unreachable(FailureClass::RateLimited { retry_after }) => {
            let delay = retry_after?;
            Some(UtcTimestamp::from_unix_nanos(
                result.finished_at().unix_nanos() + delay.min(cap).as_nanos() as i64,
            ))
        }
        AttemptOutcome::Success | AttemptOutcome::AuthRequired => None,
        AttemptOutcome::Unreachable(
            FailureClass::DnsFailure
            | FailureClass::ConnectTimeout
            | FailureClass::ReadTimeout
            | FailureClass::TotalBudgetExpired
            | FailureClass::HttpStatus(_)
            | FailureClass::MalformedBody
            | FailureClass::MissingRequiredField
            | FailureClass::SchemaDrift,
        ) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const MINUTE: i64 = 60_000_000_000;

    fn policy(cadence_secs: u64, edge_lead_secs: u64) -> DuePolicy {
        policy_with_cap(cadence_secs, edge_lead_secs, 3600)
    }

    fn policy_with_cap(cadence_secs: u64, edge_lead_secs: u64, cap_secs: u64) -> DuePolicy {
        DuePolicy {
            ordinary_cadence: MonotonicDuration::from_seconds(cadence_secs),
            reset_edge_lead: MonotonicDuration::from_seconds(edge_lead_secs),
            retry_after_cap: MonotonicDuration::from_seconds(cap_secs),
        }
    }

    fn success_entry(started_nanos: i64) -> AttemptHistoryEntry {
        let id = AttemptId::new(started_nanos as u64);
        let started_at = UtcTimestamp::from_unix_nanos(started_nanos);
        AttemptHistoryEntry {
            attempt: AttemptStarted::new(id, started_at),
            due_at: started_at,
            result: Some(AttemptResult::new(
                id,
                started_at,
                MonotonicDuration::from_nanos(0),
                AttemptOutcome::Success,
            )),
        }
    }

    fn success_entry_due_at(due_at_nanos: i64, started_nanos: i64) -> AttemptHistoryEntry {
        let mut entry = success_entry(started_nanos);
        entry.due_at = UtcTimestamp::from_unix_nanos(due_at_nanos);
        entry
    }

    fn rate_limited_entry(
        started_nanos: i64,
        retry_after_secs: Option<u64>,
    ) -> AttemptHistoryEntry {
        rate_limited_entry_finished_at(started_nanos, started_nanos, retry_after_secs)
    }

    fn rate_limited_entry_finished_at(
        started_nanos: i64,
        finished_nanos: i64,
        retry_after_secs: Option<u64>,
    ) -> AttemptHistoryEntry {
        let id = AttemptId::new(started_nanos as u64);
        let started_at = UtcTimestamp::from_unix_nanos(started_nanos);
        let finished_at = UtcTimestamp::from_unix_nanos(finished_nanos);
        AttemptHistoryEntry {
            attempt: AttemptStarted::new(id, started_at),
            due_at: started_at,
            result: Some(AttemptResult::new(
                id,
                finished_at,
                MonotonicDuration::from_nanos(0),
                AttemptOutcome::Unreachable(FailureClass::RateLimited {
                    retry_after: retry_after_secs.map(MonotonicDuration::from_seconds),
                }),
            )),
        }
    }

    // Rule 1: forced beats everything, including a history that would
    // otherwise say "not yet".
    #[test]
    fn forced_is_due_regardless_of_a_fresh_history() {
        let inputs = DueInputs {
            policy: policy(300, 120),
            history: vec![success_entry(0)],
            known_resets: vec![],
            now: UtcTimestamp::from_unix_nanos(10 * MINUTE),
            forced: true,
        };
        assert_eq!(
            evaluate(&inputs),
            DueDecision::Due {
                due_at: UtcTimestamp::from_unix_nanos(10 * MINUTE),
                reason: DueReason::ForcedOrManual,
                basis: None,
            }
        );
    }

    // Rule 4, empty-history case: an account with no history at all is due.
    #[test]
    fn no_history_is_due_on_ordinary_cadence_with_no_basis() {
        let inputs = DueInputs {
            policy: policy(300, 120),
            history: vec![],
            known_resets: vec![],
            now: UtcTimestamp::from_unix_nanos(0),
            forced: false,
        };
        assert_eq!(
            evaluate(&inputs),
            DueDecision::Due {
                due_at: UtcTimestamp::from_unix_nanos(0),
                reason: DueReason::OrdinaryCadence,
                basis: None,
            }
        );
    }

    // Rule 4: the ordinary interval expired since the previous due instant.
    #[test]
    fn expired_cadence_is_due_on_ordinary_cadence_with_a_basis() {
        let last = success_entry(0);
        let inputs = DueInputs {
            policy: policy(300, 120),
            history: vec![last.clone()],
            known_resets: vec![],
            now: UtcTimestamp::from_unix_nanos(5 * 60 * 1_000_000_000),
            forced: false,
        };
        assert_eq!(
            evaluate(&inputs),
            DueDecision::Due {
                due_at: UtcTimestamp::from_unix_nanos(5 * MINUTE),
                reason: DueReason::OrdinaryCadence,
                basis: Some(last.basis()),
            }
        );
    }

    // Rule 5: not yet due before the cadence elapses.
    #[test]
    fn unexpired_cadence_is_not_yet_due() {
        let last = success_entry(0);
        let inputs = DueInputs {
            policy: policy(300, 120),
            history: vec![last],
            known_resets: vec![],
            now: UtcTimestamp::from_unix_nanos(60 * 1_000_000_000),
            forced: false,
        };
        assert_eq!(
            evaluate(&inputs),
            DueDecision::NotYet {
                next_due_at: UtcTimestamp::from_unix_nanos(5 * 60 * 1_000_000_000),
                reason: DueReason::OrdinaryCadence,
            }
        );
    }

    // Rule 3: a reset approaches within the edge lead and the only evidence on
    // hand is older than the lead instant before it.
    #[test]
    fn approaching_reset_with_stale_evidence_is_due_on_reset_edge() {
        let last = success_entry(0);
        let reset = UtcTimestamp::from_unix_nanos(3 * MINUTE);
        let inputs = DueInputs {
            policy: policy(300, 120), // 5 min cadence, 2 min edge lead
            history: vec![last.clone()],
            known_resets: vec![reset],
            // 1 minute left before the reset: within the 2-minute lead, and
            // the only evidence (from t=0) is older than reset-lead (t=1min).
            now: UtcTimestamp::from_unix_nanos(2 * MINUTE),
            forced: false,
        };
        assert_eq!(
            evaluate(&inputs),
            DueDecision::Due {
                due_at: UtcTimestamp::from_unix_nanos(MINUTE),
                reason: DueReason::ResetEdge,
                basis: Some(last.basis()),
            }
        );
    }

    // Rule 3, negative: evidence already at least as fresh as the lead instant
    // must not trigger a redundant reset-edge sample.
    #[test]
    fn approaching_reset_with_fresh_evidence_is_not_due_on_reset_edge() {
        let reset = UtcTimestamp::from_unix_nanos(3 * MINUTE);
        // Evidence at t=100s is >= reset-lead (180s-120s=60s), so it counts as
        // the pre-reset sample already.
        let last = success_entry(100 * 1_000_000_000);
        let inputs = DueInputs {
            policy: policy(300, 120),
            history: vec![last],
            known_resets: vec![reset],
            now: UtcTimestamp::from_unix_nanos(2 * MINUTE),
            forced: false,
        };
        assert!(matches!(evaluate(&inputs), DueDecision::NotYet { .. }));
    }

    // Rule 2: a known reset has passed and no attempt has observed the
    // post-reset state yet.
    #[test]
    fn passed_reset_with_no_post_reset_observation_is_due_on_post_reset_confirmation() {
        let last = success_entry(0);
        let reset = UtcTimestamp::from_unix_nanos(3 * MINUTE);
        let inputs = DueInputs {
            policy: policy(300, 120),
            history: vec![last.clone()],
            known_resets: vec![reset],
            now: UtcTimestamp::from_unix_nanos(4 * MINUTE),
            forced: false,
        };
        assert_eq!(
            evaluate(&inputs),
            DueDecision::Due {
                due_at: reset,
                reason: DueReason::PostResetConfirmation,
                basis: Some(last.basis()),
            }
        );
    }

    // Rule 2, negative: an attempt that started at or after the reset already
    // carries the post-reset observation, so confirmation must not repeat.
    #[test]
    fn passed_reset_already_observed_is_not_due_on_post_reset_confirmation() {
        let reset = UtcTimestamp::from_unix_nanos(3 * MINUTE);
        let last = success_entry(3 * MINUTE); // started exactly at the reset
        let inputs = DueInputs {
            policy: policy(300, 120),
            history: vec![last],
            known_resets: vec![reset],
            now: UtcTimestamp::from_unix_nanos(4 * MINUTE),
            forced: false,
        };
        assert!(matches!(
            evaluate(&inputs),
            DueDecision::NotYet {
                reason: DueReason::OrdinaryCadence,
                ..
            }
        ));
    }

    // Rule 5: a Retry-After longer than the remaining cadence clamps the next
    // due instant up, so a postponed interval is not misread as a missed one.
    #[test]
    fn retry_after_longer_than_remaining_cadence_postpones_the_next_due_instant() {
        let last = rate_limited_entry(0, Some(600)); // 10 min Retry-After
        let inputs = DueInputs {
            policy: policy(300, 120), // 5 min cadence
            history: vec![last],
            known_resets: vec![],
            now: UtcTimestamp::from_unix_nanos(MINUTE),
            forced: false,
        };
        assert_eq!(
            evaluate(&inputs),
            DueDecision::NotYet {
                // 600s from finish, not the 300s cadence boundary.
                next_due_at: UtcTimestamp::from_unix_nanos(600 * 1_000_000_000),
                reason: DueReason::OrdinaryCadence,
            }
        );
    }

    #[test]
    fn retry_after_is_anchored_on_the_result_finish_not_the_attempt_start() {
        let last = rate_limited_entry_finished_at(0, MINUTE, Some(600));
        let inputs = DueInputs {
            policy: policy(300, 120),
            history: vec![last],
            known_resets: vec![],
            now: UtcTimestamp::from_unix_nanos(2 * MINUTE),
            forced: false,
        };

        assert_eq!(
            evaluate(&inputs),
            DueDecision::NotYet {
                next_due_at: UtcTimestamp::from_unix_nanos(11 * MINUTE),
                reason: DueReason::OrdinaryCadence,
            }
        );
    }

    // A Retry-After shorter than the remaining cadence must not shorten the
    // wait: the cadence boundary still governs.
    #[test]
    fn retry_after_shorter_than_remaining_cadence_does_not_shorten_the_wait() {
        let last = rate_limited_entry(0, Some(30)); // 30s Retry-After
        let inputs = DueInputs {
            policy: policy(300, 120), // 5 min cadence
            history: vec![last],
            known_resets: vec![],
            now: UtcTimestamp::from_unix_nanos(MINUTE),
            forced: false,
        };
        assert_eq!(
            evaluate(&inputs),
            DueDecision::NotYet {
                next_due_at: UtcTimestamp::from_unix_nanos(5 * 60 * 1_000_000_000),
                reason: DueReason::OrdinaryCadence,
            }
        );
    }

    // Rule 5, capped side: a header past the ceiling is clamped to it, so a
    // broken or malicious header cannot silence an account for a day. An
    // implementation that trusted the header verbatim would wait 7200s here.
    #[test]
    fn a_retry_after_above_the_cap_is_clamped_to_the_cap() {
        let last = rate_limited_entry(0, Some(7200)); // 2 h Retry-After
        let inputs = DueInputs {
            policy: policy_with_cap(300, 120, 3600),
            history: vec![last],
            known_resets: vec![],
            now: UtcTimestamp::from_unix_nanos(MINUTE),
            forced: false,
        };
        assert_eq!(
            evaluate(&inputs),
            DueDecision::NotYet {
                next_due_at: UtcTimestamp::from_unix_nanos(3600 * 1_000_000_000),
                reason: DueReason::OrdinaryCadence,
            }
        );
    }

    // Rule 5, uncapped side: a header below the ceiling passes through whole.
    // The near-identical positive that pairs with the clamp above, differing
    // only in which side of the ceiling the delay sits on.
    #[test]
    fn a_retry_after_below_the_cap_passes_through_uncapped() {
        let last = rate_limited_entry(0, Some(1800)); // 30 min Retry-After
        let inputs = DueInputs {
            policy: policy_with_cap(300, 120, 3600),
            history: vec![last],
            known_resets: vec![],
            now: UtcTimestamp::from_unix_nanos(MINUTE),
            forced: false,
        };
        assert_eq!(
            evaluate(&inputs),
            DueDecision::NotYet {
                next_due_at: UtcTimestamp::from_unix_nanos(1800 * 1_000_000_000),
                reason: DueReason::OrdinaryCadence,
            }
        );
    }

    // The scheduler's shape, at due.rs level: the entry the sampler builds
    // (a persisted due instant and a reconstructed result) after a one-hour
    // lockout. Every 300 s tick until the header's instant reports not-yet
    // with the same next due instant, the cadence boundary long past; the
    // tick at the instant itself is due, and the grid resumes from the
    // postponement's end rather than collapsing into a catch-up storm.
    #[test]
    fn an_hour_lockout_holds_every_tick_until_the_header_s_instant() {
        let lockout_policy = policy(300, 120); // 5 min cadence, 1 h cap
        // The entry as the sampler rebuilds it from the store: due at t=0,
        // refused seconds later with Retry-After: 3600.
        let refused = rate_limited_entry_finished_at(0, 1_000_000_000, Some(3600));
        let next_due = UtcTimestamp::from_unix_nanos(3601 * 1_000_000_000);

        // Eleven ticks inside the lockout, on the 300 s cadence grid.
        for tick in 1..=11i64 {
            let now = UtcTimestamp::from_unix_nanos(tick * 5 * MINUTE);
            let inputs = DueInputs {
                policy: lockout_policy,
                history: vec![refused.clone()],
                known_resets: vec![],
                now,
                forced: false,
            };
            assert_eq!(
                evaluate(&inputs),
                DueDecision::NotYet {
                    next_due_at: next_due,
                    reason: DueReason::OrdinaryCadence,
                },
                "tick at {}s must hold until the header's instant",
                now.unix_nanos()
            );
        }

        // The tick after the header's instant is due, at the postponement's
        // end: the grid resumes there, so the next cadence boundary is one
        // interval later, not one per elapsed tick.
        let inputs = DueInputs {
            policy: lockout_policy,
            history: vec![refused],
            known_resets: vec![],
            now: UtcTimestamp::from_unix_nanos(3601 * 1_000_000_000),
            forced: false,
        };
        assert_eq!(
            evaluate(&inputs),
            DueDecision::Due {
                due_at: UtcTimestamp::from_unix_nanos(3601 * 1_000_000_000),
                reason: DueReason::OrdinaryCadence,
                basis: Some(DueBasisRef::Result(AttemptId::new(0))),
            }
        );
    }

    // A reset that has already passed fires the post-reset confirmation even
    // though the most recent result's Retry-After is still holding: a reading
    // at a reset edge is worth one refused attempt.
    #[test]
    fn a_passed_reset_inside_the_postponement_still_fires() {
        let last = rate_limited_entry(0, Some(3600));
        let reset = UtcTimestamp::from_unix_nanos(MINUTE);
        let inputs = DueInputs {
            policy: policy(300, 120),
            history: vec![last],
            known_resets: vec![reset],
            now: UtcTimestamp::from_unix_nanos(2 * MINUTE),
            forced: false,
        };
        assert_eq!(
            evaluate(&inputs),
            DueDecision::Due {
                due_at: reset,
                reason: DueReason::PostResetConfirmation,
                basis: Some(DueBasisRef::Result(AttemptId::new(0))),
            }
        );
    }

    // A reset approaching within the edge lead fires the reset-edge sample
    // inside the postponement, for the same reason.
    #[test]
    fn a_reset_edge_inside_the_postponement_still_fires() {
        let last = rate_limited_entry(0, Some(3600));
        let reset = UtcTimestamp::from_unix_nanos(3 * MINUTE);
        let inputs = DueInputs {
            policy: policy(300, 120), // 2 min edge lead
            history: vec![last],
            known_resets: vec![reset],
            now: UtcTimestamp::from_unix_nanos(2 * MINUTE),
            forced: false,
        };
        assert_eq!(
            evaluate(&inputs),
            DueDecision::Due {
                due_at: UtcTimestamp::from_unix_nanos(MINUTE),
                reason: DueReason::ResetEdge,
                basis: Some(DueBasisRef::Result(AttemptId::new(0))),
            }
        );
    }

    #[test]
    fn ordinary_cadence_keeps_its_grid_after_a_late_attempt() {
        let inputs = DueInputs {
            policy: policy(300, 120),
            // The attempt became due at t=0 but did not start until t=40s.
            history: vec![success_entry_due_at(0, 40 * 1_000_000_000)],
            known_resets: vec![],
            now: UtcTimestamp::from_unix_nanos(MINUTE),
            forced: false,
        };

        assert_eq!(
            evaluate(&inputs),
            DueDecision::NotYet {
                // 260 seconds remain from the first-minute scheduler tick,
                // rather than a full cadence after the observed start.
                next_due_at: UtcTimestamp::from_unix_nanos(5 * MINUTE),
                reason: DueReason::OrdinaryCadence,
            }
        );
    }

    #[test]
    fn simulated_hour_with_late_starts_keeps_twelve_attempts_on_the_grid() {
        let sim_policy = policy(300, 120);
        let late_start = 40 * 1_000_000_000;
        let mut history = Vec::new();
        let mut due_instants = Vec::new();

        for tick in 0..60i64 {
            let now = UtcTimestamp::from_unix_nanos(tick * MINUTE);
            let inputs = DueInputs {
                policy: sim_policy,
                history: history.clone(),
                known_resets: vec![],
                now,
                forced: false,
            };
            if let DueDecision::Due {
                due_at,
                reason: DueReason::OrdinaryCadence,
                ..
            } = evaluate(&inputs)
            {
                due_instants.push(due_at);
                history.push(success_entry_due_at(
                    due_at.unix_nanos(),
                    now.unix_nanos() + late_start,
                ));
            }
        }

        assert_eq!(due_instants.len(), 12);
        assert_eq!(due_instants[3].unix_nanos(), 15 * MINUTE);
    }

    /// Integration: a scheduler ticking every minute against a 5-minute
    /// ordinary cadence, over a full simulated day, with two known resets
    /// spaced far enough apart not to interact. The expected count is
    /// hand-computed rather than read back from a run:
    ///
    /// - Reset-free, the grid fires at every multiple of 5 minutes in
    ///   `[0, 1440)` minutes: `1440 / 5 = 288` attempts.
    /// - Each reset (chosen on that grid, at minute 360 and minute 1080)
    ///   perturbs the grid by exactly one extra attempt: the account's last
    ///   evidence before the reset is always 5 minutes stale relative to the
    ///   grid, which is older than the 2-minute reset-edge lead, so rule 3
    ///   fires 2 minutes early (`reset - 2min`). The reset instant itself
    ///   still produces an attempt, but via rule 2 (`PostResetConfirmation`)
    ///   rather than rule 4, since the reset-edge attempt is now the most
    ///   recent evidence. The grid then continues unperturbed from the reset
    ///   instant, because the reset lands exactly on it.
    /// - Total: `288 + 2 = 290`, of which exactly 2 are `ResetEdge` and
    ///   exactly 2 are `PostResetConfirmation`; the remaining 286 are
    ///   `OrdinaryCadence` (288 grid slots minus the 2 the resets reclassified).
    #[test]
    fn simulated_day_ticking_every_minute_yields_the_expected_attempt_count_including_reset_edges()
    {
        let sim_policy = policy(300, 120); // 5 min cadence, 2 min edge lead
        let known_resets = vec![
            UtcTimestamp::from_unix_nanos(360 * MINUTE),  // 6h
            UtcTimestamp::from_unix_nanos(1080 * MINUTE), // 18h
        ];

        let mut history: Vec<AttemptHistoryEntry> = Vec::new();
        let mut total = 0u32;
        let mut reset_edge = 0u32;
        let mut post_reset = 0u32;
        let mut ordinary = 0u32;

        for tick in 0..1440i64 {
            let now = UtcTimestamp::from_unix_nanos(tick * MINUTE);
            let inputs = DueInputs {
                policy: sim_policy,
                history: history.clone(),
                known_resets: known_resets.clone(),
                now,
                forced: false,
            };
            if let DueDecision::Due { due_at, reason, .. } = evaluate(&inputs) {
                total += 1;
                match reason {
                    DueReason::ResetEdge => reset_edge += 1,
                    DueReason::PostResetConfirmation => post_reset += 1,
                    DueReason::OrdinaryCadence => ordinary += 1,
                    DueReason::ForcedOrManual => unreachable!("forced is never set in this sim"),
                }
                history.push(success_entry_due_at(due_at.unix_nanos(), now.unix_nanos()));
            }
        }

        assert_eq!(
            total, 290,
            "expected 288 grid attempts plus 1 early reset-edge sample per reset"
        );
        assert_eq!(
            reset_edge, 2,
            "one early sample per reset, not per tick within the lead window"
        );
        assert_eq!(
            post_reset, 2,
            "one confirmation per reset, not repeated once observed"
        );
        assert_eq!(
            ordinary, 286,
            "288 grid slots minus the 2 the resets reclassified to PostResetConfirmation"
        );
    }

    proptest::proptest! {
        /// The next-due instant after a result is a pure function of the
        /// result and the policy's ordinary cadence and cap alone (PLAN.md
        /// 14.5): it must match the documented postponement formula
        /// independently computed here, and recomputing it from the same
        /// inputs at a later time (simulating a reconstruction from a
        /// persisted attempt plus its policy snapshot, rather than a value
        /// cached at decision time) must reproduce exactly the same instant.
        #[test]
        fn next_due_after_reconstructs_deterministically_from_result_and_policy(
            finished_secs in 0i64..10_000_000,
            cadence_secs in 1u64..100_000,
            cap_secs in 0u64..100_000,
            retry_after_secs in proptest::option::of(0u64..100_000),
        ) {
            let finished_at = UtcTimestamp::from_unix_nanos(finished_secs * 1_000_000_000);
            let cadence = MonotonicDuration::from_seconds(cadence_secs);
            let cap = MonotonicDuration::from_seconds(cap_secs);
            let retry_after = retry_after_secs.map(MonotonicDuration::from_seconds);
            let result = AttemptResult::new(
                AttemptId::new(1),
                finished_at,
                MonotonicDuration::from_nanos(0),
                AttemptOutcome::Unreachable(FailureClass::RateLimited { retry_after }),
            );

            let postpone_nanos = retry_after
                .map(|delay| delay.min(cap).as_nanos())
                .unwrap_or(0)
                .max(cadence.as_nanos());
            let expected = UtcTimestamp::from_unix_nanos(finished_at.unix_nanos() + postpone_nanos as i64);

            // Two independent reconstructions from the same (result, policy)
            // pair, as two different callers would perform at two different
            // times, must agree with each other and with the formula.
            let first = next_due_after(&result, cadence, cap);
            let second = next_due_after(&result, cadence, cap);
            prop_assert_eq!(first, expected);
            prop_assert_eq!(first, second);
        }

        /// A non-rate-limited outcome always resumes the ordinary cadence
        /// from the attempt's finish, regardless of what it is.
        #[test]
        fn next_due_after_non_rate_limited_outcome_resumes_ordinary_cadence(
            finished_secs in 0i64..10_000_000,
            cadence_secs in 1u64..100_000,
            cap_secs in 0u64..100_000,
            outcome_choice in 0u8..3,
        ) {
            let finished_at = UtcTimestamp::from_unix_nanos(finished_secs * 1_000_000_000);
            let cadence = MonotonicDuration::from_seconds(cadence_secs);
            let cap = MonotonicDuration::from_seconds(cap_secs);
            let outcome = match outcome_choice {
                0 => AttemptOutcome::Success,
                1 => AttemptOutcome::AuthRequired,
                _ => AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
            };
            let result = AttemptResult::new(
                AttemptId::new(1),
                finished_at,
                MonotonicDuration::from_nanos(0),
                outcome,
            );

            let expected = UtcTimestamp::from_unix_nanos(
                finished_at.unix_nanos() + cadence.as_nanos() as i64,
            );
            prop_assert_eq!(next_due_after(&result, cadence, cap), expected);
        }
    }
}
