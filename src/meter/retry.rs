//! The conservative retry and backoff policy for one sampling attempt.
//!
//! Sampling is evidence collection, not a request-success benchmark, and that
//! reframing decides every default here (PLAN.md 14.5):
//!
//! - an authentication outcome is never retried: a retry cannot succeed and it
//!   costs budget;
//! - a rate limit is never retried immediately: an immediate retry makes the
//!   limit worse, and the provider's `Retry-After` is handed to the due
//!   decision to postpone against rather than slept on here;
//! - connection establishment and transient transport failures retry at most a
//!   small bounded number of times with backoff;
//! - no retry sequence may exceed the command's wall-clock budget: a backoff
//!   that would not fit in the remaining budget stops the sequence instead.
//!
//! Every network try is preserved as an ordered [`NetworkTry`] on the one
//! logical attempt. Retries stay visible rather than collapsing into whichever
//! result arrived last, because coverage exists to report the truth and a
//! hidden retry would let it report a cleaner picture than the truth.
//!
//! ## What this module is and is not
//!
//! This is the decision and its evidence types. The production [`RetryEnv`]
//! implementation, which drives the real transport and adapter under bounded
//! scoped threads, lands with `aub sample` (`aub-eun.6`); the seam is defined
//! here so the policy is testable now against a scripted environment. Resolving
//! the policy tunables from the configuration file is a later bead's job: the
//! integration point that already exists is the `retry_backoff_policy` string
//! column on the sampling policy snapshot, and [`RetryBackoffPolicy::render`]
//! and [`RetryBackoffPolicy::parse`] are its codec.

use std::fmt;

use crate::domain::attempt::AttemptOutcome;
use crate::domain::failure::{FailureClass, HttpStatusClass};
use crate::domain::time::{MonotonicDuration, MonotonicInstant};
use crate::meter::adapter::ProviderObservation;

/// How the delay between transient retries grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackoffAlgorithm {
    /// Every retry waits the base delay.
    Fixed,
    /// Retry `n` waits `base * 2^(n - 1)`, saturating rather than overflowing.
    Exponential,
}

/// The resolved retry and backoff policy for one account at one instant.
///
/// Carried in the sampling policy snapshot as a string
/// ([`render`](Self::render) / [`parse`](Self::parse)) so `coverage`
/// reconstructs a past attempt against the policy that was actually in force.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryBackoffPolicy {
    /// The most retries a transient failure may trigger, on top of the first
    /// try. Zero disables retrying entirely.
    pub max_transient_retries: u32,
    /// The delay before the first retry; later retries scale it by the
    /// algorithm.
    pub base_backoff: MonotonicDuration,
    pub algorithm: BackoffAlgorithm,
}

impl RetryBackoffPolicy {
    /// The conservative default: two retries for a transient transport failure,
    /// starting at 250ms and doubling. Small and bounded, as PLAN.md 14.5 asks.
    pub const fn conservative_default() -> Self {
        Self {
            max_transient_retries: 2,
            base_backoff: MonotonicDuration::from_millis(250),
            algorithm: BackoffAlgorithm::Exponential,
        }
    }

    /// The delay before retry number `retry_number` (1 for the first retry).
    fn backoff_for(self, retry_number: u32) -> MonotonicDuration {
        match self.algorithm {
            BackoffAlgorithm::Fixed => self.base_backoff,
            BackoffAlgorithm::Exponential => {
                let factor = 1u64
                    .checked_shl(retry_number.saturating_sub(1))
                    .unwrap_or(u64::MAX);
                MonotonicDuration::from_nanos(self.base_backoff.as_nanos().saturating_mul(factor))
            }
        }
    }

    /// The decision after one network try produced `latest`.
    ///
    /// `retries_used` is how many retries have already happened (0 after the
    /// first failure). `budget_remaining` is `None` when the command's
    /// wall-clock budget is spent. The match is exhaustive with no wildcard arm,
    /// so a new [`AttemptOutcome`] or [`FailureClass`] variant forces a decision
    /// here rather than defaulting to a retry.
    pub fn decide(
        self,
        latest: AttemptOutcome,
        retries_used: u32,
        budget_remaining: Option<MonotonicDuration>,
    ) -> RetryDecision {
        match latest {
            // A retry cannot help a success, and it cannot help an authenticated
            // rejection: the credential is the problem and it will still be the
            // problem next call.
            AttemptOutcome::Success | AttemptOutcome::AuthRequired => RetryDecision::Stop,
            AttemptOutcome::Unreachable(class) => {
                self.decide_unreachable(class, retries_used, budget_remaining)
            }
        }
    }

    fn decide_unreachable(
        self,
        class: FailureClass,
        retries_used: u32,
        budget_remaining: Option<MonotonicDuration>,
    ) -> RetryDecision {
        if !is_transient(class) {
            return RetryDecision::Stop;
        }
        if retries_used >= self.max_transient_retries {
            return RetryDecision::Stop;
        }
        let backoff = self.backoff_for(retries_used + 1);
        match budget_remaining {
            // The backoff itself must fit: a sequence never starts a wait it
            // cannot afford, so the budget is respected without ever being
            // burned on a pointless sleep.
            Some(remaining) if backoff.as_nanos() <= remaining.as_nanos() => {
                RetryDecision::Retry { backoff }
            }
            _ => RetryDecision::Stop,
        }
    }

    /// The stable string form for the `retry_backoff_policy` snapshot column,
    /// `"<algorithm>-<max_retries>-<base_ms>ms"`.
    pub fn render(self) -> String {
        let algorithm = match self.algorithm {
            BackoffAlgorithm::Fixed => "fixed",
            BackoffAlgorithm::Exponential => "exponential",
        };
        let base_ms = self.base_backoff.as_nanos() / 1_000_000;
        format!("{algorithm}-{}-{base_ms}ms", self.max_transient_retries)
    }

    /// Parses [`render`](Self::render)'s output back into a policy.
    pub fn parse(text: &str) -> Result<Self, RetryPolicyParseError> {
        let parts: Vec<&str> = text.split('-').collect();
        let [algorithm, max_retries, base] = parts.as_slice() else {
            return Err(RetryPolicyParseError::Shape(text.to_string()));
        };
        let algorithm = match *algorithm {
            "fixed" => BackoffAlgorithm::Fixed,
            "exponential" => BackoffAlgorithm::Exponential,
            other => return Err(RetryPolicyParseError::UnknownAlgorithm(other.to_string())),
        };
        let max_transient_retries = max_retries
            .parse::<u32>()
            .map_err(|_| RetryPolicyParseError::BadNumber((*max_retries).to_string()))?;
        let base_ms = base
            .strip_suffix("ms")
            .ok_or_else(|| RetryPolicyParseError::BadNumber((*base).to_string()))?
            .parse::<u64>()
            .map_err(|_| RetryPolicyParseError::BadNumber((*base).to_string()))?;
        Ok(Self {
            max_transient_retries,
            base_backoff: MonotonicDuration::from_millis(base_ms),
            algorithm,
        })
    }
}

/// Which failure classes a bounded retry can plausibly help.
///
/// Only connection establishment (`ConnectTimeout`, `DnsFailure`) and transient
/// transport (`ReadTimeout`) qualify, matching PLAN.md 14.5 exactly. A server
/// error is a response and therefore evidence; a client error means this
/// request was wrong; a malformed or incomplete body is an API-contract problem;
/// a rate limit is handled by postponing the next due time, not by retrying;
/// and a budget expiry means there is nothing left to retry with. None of those
/// is retried.
fn is_transient(class: FailureClass) -> bool {
    match class {
        FailureClass::ConnectTimeout | FailureClass::DnsFailure | FailureClass::ReadTimeout => true,
        FailureClass::TotalBudgetExpired
        | FailureClass::HttpStatus(HttpStatusClass::ClientError)
        | FailureClass::HttpStatus(HttpStatusClass::ServerError)
        | FailureClass::RateLimited { .. }
        | FailureClass::MalformedBody
        | FailureClass::MissingRequiredField => false,
    }
}

/// What [`RetryBackoffPolicy::decide`] concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// Wait `backoff`, then try once more.
    Retry { backoff: MonotonicDuration },
    /// The attempt is terminal; do not try again.
    Stop,
}

/// Why a `retry_backoff_policy` string could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryPolicyParseError {
    /// Not the `algorithm-max-basems` shape.
    Shape(String),
    /// The algorithm token is neither `fixed` nor `exponential`.
    UnknownAlgorithm(String),
    /// The retry count or base delay is not a number in the expected form.
    BadNumber(String),
}

impl fmt::Display for RetryPolicyParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Shape(text) => {
                write!(f, "expected 'algorithm-max-basems', got {text:?}")
            }
            Self::UnknownAlgorithm(token) => {
                write!(
                    f,
                    "unknown backoff algorithm {token:?}, expected fixed or exponential"
                )
            }
            Self::BadNumber(token) => write!(f, "not a valid number: {token:?}"),
        }
    }
}

impl std::error::Error for RetryPolicyParseError {}

/// One network try inside a single logical sampling attempt: its position in the
/// sequence, the monotonic instants it started and ended, and how it turned out.
/// The final [`RetryOutcome`] carries these in order so a retry is never lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkTry {
    /// 1-based position in the retry sequence.
    pub ordinal: u32,
    pub started: MonotonicInstant,
    pub ended: MonotonicInstant,
    pub outcome: AttemptOutcome,
}

impl NetworkTry {
    /// How long this try took on the monotonic clock.
    pub fn elapsed(self) -> MonotonicDuration {
        self.ended.duration_since(self.started)
    }
}

/// The terminal result of one logical sampling attempt: the ordered network
/// tries and the observation the sequence ended on.
///
/// One logical sample is exactly one meter attempt and at most one result; the
/// tries are retry metadata on that one result, never extra attempts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryOutcome<T> {
    pub tries: Vec<NetworkTry>,
    pub terminal: ProviderObservation<T>,
}

impl<T> RetryOutcome<T> {
    /// The single attempt outcome to persist for this logical attempt.
    pub fn attempt_outcome(&self) -> AttemptOutcome {
        attempt_outcome_of(&self.terminal)
    }

    /// The provider's advertised retry delay, when the terminal outcome was a
    /// rate limit that supplied one. This is what the due decision reads to
    /// postpone the next attempt rather than treating the interval as missed.
    pub fn retry_after(&self) -> Option<MonotonicDuration> {
        let ProviderObservation::Unreachable(class) = &self.terminal else {
            return None;
        };
        match class {
            FailureClass::RateLimited { retry_after } => *retry_after,
            FailureClass::DnsFailure
            | FailureClass::ConnectTimeout
            | FailureClass::ReadTimeout
            | FailureClass::TotalBudgetExpired
            | FailureClass::HttpStatus(_)
            | FailureClass::MalformedBody
            | FailureClass::MissingRequiredField => None,
        }
    }
}

/// Reduces an adapter's classified observation to the one attempt outcome the
/// retry policy and the attempt record both speak.
pub(crate) fn attempt_outcome_of<T>(observation: &ProviderObservation<T>) -> AttemptOutcome {
    match observation {
        ProviderObservation::Measured(_) => AttemptOutcome::Success,
        ProviderObservation::AuthRequired(_) => AttemptOutcome::AuthRequired,
        ProviderObservation::Unreachable(class) => AttemptOutcome::Unreachable(*class),
    }
}

/// Everything one logical sampling attempt needs from the outside world: the
/// classified network try, the monotonic clock, the remaining command budget,
/// and the blocking wait between retries.
///
/// A single object rather than separate closures so the clock a test advances
/// during [`attempt`](Self::attempt) and [`wait`](Self::wait) is the same clock
/// [`budget_remaining`](Self::budget_remaining) reads. The production
/// implementation (transport, adapter, real clock, `std::thread::sleep`) lands
/// with `aub-eun.6`.
pub trait RetryEnv {
    /// The adapter's reading type.
    type Reading;

    /// Perform one classified network try. In production this is the transport
    /// call plus the adapter classification; the command budget clips the
    /// per-call timeouts, so a try made with no budget left returns
    /// [`FailureClass::TotalBudgetExpired`].
    fn attempt(&mut self) -> ProviderObservation<Self::Reading>;

    /// The monotonic instant now.
    fn now(&self) -> MonotonicInstant;

    /// The remaining command budget, `None` once it is spent.
    fn budget_remaining(&self) -> Option<MonotonicDuration>;

    /// Block for `delay` before the next try.
    fn wait(&mut self, delay: MonotonicDuration);
}

/// Runs one logical sampling attempt under `policy`: try, record, decide, wait,
/// repeat. Returns the ordered tries and the terminal observation. The sequence
/// always terminates: a non-transient outcome, a success, an authentication
/// rejection, the retry ceiling, or a backoff that will not fit the budget each
/// stop it.
pub fn run_with_retry<E: RetryEnv>(
    policy: RetryBackoffPolicy,
    env: &mut E,
) -> RetryOutcome<E::Reading> {
    let mut tries: Vec<NetworkTry> = Vec::new();
    let mut retries_used: u32 = 0;
    loop {
        let started = env.now();
        let observation = env.attempt();
        let ended = env.now();
        let outcome = attempt_outcome_of(&observation);
        tries.push(NetworkTry {
            ordinal: tries.len() as u32 + 1,
            started,
            ended,
            outcome,
        });
        match policy.decide(outcome, retries_used, env.budget_remaining()) {
            RetryDecision::Stop => {
                return RetryOutcome {
                    tries,
                    terminal: observation,
                };
            }
            RetryDecision::Retry { backoff } => {
                env.wait(backoff);
                retries_used += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::failure::AuthReason;
    use crate::domain::time::{Clock, FakeClock, UtcTimestamp};
    use crate::meter::transport::CommandBudget;

    const NO_BUDGET_PRESSURE: Option<MonotonicDuration> =
        Some(MonotonicDuration::from_seconds(3600));

    fn exp_2_250() -> RetryBackoffPolicy {
        RetryBackoffPolicy::conservative_default()
    }

    // --- the pure decision: one case per outcome class -----------------------

    #[test]
    fn a_success_is_never_retried() {
        assert_eq!(
            exp_2_250().decide(AttemptOutcome::Success, 0, NO_BUDGET_PRESSURE),
            RetryDecision::Stop
        );
    }

    #[test]
    fn an_authentication_outcome_is_never_retried() {
        assert_eq!(
            exp_2_250().decide(AttemptOutcome::AuthRequired, 0, NO_BUDGET_PRESSURE),
            RetryDecision::Stop
        );
    }

    #[test]
    fn a_rate_limit_is_never_retried_immediately() {
        let rate_limited = AttemptOutcome::Unreachable(FailureClass::RateLimited {
            retry_after: Some(MonotonicDuration::from_seconds(30)),
        });
        assert_eq!(
            exp_2_250().decide(rate_limited, 0, NO_BUDGET_PRESSURE),
            RetryDecision::Stop
        );
    }

    #[test]
    fn a_connection_or_transport_failure_is_retried_with_backoff() {
        for class in [
            FailureClass::ConnectTimeout,
            FailureClass::DnsFailure,
            FailureClass::ReadTimeout,
        ] {
            assert_eq!(
                exp_2_250().decide(AttemptOutcome::Unreachable(class), 0, NO_BUDGET_PRESSURE),
                RetryDecision::Retry {
                    backoff: MonotonicDuration::from_millis(250)
                },
                "{class:?} should retry"
            );
        }
    }

    /// The planted negative: a server error and a malformed body look retryable
    /// (they are transport-shaped failures) but a conservative evidence
    /// collector does not retry them. An implementation that retried every
    /// `Unreachable` would pass the four tests above and fail this one.
    #[test]
    fn a_server_error_or_malformed_body_or_client_error_is_not_retried() {
        for class in [
            FailureClass::HttpStatus(HttpStatusClass::ServerError),
            FailureClass::HttpStatus(HttpStatusClass::ClientError),
            FailureClass::MalformedBody,
            FailureClass::MissingRequiredField,
            FailureClass::TotalBudgetExpired,
        ] {
            assert_eq!(
                exp_2_250().decide(AttemptOutcome::Unreachable(class), 0, NO_BUDGET_PRESSURE),
                RetryDecision::Stop,
                "{class:?} must not be retried"
            );
        }
    }

    #[test]
    fn the_retry_ceiling_stops_the_sequence() {
        let policy = exp_2_250();
        let transient = AttemptOutcome::Unreachable(FailureClass::ConnectTimeout);
        assert!(matches!(
            policy.decide(transient, 1, NO_BUDGET_PRESSURE),
            RetryDecision::Retry { .. }
        ));
        assert_eq!(
            policy.decide(transient, 2, NO_BUDGET_PRESSURE),
            RetryDecision::Stop,
            "the third try is one past the two-retry ceiling"
        );
    }

    #[test]
    fn a_backoff_that_would_not_fit_the_budget_stops_the_sequence() {
        let policy = exp_2_250();
        let transient = AttemptOutcome::Unreachable(FailureClass::ReadTimeout);
        // 250ms backoff, 100ms left: does not fit.
        assert_eq!(
            policy.decide(transient, 0, Some(MonotonicDuration::from_millis(100))),
            RetryDecision::Stop
        );
        // Budget already spent.
        assert_eq!(policy.decide(transient, 0, None), RetryDecision::Stop);
        // Exactly fits.
        assert_eq!(
            policy.decide(transient, 0, Some(MonotonicDuration::from_millis(250))),
            RetryDecision::Retry {
                backoff: MonotonicDuration::from_millis(250)
            }
        );
    }

    #[test]
    fn exponential_backoff_doubles_and_fixed_does_not() {
        let exp = RetryBackoffPolicy {
            max_transient_retries: 5,
            base_backoff: MonotonicDuration::from_millis(100),
            algorithm: BackoffAlgorithm::Exponential,
        };
        assert_eq!(exp.backoff_for(1), MonotonicDuration::from_millis(100));
        assert_eq!(exp.backoff_for(2), MonotonicDuration::from_millis(200));
        assert_eq!(exp.backoff_for(3), MonotonicDuration::from_millis(400));

        let fixed = RetryBackoffPolicy {
            algorithm: BackoffAlgorithm::Fixed,
            ..exp
        };
        assert_eq!(fixed.backoff_for(1), MonotonicDuration::from_millis(100));
        assert_eq!(fixed.backoff_for(3), MonotonicDuration::from_millis(100));
    }

    // --- the snapshot string codec ------------------------------------------

    #[test]
    fn the_policy_round_trips_through_its_snapshot_string() {
        for policy in [
            RetryBackoffPolicy::conservative_default(),
            RetryBackoffPolicy {
                max_transient_retries: 0,
                base_backoff: MonotonicDuration::from_millis(500),
                algorithm: BackoffAlgorithm::Fixed,
            },
        ] {
            assert_eq!(RetryBackoffPolicy::parse(&policy.render()), Ok(policy));
        }
        assert_eq!(
            RetryBackoffPolicy::conservative_default().render(),
            "exponential-2-250ms"
        );
    }

    #[test]
    fn an_unparseable_policy_string_is_rejected_naming_why() {
        assert!(matches!(
            RetryBackoffPolicy::parse("exponential-2"),
            Err(RetryPolicyParseError::Shape(_))
        ));
        assert!(matches!(
            RetryBackoffPolicy::parse("linear-2-250ms"),
            Err(RetryPolicyParseError::UnknownAlgorithm(_))
        ));
        assert!(matches!(
            RetryBackoffPolicy::parse("exponential-two-250ms"),
            Err(RetryPolicyParseError::BadNumber(_))
        ));
        assert!(matches!(
            RetryBackoffPolicy::parse("exponential-2-250s"),
            Err(RetryPolicyParseError::BadNumber(_))
        ));
    }

    /// The `retry_backoff_policy` column on the sampling policy snapshot
    /// (`src/store/sampling_policy_snapshot.rs`) is a `String`; this is the
    /// value that goes in it and reads back out, so `coverage` reconstructs a
    /// past attempt against the retry policy that was in force. The snapshot
    /// write itself belongs to the policy-resolver bead, which owns both sides.
    #[test]
    fn the_policy_is_a_faithful_string_for_the_snapshot_column() {
        let policy = RetryBackoffPolicy::conservative_default();
        let stored: String = policy.render();
        assert_eq!(RetryBackoffPolicy::parse(&stored), Ok(policy));
    }

    // --- the driver over a scripted environment ----------------------------

    /// A scripted [`RetryEnv`]: a queue of observations to hand out, a fixed
    /// duration charged to the clock per try and per wait, and a real
    /// [`CommandBudget`] over a [`FakeClock`] so budget pressure is exact.
    struct ScriptedEnv {
        clock: FakeClock,
        budget: CommandBudget,
        per_try: MonotonicDuration,
        script: std::collections::VecDeque<ProviderObservation<u64>>,
    }

    impl ScriptedEnv {
        fn new(
            budget: MonotonicDuration,
            per_try: MonotonicDuration,
            script: Vec<ProviderObservation<u64>>,
        ) -> Self {
            let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(0));
            let budget = CommandBudget::new(budget, &clock);
            Self {
                clock,
                budget,
                per_try,
                script: script.into(),
            }
        }
    }

    impl RetryEnv for ScriptedEnv {
        type Reading = u64;

        fn attempt(&mut self) -> ProviderObservation<u64> {
            self.clock.advance(self.per_try);
            self.script
                .pop_front()
                .unwrap_or(ProviderObservation::Unreachable(
                    FailureClass::ConnectTimeout,
                ))
        }

        fn now(&self) -> MonotonicInstant {
            self.clock.monotonic_now()
        }

        fn budget_remaining(&self) -> Option<MonotonicDuration> {
            self.budget.remaining(&self.clock)
        }

        fn wait(&mut self, delay: MonotonicDuration) {
            self.clock.advance(delay);
        }
    }

    fn timeout() -> ProviderObservation<u64> {
        ProviderObservation::Unreachable(FailureClass::ConnectTimeout)
    }

    #[test]
    fn one_logical_sample_is_one_attempt_with_ordered_retry_entries() {
        let mut env = ScriptedEnv::new(
            MonotonicDuration::from_seconds(60),
            MonotonicDuration::from_millis(10),
            vec![timeout(), timeout(), ProviderObservation::Measured(42)],
        );
        let outcome = run_with_retry(RetryBackoffPolicy::conservative_default(), &mut env);

        assert_eq!(outcome.tries.len(), 3, "one initial try and two retries");
        assert_eq!(
            outcome.tries.iter().map(|t| t.ordinal).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        for entry in &outcome.tries {
            assert_eq!(entry.elapsed(), MonotonicDuration::from_millis(10));
        }
        assert_eq!(
            outcome.tries.iter().map(|t| t.outcome).collect::<Vec<_>>(),
            vec![
                AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
                AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
                AttemptOutcome::Success,
            ]
        );
        assert_eq!(outcome.attempt_outcome(), AttemptOutcome::Success);
        assert_eq!(outcome.terminal, ProviderObservation::Measured(42));
    }

    #[test]
    fn the_budget_truncates_a_retry_schedule_that_would_outrun_it() {
        // Budget 400ms; per try 10ms; backoff schedule 250ms, 500ms.
        // try1 (t=10) fails, retry1 backoff 250 fits (390 left) -> wait to 260.
        // try2 (t=270) fails, retry2 backoff 500 > 130 left -> stop.
        let mut env = ScriptedEnv::new(
            MonotonicDuration::from_millis(400),
            MonotonicDuration::from_millis(10),
            vec![timeout(), timeout(), timeout()],
        );
        let outcome = run_with_retry(RetryBackoffPolicy::conservative_default(), &mut env);

        assert_eq!(outcome.tries.len(), 2, "the budget stopped the third try");
        assert_eq!(
            outcome.attempt_outcome(),
            AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
            "the terminal outcome is the last failure actually observed, not a synthetic budget expiry"
        );
    }

    #[test]
    fn a_rate_limit_stops_after_one_try_and_keeps_its_retry_after_for_the_due_decision() {
        let retry_after = MonotonicDuration::from_seconds(45);
        let mut env = ScriptedEnv::new(
            MonotonicDuration::from_seconds(60),
            MonotonicDuration::from_millis(10),
            vec![ProviderObservation::Unreachable(
                FailureClass::RateLimited {
                    retry_after: Some(retry_after),
                },
            )],
        );
        let outcome = run_with_retry(RetryBackoffPolicy::conservative_default(), &mut env);

        assert_eq!(outcome.tries.len(), 1, "no immediate retry on a rate limit");
        assert_eq!(outcome.retry_after(), Some(retry_after));
    }

    #[test]
    fn an_authentication_rejection_stops_after_one_try() {
        let mut env = ScriptedEnv::new(
            MonotonicDuration::from_seconds(60),
            MonotonicDuration::from_millis(10),
            vec![ProviderObservation::AuthRequired(
                AuthReason::CredentialRejected,
            )],
        );
        let outcome = run_with_retry(RetryBackoffPolicy::conservative_default(), &mut env);

        assert_eq!(outcome.tries.len(), 1);
        assert_eq!(outcome.attempt_outcome(), AttemptOutcome::AuthRequired);
        assert_eq!(outcome.retry_after(), None);
    }
}
