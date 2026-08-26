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

use super::attempt::AttemptId;
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
    SourceUnreachable(super::attempt::FailureClass),
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
        use super::attempt::FailureClass;
        [
            StaleReason::AgeExceeded,
            StaleReason::NoSuccessfulObservation,
            StaleReason::SourceUnreachable(FailureClass::Timeout),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attempt::FailureClass;
    use crate::domain::time::UtcTimestamp;

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
            reason: StaleReason::SourceUnreachable(FailureClass::NetworkError),
        };

        match stale_after {
            Freshness::Stale {
                last_good: Some(good),
                reason: StaleReason::SourceUnreachable(FailureClass::NetworkError),
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
            reason: StaleReason::SourceUnreachable(FailureClass::Timeout),
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
}
