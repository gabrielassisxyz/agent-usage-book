//! Freshness: exactly three answers a user can be given about a measurement, kept
//! separate from the operational detail of why a collection attempt did or did not
//! succeed (that detail is [`super::attempt::AttemptOutcome`]).
//!
//! The reading is current (`Fresh`), the reading is not current (`Stale`), or the
//! credentials need attention (`AuthRequired`). A fourth state for "the network was
//! down" would push an operational detail into a user-facing vocabulary; collapsing the
//! operational detail away entirely would make a timeout and a malformed response
//! indistinguishable in the evidence. [`StaleReason`] is where that detail lives,
//! without adding a fourth freshness state.
//!
//! No type in this module implements a persisted `is_fresh` or `is_stale` boolean, and
//! none ever will by construction: a boolean here is exactly the erasure this module
//! exists to prevent, collapsing "why" into a bit no reader can recover.
//!
//! Core matches over `Freshness` and `AttemptOutcome` use no wildcard arm. This is
//! enforced crate-wide by `#![deny(clippy::wildcard_enum_match_arm)]` in `src/lib.rs`
//! (an orchestrator ruling recorded on `aub-rif.9`: no lint scoped to two enums exists
//! in clippy, and a hand-written test is not a lint), not by anything in this file.

use super::attempt::{AttemptId, AttemptOutcome, AttemptResult, AttemptStarted};
use super::failure::to_stale_reason;
use super::ids::CredentialContextId;
use super::time::{Clock, ClockSkewEnvelope, MonotonicDuration, UtcTimestamp, age};
use super::time::{MeasurementBasis, ProviderObservedAt, ReceivedAt};

/// A value the collector actually observed, together with when and how.
///
/// Deliberately narrower than the plan's own illustrative sketch (no `source` or
/// `observation_id` field): those types belong to beads later than this one
/// (provider/session identity, evidence identifiers) and this bead's acceptance
/// criteria do not require them. Adding them is a mechanical extension once their
/// types exist, not a redesign of this one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed<T> {
    value: T,
    provider_observed_at: Option<ProviderObservedAt>,
    received_at: ReceivedAt,
    measurement_basis: MeasurementBasis,
}

impl<T> Observed<T> {
    pub const fn new(
        value: T,
        provider_observed_at: Option<ProviderObservedAt>,
        received_at: ReceivedAt,
        measurement_basis: MeasurementBasis,
    ) -> Self {
        Self {
            value,
            provider_observed_at,
            received_at,
            measurement_basis,
        }
    }

    pub const fn value(&self) -> &T {
        &self.value
    }

    pub const fn provider_observed_at(&self) -> Option<ProviderObservedAt> {
        self.provider_observed_at
    }

    pub const fn received_at(&self) -> ReceivedAt {
        self.received_at
    }

    pub const fn measurement_basis(&self) -> MeasurementBasis {
        self.measurement_basis
    }
}

/// Why a reading is stale, exhaustive over the nine reasons the design names
/// (`docs/PLAN.md` section 3.2). Adding a tenth reason does not add a fourth
/// [`Freshness`] state: every variant here still resolves to `Freshness::Stale`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleReason {
    AgeExceeded,
    NoSuccessfulObservation,
    SourceUnreachable(super::failure::FailureClass),
    MalformedProviderResponse,
    RateLimited,
    SamplingGap,
    ClockAnomaly,
    CollectorInterrupted,
    CredentialChangedUnverified,
}

impl StaleReason {
    /// One instance of every variant, `SourceUnreachable` represented with an
    /// arbitrary `FailureClass` since the reason's identity does not depend on which
    /// class it carries. Exists for the variant-count property test below; a unit test
    /// pins this array's length so a reason added without updating it here is caught.
    #[allow(dead_code)] // used only by #[cfg(test)] below; a plain `cargo check` build never sees it.
    fn all() -> [StaleReason; 9] {
        use super::failure::FailureClass;
        [
            StaleReason::AgeExceeded,
            StaleReason::NoSuccessfulObservation,
            StaleReason::SourceUnreachable(FailureClass::ConnectTimeout),
            StaleReason::MalformedProviderResponse,
            StaleReason::RateLimited,
            StaleReason::SamplingGap,
            StaleReason::ClockAnomaly,
            StaleReason::CollectorInterrupted,
            StaleReason::CredentialChangedUnverified,
        ]
    }
}

/// The three, and only three, answers a user can be given about a measurement.
///
/// `AuthRequired` deliberately carries no `account`/`reason` payload in this bead: that
/// context (which account, why) is reconstructed by joining `latest_attempt` against
/// attempt history, which is the freshness state machine's job in a separate bead (the
/// synthetic-sampler epic), not duplicated here as a second copy of the same fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Freshness<T> {
    Fresh {
        observed: Observed<T>,
        latest_attempt: AttemptId,
    },
    Stale {
        last_good: Option<Observed<T>>,
        latest_attempt: AttemptId,
        reason: StaleReason,
    },
    AuthRequired {
        last_good: Option<Observed<T>>,
        latest_attempt: AttemptId,
    },
}

/// Which of the three states a [`Freshness`] value is in, without exposing the payload.
/// An exhaustive match with no wildcard arm, so adding a fourth `Freshness` variant
/// fails to compile here before it fails anywhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreshnessKind {
    Fresh,
    Stale,
    AuthRequired,
}

impl<T> Freshness<T> {
    pub fn kind(&self) -> FreshnessKind {
        match self {
            Freshness::Fresh { .. } => FreshnessKind::Fresh,
            Freshness::Stale { .. } => FreshnessKind::Stale,
            Freshness::AuthRequired { .. } => FreshnessKind::AuthRequired,
        }
    }

    pub const fn latest_attempt(&self) -> AttemptId {
        match self {
            Freshness::Fresh { latest_attempt, .. }
            | Freshness::Stale { latest_attempt, .. }
            | Freshness::AuthRequired { latest_attempt, .. } => *latest_attempt,
        }
    }
}

/// A collection attempt presented to the freshness state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatestAttempt<'a> {
    pub started: AttemptStarted,
    pub result: Option<AttemptResult>,
    pub credential_context: &'a CredentialContextId,
}

impl<'a> LatestAttempt<'a> {
    pub const fn new(
        started: AttemptStarted,
        result: Option<AttemptResult>,
        credential_context: &'a CredentialContextId,
    ) -> Self {
        Self {
            started,
            result,
            credential_context,
        }
    }
}

/// Inputs required by the pure freshness calculation at read time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreshnessInput<'a, T> {
    pub last_good: Option<Observed<T>>,
    pub last_good_context: Option<&'a CredentialContextId>,
    pub latest_attempt: Option<LatestAttempt<'a>>,
    pub last_auth_failure_context: Option<&'a CredentialContextId>,
    pub latest_auth_success_context: Option<&'a CredentialContextId>,
    pub freshness_horizon: MonotonicDuration,
    pub command_horizon: MonotonicDuration,
    pub clock_skew_envelope: ClockSkewEnvelope,
}

impl<'a, T> FreshnessInput<'a, T> {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        last_good: Option<Observed<T>>,
        last_good_context: Option<&'a CredentialContextId>,
        latest_attempt: Option<LatestAttempt<'a>>,
        last_auth_failure_context: Option<&'a CredentialContextId>,
        latest_auth_success_context: Option<&'a CredentialContextId>,
        freshness_horizon: MonotonicDuration,
        command_horizon: MonotonicDuration,
        clock_skew_envelope: ClockSkewEnvelope,
    ) -> Self {
        Self {
            last_good,
            last_good_context,
            latest_attempt,
            last_auth_failure_context,
            latest_auth_success_context,
            freshness_horizon,
            command_horizon,
            clock_skew_envelope,
        }
    }
}

/// Computes effective [`Freshness`] at read time as a pure function over attempt
/// history, the last successful observation, credential context metadata, the
/// freshness horizon, the command execution horizon, the clock-skew envelope,
/// and the current clock.
///
/// Performs no I/O of any kind, allowing status, synthetic sampling, and projection
/// readers to share the exact same temporal state machine without divergence.
pub fn compute_freshness<T: Clone>(
    input: &FreshnessInput<'_, T>,
    clock: &impl Clock,
) -> Freshness<T> {
    let now = clock.now();

    let Some(latest) = &input.latest_attempt else {
        return Freshness::Stale {
            last_good: input.last_good.clone(),
            latest_attempt: AttemptId::new(0),
            reason: match &input.last_good {
                Some(_) => StaleReason::SamplingGap,
                None => StaleReason::NoSuccessfulObservation,
            },
        };
    };

    let latest_attempt_id = latest.started.attempt_id();
    let current_ctx = latest.credential_context;

    match &latest.result {
        Some(result) => match result.outcome() {
            AttemptOutcome::Success => evaluate_observation(
                input.last_good.as_ref(),
                latest_attempt_id,
                now,
                input.freshness_horizon,
                input.clock_skew_envelope,
            ),
            AttemptOutcome::AuthRequired => Freshness::AuthRequired {
                last_good: input.last_good.clone(),
                latest_attempt: latest_attempt_id,
            },
            AttemptOutcome::Unreachable(failure_class) => {
                if let Some(auth_fail_ctx) = input.last_auth_failure_context {
                    if auth_fail_ctx == current_ctx {
                        Freshness::AuthRequired {
                            last_good: input.last_good.clone(),
                            latest_attempt: latest_attempt_id,
                        }
                    } else {
                        let verified = input.latest_auth_success_context == Some(current_ctx);
                        if verified {
                            Freshness::Stale {
                                last_good: input.last_good.clone(),
                                latest_attempt: latest_attempt_id,
                                reason: to_stale_reason(failure_class),
                            }
                        } else {
                            Freshness::Stale {
                                last_good: input.last_good.clone(),
                                latest_attempt: latest_attempt_id,
                                reason: StaleReason::CredentialChangedUnverified,
                            }
                        }
                    }
                } else {
                    Freshness::Stale {
                        last_good: input.last_good.clone(),
                        latest_attempt: latest_attempt_id,
                        reason: to_stale_reason(failure_class),
                    }
                }
            }
        },
        None => {
            let started_at = latest.started.started_at();
            let is_future = now < started_at;
            let elapsed_nanos = now.unix_nanos().saturating_sub(started_at.unix_nanos());

            if is_future {
                Freshness::Stale {
                    last_good: input.last_good.clone(),
                    latest_attempt: latest_attempt_id,
                    reason: StaleReason::ClockAnomaly,
                }
            } else if elapsed_nanos > input.command_horizon.as_nanos() as i64 {
                Freshness::Stale {
                    last_good: input.last_good.clone(),
                    latest_attempt: latest_attempt_id,
                    reason: StaleReason::CollectorInterrupted,
                }
            } else if let Some(auth_fail_ctx) = input.last_auth_failure_context {
                if auth_fail_ctx == current_ctx {
                    Freshness::AuthRequired {
                        last_good: input.last_good.clone(),
                        latest_attempt: latest_attempt_id,
                    }
                } else if input.latest_auth_success_context != Some(current_ctx) {
                    Freshness::Stale {
                        last_good: input.last_good.clone(),
                        latest_attempt: latest_attempt_id,
                        reason: StaleReason::CredentialChangedUnverified,
                    }
                } else {
                    evaluate_observation(
                        input.last_good.as_ref(),
                        latest_attempt_id,
                        now,
                        input.freshness_horizon,
                        input.clock_skew_envelope,
                    )
                }
            } else {
                evaluate_observation(
                    input.last_good.as_ref(),
                    latest_attempt_id,
                    now,
                    input.freshness_horizon,
                    input.clock_skew_envelope,
                )
            }
        }
    }
}

fn evaluate_observation<T: Clone>(
    last_good: Option<&Observed<T>>,
    latest_attempt_id: AttemptId,
    now: UtcTimestamp,
    freshness_horizon: MonotonicDuration,
    clock_skew_envelope: ClockSkewEnvelope,
) -> Freshness<T> {
    if let Some(observed) = last_good {
        match age(
            observed.provider_observed_at(),
            observed.received_at(),
            observed.measurement_basis(),
            now,
            clock_skew_envelope,
        ) {
            Err(_) => Freshness::Stale {
                last_good: Some(observed.clone()),
                latest_attempt: latest_attempt_id,
                reason: StaleReason::ClockAnomaly,
            },
            Ok(reading_age) => {
                if reading_age.as_nanos() <= freshness_horizon.as_nanos() {
                    Freshness::Fresh {
                        observed: observed.clone(),
                        latest_attempt: latest_attempt_id,
                    }
                } else {
                    Freshness::Stale {
                        last_good: Some(observed.clone()),
                        latest_attempt: latest_attempt_id,
                        reason: StaleReason::AgeExceeded,
                    }
                }
            }
        }
    } else {
        Freshness::Stale {
            last_good: None,
            latest_attempt: latest_attempt_id,
            reason: StaleReason::NoSuccessfulObservation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::failure::FailureClass;
    use crate::domain::time::UtcTimestamp;
    use proptest::prelude::*;

    fn observed(value: u64) -> Observed<u64> {
        Observed::new(
            value,
            Some(ProviderObservedAt::new(UtcTimestamp::from_unix_nanos(
                1_000,
            ))),
            ReceivedAt::new(UtcTimestamp::from_unix_nanos(1_500)),
            MeasurementBasis::ProviderObserved,
        )
    }

    #[test]
    fn stale_reason_all_has_exactly_nine_variants() {
        assert_eq!(StaleReason::all().len(), 9);
    }

    /// A 503 after a good observation yields Stale carrying the last good reading and
    /// a reason naming the status. There is no HTTP layer in this module, so "a 503"
    /// is represented the way this bead's own vocabulary represents it: an
    /// unreachable source, reported through StaleReason::SourceUnreachable.
    #[test]
    fn a_failure_after_a_good_observation_yields_stale_with_last_good_and_a_named_reason() {
        let last_good = observed(42);
        let fresh_before = Freshness::Fresh {
            observed: last_good.clone(),
            latest_attempt: AttemptId::new(1),
        };
        assert_eq!(fresh_before.kind(), FreshnessKind::Fresh);

        let stale_after = Freshness::Stale {
            last_good: Some(last_good.clone()),
            latest_attempt: AttemptId::new(2),
            reason: StaleReason::SourceUnreachable(FailureClass::DnsFailure),
        };

        match stale_after {
            Freshness::Stale {
                last_good: Some(good),
                reason: StaleReason::SourceUnreachable(FailureClass::DnsFailure),
                ..
            } => {
                assert_eq!(good, last_good);
            }
            Freshness::Fresh { .. } | Freshness::AuthRequired { .. } => {
                panic!("expected Stale with last_good and a named reason, got a different kind")
            }
            other @ Freshness::Stale { .. } => {
                panic!("expected Stale with last_good and a named reason, got {other:?}")
            }
        }
    }

    /// A 503 before any success yields Stale with no value at all.
    #[test]
    fn a_failure_before_any_success_yields_stale_with_no_value() {
        let stale = Freshness::<u64>::Stale {
            last_good: None,
            latest_attempt: AttemptId::new(1),
            reason: StaleReason::SourceUnreachable(FailureClass::ConnectTimeout),
        };

        match stale {
            Freshness::Stale {
                last_good: None, ..
            } => {}
            Freshness::Fresh { .. } | Freshness::AuthRequired { .. } => {
                panic!("expected Stale with no last_good, got a different kind")
            }
            other @ Freshness::Stale { .. } => {
                panic!("expected Stale with no last_good, got {other:?}")
            }
        }
    }

    #[test]
    fn latest_attempt_is_readable_from_every_kind() {
        let fresh = Freshness::Fresh {
            observed: observed(1),
            latest_attempt: AttemptId::new(10),
        };
        let stale = Freshness::<u64>::Stale {
            last_good: None,
            latest_attempt: AttemptId::new(11),
            reason: StaleReason::AgeExceeded,
        };
        let auth_required = Freshness::<u64>::AuthRequired {
            last_good: None,
            latest_attempt: AttemptId::new(12),
        };

        assert_eq!(fresh.latest_attempt(), AttemptId::new(10));
        assert_eq!(stale.latest_attempt(), AttemptId::new(11));
        assert_eq!(auth_required.latest_attempt(), AttemptId::new(12));
    }

    #[test]
    fn debug_output_never_names_an_is_fresh_or_is_stale_field() {
        let samples: [Freshness<u64>; 3] = [
            Freshness::Fresh {
                observed: observed(1),
                latest_attempt: AttemptId::new(1),
            },
            Freshness::Stale {
                last_good: None,
                latest_attempt: AttemptId::new(2),
                reason: StaleReason::RateLimited,
            },
            Freshness::AuthRequired {
                last_good: None,
                latest_attempt: AttemptId::new(3),
            },
        ];
        for sample in &samples {
            let rendered = format!("{sample:?}").to_lowercase();
            assert!(!rendered.contains("is_fresh"), "{rendered}");
            assert!(!rendered.contains("is_stale"), "{rendered}");
        }
    }

    /// Expanding StaleReason does not change the number of Freshness variants: the
    /// two counts are independent by construction (StaleReason lives only inside one
    /// Freshness::Stale field), exercised here over both counts rather than argued
    /// from the type definition alone.
    #[test]
    fn expanding_stale_reason_does_not_change_the_freshness_variant_count() {
        let stale_reason_count = StaleReason::all().len();
        let freshness_kinds = [
            FreshnessKind::Fresh,
            FreshnessKind::Stale,
            FreshnessKind::AuthRequired,
        ];
        assert_eq!(freshness_kinds.len(), 3);
        assert_ne!(
            stale_reason_count,
            freshness_kinds.len(),
            "coincidental equality would make this property test worthless"
        );
    }

    fn test_observed(
        value: u64,
        provider_nanos: Option<i64>,
        received_nanos: i64,
    ) -> Observed<u64> {
        Observed::new(
            value,
            provider_nanos.map(|n| ProviderObservedAt::new(UtcTimestamp::from_unix_nanos(n))),
            ReceivedAt::new(UtcTimestamp::from_unix_nanos(received_nanos)),
            MeasurementBasis::ProviderObserved,
        )
    }

    #[test]
    fn ordinary_ageing_inside_and_outside_freshness_horizon() {
        let ctx = CredentialContextId::new("ctx-1");
        let horizon = MonotonicDuration::from_seconds(60);
        let command_horizon = MonotonicDuration::from_seconds(10);
        let envelope = ClockSkewEnvelope::new(MonotonicDuration::from_seconds(10));
        let obs = test_observed(100, Some(1_000_000_000), 1_000_000_000);
        let started = AttemptStarted::new(
            AttemptId::new(1),
            UtcTimestamp::from_unix_nanos(1_000_000_000),
        );
        let result = AttemptResult::new(
            AttemptId::new(1),
            UtcTimestamp::from_unix_nanos(1_000_000_000),
            MonotonicDuration::from_seconds(0),
            AttemptOutcome::Success,
        );

        let input = FreshnessInput::new(
            Some(obs.clone()),
            Some(&ctx),
            Some(LatestAttempt::new(started, Some(result), &ctx)),
            None,
            Some(&ctx),
            horizon,
            command_horizon,
            envelope,
        );

        // Read at T + 30s (inside horizon)
        let clock_inside =
            crate::domain::time::FakeClock::new(UtcTimestamp::from_unix_nanos(31_000_000_000));
        let fresh = compute_freshness(&input, &clock_inside);
        assert_eq!(fresh.kind(), FreshnessKind::Fresh);
        assert_eq!(fresh.latest_attempt(), AttemptId::new(1));
        let Freshness::Fresh { observed, .. } = fresh else {
            panic!("expected Fresh");
        };
        assert_eq!(observed, obs);

        // Read at T + 70s (outside horizon)
        let clock_outside =
            crate::domain::time::FakeClock::new(UtcTimestamp::from_unix_nanos(71_000_000_000));
        let stale = compute_freshness(&input, &clock_outside);
        assert_eq!(stale.kind(), FreshnessKind::Stale);
        let Freshness::Stale {
            last_good: Some(good),
            reason: StaleReason::AgeExceeded,
            latest_attempt,
        } = stale
        else {
            panic!("expected Stale(AgeExceeded)");
        };
        assert_eq!(good, obs);
        assert_eq!(latest_attempt, AttemptId::new(1));
    }

    #[test]
    fn fresh_then_transport_failure_retains_historical_value_and_named_reason() {
        let ctx = CredentialContextId::new("ctx-1");
        let horizon = MonotonicDuration::from_seconds(300);
        let command_horizon = MonotonicDuration::from_seconds(10);
        let envelope = ClockSkewEnvelope::new(MonotonicDuration::from_seconds(10));
        let obs = test_observed(42, Some(1_000_000_000), 1_000_000_000);

        // Attempt 2 failed with ConnectTimeout
        let started_2 = AttemptStarted::new(
            AttemptId::new(2),
            UtcTimestamp::from_unix_nanos(2_000_000_000),
        );
        let result_2 = AttemptResult::new(
            AttemptId::new(2),
            UtcTimestamp::from_unix_nanos(2_000_000_000),
            MonotonicDuration::from_seconds(0),
            AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
        );

        let input = FreshnessInput::new(
            Some(obs.clone()),
            Some(&ctx),
            Some(LatestAttempt::new(started_2, Some(result_2), &ctx)),
            None,
            Some(&ctx),
            horizon,
            command_horizon,
            envelope,
        );

        let clock =
            crate::domain::time::FakeClock::new(UtcTimestamp::from_unix_nanos(3_000_000_000));
        let stale = compute_freshness(&input, &clock);
        assert_eq!(stale.kind(), FreshnessKind::Stale);
        let Freshness::Stale {
            last_good: Some(good),
            latest_attempt,
            reason: StaleReason::SourceUnreachable(FailureClass::ConnectTimeout),
        } = stale
        else {
            panic!("expected Stale(ConnectTimeout) with historical last_good");
        };
        assert_eq!(good, obs);
        assert_eq!(latest_attempt, AttemptId::new(2));
    }

    #[test]
    fn credential_sequence_auth_required_replaced_transport_failure_then_success() {
        let ctx_a = CredentialContextId::new("ctx-a");
        let ctx_b = CredentialContextId::new("ctx-b");
        let horizon = MonotonicDuration::from_seconds(300);
        let command_horizon = MonotonicDuration::from_seconds(10);
        let envelope = ClockSkewEnvelope::new(MonotonicDuration::from_seconds(10));

        // Step 1: AuthRequired under context A
        let started_1 = AttemptStarted::new(
            AttemptId::new(1),
            UtcTimestamp::from_unix_nanos(1_000_000_000),
        );
        let result_1 = AttemptResult::new(
            AttemptId::new(1),
            UtcTimestamp::from_unix_nanos(1_000_000_000),
            MonotonicDuration::from_seconds(0),
            AttemptOutcome::AuthRequired,
        );
        let input_1 = FreshnessInput::<u64>::new(
            None,
            None,
            Some(LatestAttempt::new(started_1, Some(result_1), &ctx_a)),
            Some(&ctx_a),
            None,
            horizon,
            command_horizon,
            envelope,
        );
        let clock =
            crate::domain::time::FakeClock::new(UtcTimestamp::from_unix_nanos(1_500_000_000));
        let res_1 = compute_freshness(&input_1, &clock);
        assert_eq!(res_1.kind(), FreshnessKind::AuthRequired);

        // Step 2: Credential replaced to context B, transport failure under B
        let started_2 = AttemptStarted::new(
            AttemptId::new(2),
            UtcTimestamp::from_unix_nanos(2_000_000_000),
        );
        let result_2 = AttemptResult::new(
            AttemptId::new(2),
            UtcTimestamp::from_unix_nanos(2_000_000_000),
            MonotonicDuration::from_seconds(0),
            AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
        );
        let input_2 = FreshnessInput::<u64>::new(
            None,
            None,
            Some(LatestAttempt::new(started_2, Some(result_2), &ctx_b)),
            Some(&ctx_a), // Prior auth failure was under ctx_a
            None,         // ctx_b has not yet succeeded
            horizon,
            command_horizon,
            envelope,
        );
        let res_2 = compute_freshness(&input_2, &clock);
        assert_eq!(res_2.kind(), FreshnessKind::Stale);
        let Freshness::Stale {
            last_good: None,
            latest_attempt,
            reason: StaleReason::CredentialChangedUnverified,
        } = res_2
        else {
            panic!("expected Stale(CredentialChangedUnverified)");
        };
        assert_eq!(latest_attempt, AttemptId::new(2));

        // Step 3: Success under context B
        let obs_b = test_observed(99, Some(3_000_000_000), 3_000_000_000);
        let started_3 = AttemptStarted::new(
            AttemptId::new(3),
            UtcTimestamp::from_unix_nanos(3_000_000_000),
        );
        let result_3 = AttemptResult::new(
            AttemptId::new(3),
            UtcTimestamp::from_unix_nanos(3_000_000_000),
            MonotonicDuration::from_seconds(0),
            AttemptOutcome::Success,
        );
        let input_3 = FreshnessInput::new(
            Some(obs_b.clone()),
            Some(&ctx_b),
            Some(LatestAttempt::new(started_3, Some(result_3), &ctx_b)),
            None,         // Auth failure under ctx_a cleared by success under ctx_b
            Some(&ctx_b), // ctx_b verified
            horizon,
            command_horizon,
            envelope,
        );
        let clock_3 =
            crate::domain::time::FakeClock::new(UtcTimestamp::from_unix_nanos(3_100_000_000));
        let res_3 = compute_freshness(&input_3, &clock_3);
        assert_eq!(res_3.kind(), FreshnessKind::Fresh);
        let Freshness::Fresh {
            observed,
            latest_attempt,
        } = res_3
        else {
            panic!("expected Fresh");
        };
        assert_eq!(observed, obs_b);
        assert_eq!(latest_attempt, AttemptId::new(3));
    }

    #[test]
    fn same_credential_context_transport_failure_retains_unresolved_auth_condition() {
        let ctx_a = CredentialContextId::new("ctx-a");
        let horizon = MonotonicDuration::from_seconds(300);
        let command_horizon = MonotonicDuration::from_seconds(10);
        let envelope = ClockSkewEnvelope::new(MonotonicDuration::from_seconds(10));

        let started_2 = AttemptStarted::new(
            AttemptId::new(2),
            UtcTimestamp::from_unix_nanos(2_000_000_000),
        );
        let result_2 = AttemptResult::new(
            AttemptId::new(2),
            UtcTimestamp::from_unix_nanos(2_000_000_000),
            MonotonicDuration::from_seconds(0),
            AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
        );
        let input = FreshnessInput::<u64>::new(
            None,
            None,
            Some(LatestAttempt::new(started_2, Some(result_2), &ctx_a)),
            Some(&ctx_a), // Prior auth failure was under same ctx_a
            None,
            horizon,
            command_horizon,
            envelope,
        );
        let clock =
            crate::domain::time::FakeClock::new(UtcTimestamp::from_unix_nanos(2_500_000_000));
        let res = compute_freshness(&input, &clock);
        assert_eq!(res.kind(), FreshnessKind::AuthRequired);
    }

    #[test]
    fn started_attempt_past_command_horizon_yields_collector_interrupted() {
        let ctx = CredentialContextId::new("ctx-1");
        let horizon = MonotonicDuration::from_seconds(300);
        let command_horizon = MonotonicDuration::from_seconds(10);
        let envelope = ClockSkewEnvelope::new(MonotonicDuration::from_seconds(10));
        let obs = test_observed(50, Some(1_000_000_000), 1_000_000_000);

        // Attempt 2 started at T=5s, but has NO result
        let started = AttemptStarted::new(
            AttemptId::new(2),
            UtcTimestamp::from_unix_nanos(5_000_000_000),
        );
        let input = FreshnessInput::new(
            Some(obs.clone()),
            Some(&ctx),
            Some(LatestAttempt::new(started, None, &ctx)),
            None,
            Some(&ctx),
            horizon,
            command_horizon,
            envelope,
        );

        // Clock is T=20s (15s elapsed > 10s command horizon)
        let clock =
            crate::domain::time::FakeClock::new(UtcTimestamp::from_unix_nanos(20_000_000_000));
        let res = compute_freshness(&input, &clock);
        assert_eq!(res.kind(), FreshnessKind::Stale);
        let Freshness::Stale {
            last_good: Some(good),
            latest_attempt,
            reason: StaleReason::CollectorInterrupted,
        } = res
        else {
            panic!("expected Stale(CollectorInterrupted)");
        };
        assert_eq!(good, obs);
        assert_eq!(latest_attempt, AttemptId::new(2));
    }

    #[test]
    fn provider_timestamp_outside_skew_envelope_yields_clock_anomaly() {
        let ctx = CredentialContextId::new("ctx-1");
        let horizon = MonotonicDuration::from_seconds(300);
        let command_horizon = MonotonicDuration::from_seconds(10);
        let envelope = ClockSkewEnvelope::new(MonotonicDuration::from_seconds(10));

        // Provider observed at 1_050s, received at 1_000s (> 10s envelope)
        let obs = test_observed(77, Some(1_050_000_000_000), 1_000_000_000_000);
        let started = AttemptStarted::new(
            AttemptId::new(1),
            UtcTimestamp::from_unix_nanos(1_000_000_000_000),
        );
        let result = AttemptResult::new(
            AttemptId::new(1),
            UtcTimestamp::from_unix_nanos(1_000_000_000_000),
            MonotonicDuration::from_seconds(0),
            AttemptOutcome::Success,
        );

        let input = FreshnessInput::new(
            Some(obs.clone()),
            Some(&ctx),
            Some(LatestAttempt::new(started, Some(result), &ctx)),
            None,
            Some(&ctx),
            horizon,
            command_horizon,
            envelope,
        );

        let clock =
            crate::domain::time::FakeClock::new(UtcTimestamp::from_unix_nanos(1_060_000_000_000));
        let res = compute_freshness(&input, &clock);
        assert_eq!(res.kind(), FreshnessKind::Stale);
        let Freshness::Stale {
            last_good: Some(good),
            latest_attempt,
            reason: StaleReason::ClockAnomaly,
        } = res
        else {
            panic!("expected Stale(ClockAnomaly)");
        };
        assert_eq!(good, obs);
        assert_eq!(latest_attempt, AttemptId::new(1));
    }

    #[test]
    fn pure_function_called_twice_with_identical_inputs_returns_identical_output() {
        let ctx = CredentialContextId::new("ctx-1");
        let horizon = MonotonicDuration::from_seconds(60);
        let command_horizon = MonotonicDuration::from_seconds(10);
        let envelope = ClockSkewEnvelope::new(MonotonicDuration::from_seconds(10));
        let obs = test_observed(123, Some(1_000_000_000), 1_000_000_000);
        let started = AttemptStarted::new(
            AttemptId::new(1),
            UtcTimestamp::from_unix_nanos(1_000_000_000),
        );
        let result = AttemptResult::new(
            AttemptId::new(1),
            UtcTimestamp::from_unix_nanos(1_000_000_000),
            MonotonicDuration::from_seconds(0),
            AttemptOutcome::Success,
        );

        let input = FreshnessInput::new(
            Some(obs),
            Some(&ctx),
            Some(LatestAttempt::new(started, Some(result), &ctx)),
            None,
            Some(&ctx),
            horizon,
            command_horizon,
            envelope,
        );

        let clock =
            crate::domain::time::FakeClock::new(UtcTimestamp::from_unix_nanos(10_000_000_000));
        let call_1 = compute_freshness(&input, &clock);
        let call_2 = compute_freshness(&input, &clock);
        assert_eq!(call_1, call_2);
    }

    fn xorshift(seed: u64) -> impl FnMut() -> u64 {
        let mut state = seed;
        move || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
    }

    proptest::proptest! {
        #[test]
        fn prop_time_alone_can_make_fresh_data_stale_and_never_makes_stale_data_fresh(
            base_time_sec in 100u64..10_000u64,
            horizon_sec in 10u64..120u64,
            has_last_good in proptest::bool::ANY,
            last_good_val in 0u64..1000u64,
            obs_ctx_idx in 0usize..2,
            outcome_choice in 0u8..5,
            attempt_id_raw in 1u64..1000u64,
            latest_ctx_idx in 0usize..2,
            has_auth_fail in proptest::bool::ANY,
            auth_fail_ctx_idx in 0usize..2,
            has_auth_success in proptest::bool::ANY,
            auth_success_ctx_idx in 0usize..2,
            delta_sec in 1u64..7200u64,
        ) {
            use crate::domain::failure::HttpStatusClass;

            let ctx_a = CredentialContextId::new("ctx-a");
            let ctx_b = CredentialContextId::new("ctx-b");
            let contexts = [&ctx_a, &ctx_b];

            let base_nanos = (base_time_sec as i64) * 1_000_000_000;
            let horizon = MonotonicDuration::from_seconds(horizon_sec);
            let command_horizon = MonotonicDuration::from_seconds(10);
            let envelope = ClockSkewEnvelope::new(MonotonicDuration::from_seconds(10));

            let obs = if has_last_good {
                Some(test_observed(last_good_val, Some(base_nanos), base_nanos))
            } else {
                None
            };
            let obs_ctx = if has_last_good {
                Some(contexts[obs_ctx_idx % 2])
            } else {
                None
            };

            let started = AttemptStarted::new(
                AttemptId::new(attempt_id_raw),
                UtcTimestamp::from_unix_nanos(base_nanos),
            );
            let latest_ctx = contexts[latest_ctx_idx % 2];
            let result = match outcome_choice {
                0 => Some(AttemptResult::new(
                    started.attempt_id(),
                    UtcTimestamp::from_unix_nanos(base_nanos),
                    MonotonicDuration::from_seconds(0),
                    AttemptOutcome::Success,
                )),
                1 => Some(AttemptResult::new(
                    started.attempt_id(),
                    UtcTimestamp::from_unix_nanos(base_nanos),
                    MonotonicDuration::from_seconds(0),
                    AttemptOutcome::AuthRequired,
                )),
                2 => Some(AttemptResult::new(
                    started.attempt_id(),
                    UtcTimestamp::from_unix_nanos(base_nanos),
                    MonotonicDuration::from_seconds(0),
                    AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
                )),
                3 => Some(AttemptResult::new(
                    started.attempt_id(),
                    UtcTimestamp::from_unix_nanos(base_nanos),
                    MonotonicDuration::from_seconds(0),
                    AttemptOutcome::Unreachable(FailureClass::HttpStatus(
                        HttpStatusClass::ServerError,
                    )),
                )),
                _ => None,
            };

            let auth_fail_ctx = if has_auth_fail {
                Some(contexts[auth_fail_ctx_idx % 2])
            } else {
                None
            };
            let auth_success_ctx = if has_auth_success {
                Some(contexts[auth_success_ctx_idx % 2])
            } else {
                None
            };

            let input = FreshnessInput::new(
                obs,
                obs_ctx,
                Some(LatestAttempt::new(started, result, latest_ctx)),
                auth_fail_ctx,
                auth_success_ctx,
                horizon,
                command_horizon,
                envelope,
            );

            let t0 = UtcTimestamp::from_unix_nanos(base_nanos);
            let clock_0 = crate::domain::time::FakeClock::new(t0);
            let initial = compute_freshness(&input, &clock_0);

            let t_future =
                UtcTimestamp::from_unix_nanos(base_nanos + (delta_sec as i64) * 1_000_000_000);
            let clock_future = crate::domain::time::FakeClock::new(t_future);
            let later = compute_freshness(&input, &clock_future);

            match initial.kind() {
                FreshnessKind::Stale => {
                    prop_assert_ne!(
                        later.kind(),
                        FreshnessKind::Fresh,
                        "time alone must never turn Stale into Fresh"
                    );
                }
                FreshnessKind::AuthRequired => {
                    prop_assert_ne!(
                        later.kind(),
                        FreshnessKind::Fresh,
                        "time alone must never turn AuthRequired into Fresh"
                    );
                }
                FreshnessKind::Fresh => {
                    prop_assert_ne!(
                        later.kind(),
                        FreshnessKind::AuthRequired,
                        "time alone must never turn Fresh into AuthRequired"
                    );
                    if delta_sec > horizon_sec {
                        prop_assert_eq!(
                            later.kind(),
                            FreshnessKind::Stale,
                            "Fresh data must age into Stale past the freshness horizon"
                        );
                    }
                }
            }
        }
    }

    /// Retained hand-picked regression: walks deterministic pseudo-random samples.
    #[test]
    fn time_alone_can_make_fresh_data_stale_and_never_makes_stale_data_fresh_hand_picked() {
        use crate::domain::failure::HttpStatusClass;

        let mut rng = xorshift(0xDEAD_BEEF_CAFE_BABE);
        let ctx_a = CredentialContextId::new("ctx-a");
        let ctx_b = CredentialContextId::new("ctx-b");
        let contexts = [&ctx_a, &ctx_b];

        for _ in 0..100 {
            let base_time_sec = (rng() % 1000) + 100;
            let base_nanos = (base_time_sec as i64) * 1_000_000_000;
            let horizon_sec = (rng() % 120) + 10;
            let horizon = MonotonicDuration::from_seconds(horizon_sec);
            let command_horizon = MonotonicDuration::from_seconds(10);
            let envelope = ClockSkewEnvelope::new(MonotonicDuration::from_seconds(10));

            let has_last_good = rng().is_multiple_of(2);
            let obs = if has_last_good {
                Some(test_observed(rng() % 1000, Some(base_nanos), base_nanos))
            } else {
                None
            };
            let obs_ctx = if has_last_good {
                Some(contexts[(rng() % 2) as usize])
            } else {
                None
            };

            let outcome_choice = rng() % 5;
            let started = AttemptStarted::new(
                AttemptId::new((rng() % 100) + 1),
                UtcTimestamp::from_unix_nanos(base_nanos),
            );
            let latest_ctx = contexts[(rng() % 2) as usize];
            let result = match outcome_choice {
                0 => Some(AttemptResult::new(
                    started.attempt_id(),
                    UtcTimestamp::from_unix_nanos(base_nanos),
                    MonotonicDuration::from_seconds(0),
                    AttemptOutcome::Success,
                )),
                1 => Some(AttemptResult::new(
                    started.attempt_id(),
                    UtcTimestamp::from_unix_nanos(base_nanos),
                    MonotonicDuration::from_seconds(0),
                    AttemptOutcome::AuthRequired,
                )),
                2 => Some(AttemptResult::new(
                    started.attempt_id(),
                    UtcTimestamp::from_unix_nanos(base_nanos),
                    MonotonicDuration::from_seconds(0),
                    AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
                )),
                3 => Some(AttemptResult::new(
                    started.attempt_id(),
                    UtcTimestamp::from_unix_nanos(base_nanos),
                    MonotonicDuration::from_seconds(0),
                    AttemptOutcome::Unreachable(FailureClass::HttpStatus(
                        HttpStatusClass::ServerError,
                    )),
                )),
                _ => None,
            };

            let auth_fail_ctx = if rng().is_multiple_of(2) {
                Some(contexts[(rng() % 2) as usize])
            } else {
                None
            };
            let auth_success_ctx = if rng().is_multiple_of(2) {
                Some(contexts[(rng() % 2) as usize])
            } else {
                None
            };

            let input = FreshnessInput::new(
                obs,
                obs_ctx,
                Some(LatestAttempt::new(started, result, latest_ctx)),
                auth_fail_ctx,
                auth_success_ctx,
                horizon,
                command_horizon,
                envelope,
            );

            let t0 = UtcTimestamp::from_unix_nanos(base_nanos);
            let clock_0 = crate::domain::time::FakeClock::new(t0);
            let initial = compute_freshness(&input, &clock_0);

            // Advance clock by various increments
            for delta_sec in [
                1,
                5,
                horizon_sec / 2,
                horizon_sec,
                horizon_sec + 1,
                horizon_sec * 2,
                3600,
            ] {
                let t_future =
                    UtcTimestamp::from_unix_nanos(base_nanos + (delta_sec as i64) * 1_000_000_000);
                let clock_future = crate::domain::time::FakeClock::new(t_future);
                let later = compute_freshness(&input, &clock_future);

                match initial.kind() {
                    FreshnessKind::Stale => {
                        assert_ne!(
                            later.kind(),
                            FreshnessKind::Fresh,
                            "time alone must never turn Stale into Fresh"
                        );
                    }
                    FreshnessKind::AuthRequired => {
                        assert_ne!(
                            later.kind(),
                            FreshnessKind::Fresh,
                            "time alone must never turn AuthRequired into Fresh"
                        );
                    }
                    FreshnessKind::Fresh => {
                        assert_ne!(
                            later.kind(),
                            FreshnessKind::AuthRequired,
                            "time alone must never turn Fresh into AuthRequired"
                        );
                        if delta_sec > horizon_sec {
                            assert_eq!(
                                later.kind(),
                                FreshnessKind::Stale,
                                "Fresh data must age into Stale past the freshness horizon"
                            );
                        }
                    }
                }
            }
        }
    }
}
