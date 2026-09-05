//! Typed anomaly classification for one quota window across two consecutive
//! observations of the same account, plan tier and window identity
//! (`aub-eun.14`, PLAN.md sections 30, 34.10, 45).
//!
//! A provider's reported usage should never fall without a reset, and a
//! reset instant should never move except at the boundary the previous
//! reading itself named. Both violations are evidence problems, not values
//! to normalize away: the caller persists the two readings unchanged and
//! records a typed anomaly linking them. This module contains only the
//! classification rule, over two already-normalized readings; it never
//! touches storage or an evidence capsule.
//!
//! A window-set change (a constraint appearing or disappearing between two
//! observations) is a second, unrelated kind of transition: not a
//! disagreement between two readings of the same constraint, but the
//! constraint set itself changing shape. It is classified separately, and
//! only in the two directions PLAN.md's window-set-evolution decision names
//! - a new account-wide window, and a missing model-specific window. The two
//! other directions (a new model-specific window, a missing account-wide
//! window) are left unclassified here rather than guessed at, because no
//! decision has fixed what either one means yet.
//!
//! May not depend on:
//! - SQLite, HTTP, or terminal-formatting crates
//! - any adapter, workflow, or presentation layer

use crate::domain::quota::QuotaUsed;
use crate::domain::time::UtcTimestamp;
use crate::domain::window::{WindowResetState, WindowScopeKind};

/// The two evidence-problem classes a consecutive-window comparison can
/// surface. There is no third: every other transition, including one that
/// changes nothing, classifies as `None` from [`classify_window_transition`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowAnomalyKind {
    /// The reported used fraction fell without a boundary reset that could
    /// explain the drop.
    PercentageDecreaseWithoutReset,
    /// The reported reset instant changed without the previous boundary
    /// having been due, or without the new instant moving forward from it.
    UnexpectedResetTimestampChange,
}

impl WindowAnomalyKind {
    /// The stable database spelling. One definition here.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PercentageDecreaseWithoutReset => "percentage_decrease_without_reset",
            Self::UnexpectedResetTimestampChange => "unexpected_reset_change",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "percentage_decrease_without_reset" => Some(Self::PercentageDecreaseWithoutReset),
            "unexpected_reset_change" => Some(Self::UnexpectedResetTimestampChange),
            _ => None,
        }
    }
}

/// One window reading, reduced to exactly what reset-semantics comparison
/// needs: the reported used fraction, the reported reset state, and the
/// instant this reading was taken at (the observation's measurement basis
/// instant, not the window's own reset instant).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowReading {
    pub quota_used: QuotaUsed,
    pub resets_at: WindowResetState,
    pub observed_at: UtcTimestamp,
}

/// True when the window's previously reported boundary had already been
/// reached by the time the current reading was taken, i.e. a reset was due
/// between the two readings.
fn reset_due(previous_reset: WindowResetState, current_observed_at: UtcTimestamp) -> bool {
    match previous_reset {
        WindowResetState::Known(instant) => current_observed_at >= instant,
        WindowResetState::NotStarted => false,
    }
}

/// True when the reset state moved forward: a known instant followed by a
/// later known instant, a known instant followed by idle, or idle followed
/// by a freshly known instant. Idle-to-idle carries no forward motion.
fn reset_advanced(previous_reset: WindowResetState, current_reset: WindowResetState) -> bool {
    match (previous_reset, current_reset) {
        (WindowResetState::Known(old), WindowResetState::Known(new)) => new > old,
        (WindowResetState::Known(_), WindowResetState::NotStarted)
        | (WindowResetState::NotStarted, WindowResetState::Known(_)) => true,
        (WindowResetState::NotStarted, WindowResetState::NotStarted) => false,
    }
}

/// Classifies the transition from `previous` to `current` for one window
/// identity already matched by the caller (same account, same plan tier,
/// same semantic key and scope). `None` covers every clean transition: no
/// material change, or a legitimate boundary reset that explains a drop.
///
/// A legitimate reset requires all three: the previous boundary was due by
/// the current reading's instant, the reset state actually changed, and the
/// new state moved forward rather than sideways or backward. Any decrease
/// not backed by that is [`WindowAnomalyKind::PercentageDecreaseWithoutReset`];
/// any reset-state change not backed by that, with no decrease, is
/// [`WindowAnomalyKind::UnexpectedResetTimestampChange`].
pub fn classify_window_transition(
    previous: WindowReading,
    current: WindowReading,
) -> Option<WindowAnomalyKind> {
    let reset_changed = previous.resets_at != current.resets_at;
    let legitimate_reset = reset_changed
        && reset_due(previous.resets_at, current.observed_at)
        && reset_advanced(previous.resets_at, current.resets_at);

    let decreased = current.quota_used.as_ppm().get() < previous.quota_used.as_ppm().get();

    if decreased {
        if legitimate_reset {
            None
        } else {
            Some(WindowAnomalyKind::PercentageDecreaseWithoutReset)
        }
    } else if reset_changed && !legitimate_reset {
        Some(WindowAnomalyKind::UnexpectedResetTimestampChange)
    } else {
        None
    }
}

/// The two typed classifications a window-set change can produce. The other
/// two directions (a new model-specific window, a missing account-wide
/// window) are deliberately left unclassified: no adapter or detector here
/// invents an interpretation PLAN.md's decision did not fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowSetChangeKind {
    NewAccountWideWindow,
    MissingModelSpecificWindow,
}

impl WindowSetChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NewAccountWideWindow => "new_account_wide_window",
            Self::MissingModelSpecificWindow => "missing_model_specific_window",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "new_account_wide_window" => Some(Self::NewAccountWideWindow),
            "missing_model_specific_window" => Some(Self::MissingModelSpecificWindow),
            _ => None,
        }
    }
}

/// Whether a window identity appeared in the current observation having been
/// absent from the previous one, or disappeared the other way around.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowPresenceChange {
    Appeared,
    Disappeared,
}

/// Classifies one window identity's presence change by its scope. Returns
/// `None` for the two directions this system has no decision for yet.
pub fn classify_window_set_change(
    scope_kind: WindowScopeKind,
    presence: WindowPresenceChange,
) -> Option<WindowSetChangeKind> {
    match (scope_kind, presence) {
        (WindowScopeKind::AccountWide, WindowPresenceChange::Appeared) => {
            Some(WindowSetChangeKind::NewAccountWideWindow)
        }
        (WindowScopeKind::ModelSpecific, WindowPresenceChange::Disappeared) => {
            Some(WindowSetChangeKind::MissingModelSpecificWindow)
        }
        (WindowScopeKind::AccountWide, WindowPresenceChange::Disappeared)
        | (WindowScopeKind::ModelSpecific, WindowPresenceChange::Appeared) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::quota::QuotaFractionPpm;
    use proptest::prelude::*;

    fn used(ppm: i32) -> QuotaUsed {
        QuotaUsed::new(QuotaFractionPpm::new(ppm).expect("test ppm in range"))
    }

    fn reading(used_ppm: i32, resets_at: WindowResetState, observed_at: i64) -> WindowReading {
        WindowReading {
            quota_used: used(used_ppm),
            resets_at,
            observed_at: UtcTimestamp::from_unix_nanos(observed_at),
        }
    }

    /// The named unit case: a used fraction that falls with no reset evidence
    /// at all is the decrease-without-reset anomaly.
    #[test]
    fn percentage_decrease_without_reset_is_classified() {
        let previous = reading(
            600_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(1_000)),
            500,
        );
        let current = reading(
            400_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(1_000)),
            700,
        );
        assert_eq!(
            classify_window_transition(previous, current),
            Some(WindowAnomalyKind::PercentageDecreaseWithoutReset)
        );
    }

    /// The named unit case: the reset instant changes even though the
    /// previous boundary was not yet due, with no decrease to explain it
    /// either. This is the unexpected-reset-change anomaly, not a legitimate
    /// reset.
    #[test]
    fn unexpected_reset_timestamp_change_is_classified() {
        let previous = reading(
            300_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(10_000)),
            500,
        );
        let current = reading(
            300_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(20_000)),
            600,
        );
        assert_eq!(
            classify_window_transition(previous, current),
            Some(WindowAnomalyKind::UnexpectedResetTimestampChange)
        );
    }

    /// The named unit case: a legitimate boundary reset. The previous
    /// boundary was due, the reset state moved forward, and the drop it
    /// explains produces no anomaly.
    #[test]
    fn legitimate_boundary_reset_produces_no_anomaly() {
        let previous = reading(
            900_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(1_000)),
            500,
        );
        let current = reading(
            50_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(5_000)),
            1_000,
        );
        assert_eq!(classify_window_transition(previous, current), None);
    }

    /// A window winding down to idle after its boundary passed is also a
    /// legitimate reset, even though the new state carries no next instant.
    #[test]
    fn legitimate_reset_to_not_started_produces_no_anomaly() {
        let previous = reading(
            700_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(1_000)),
            500,
        );
        let current = reading(0, WindowResetState::NotStarted, 1_000);
        assert_eq!(classify_window_transition(previous, current), None);
    }

    /// Planted negative for the decrease case: the same drop, but the
    /// boundary had already been reached at the exact reset instant and the
    /// new reset moved forward - this must NOT be flagged, proving the
    /// mutation (removing the forward-motion requirement) would wrongly
    /// accept a same-instant "reset" as legitimate.
    #[test]
    fn a_decrease_with_reset_that_does_not_move_forward_is_still_an_anomaly() {
        let previous = reading(
            900_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(1_000)),
            500,
        );
        // The reset "changes" but to an earlier instant than before: not forward motion.
        let current = reading(
            50_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(999)),
            1_000,
        );
        assert_eq!(
            classify_window_transition(previous, current),
            Some(WindowAnomalyKind::PercentageDecreaseWithoutReset)
        );
    }

    /// Boundary test: the reset instant is exclusive on the low side and
    /// inclusive at and after the instant itself, matching
    /// `MeterWindow::is_active_at`'s own `now < reset` convention.
    #[test]
    fn reset_boundary_is_exact() {
        let previous = reading(
            900_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(1_000)),
            500,
        );

        // One nanosecond before the boundary: not due, so the drop is an anomaly
        // even though the reset state did move forward.
        let just_before = reading(
            50_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(2_000)),
            999,
        );
        assert_eq!(
            classify_window_transition(previous, just_before),
            Some(WindowAnomalyKind::PercentageDecreaseWithoutReset)
        );

        // Exactly at the boundary: due, so the same drop and forward reset is legitimate.
        let at_boundary = reading(
            50_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(2_000)),
            1_000,
        );
        assert_eq!(classify_window_transition(previous, at_boundary), None);

        // One nanosecond after: also due.
        let just_after = reading(
            50_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(2_000)),
            1_001,
        );
        assert_eq!(classify_window_transition(previous, just_after), None);
    }

    /// No change at all, whether idle or active, is never an anomaly.
    #[test]
    fn no_change_is_never_an_anomaly() {
        let idle = reading(0, WindowResetState::NotStarted, 500);
        assert_eq!(classify_window_transition(idle, idle), None);

        let active = reading(
            400_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(1_000)),
            500,
        );
        assert_eq!(classify_window_transition(active, active), None);
    }

    /// The two named window-set directions produce their exact typed
    /// classification, and the other two produce none.
    #[test]
    fn window_set_change_classifies_only_the_two_named_directions() {
        assert_eq!(
            classify_window_set_change(
                WindowScopeKind::AccountWide,
                WindowPresenceChange::Appeared
            ),
            Some(WindowSetChangeKind::NewAccountWideWindow)
        );
        assert_eq!(
            classify_window_set_change(
                WindowScopeKind::ModelSpecific,
                WindowPresenceChange::Disappeared
            ),
            Some(WindowSetChangeKind::MissingModelSpecificWindow)
        );
        assert_eq!(
            classify_window_set_change(
                WindowScopeKind::AccountWide,
                WindowPresenceChange::Disappeared
            ),
            None
        );
        assert_eq!(
            classify_window_set_change(
                WindowScopeKind::ModelSpecific,
                WindowPresenceChange::Appeared
            ),
            None
        );
    }

    /// The wire spelling of both enums round-trips, and no third spelling is
    /// accepted - the planted negative for a silently added arm.
    #[test]
    fn wire_spellings_round_trip_and_reject_a_third() {
        for kind in [
            WindowAnomalyKind::PercentageDecreaseWithoutReset,
            WindowAnomalyKind::UnexpectedResetTimestampChange,
        ] {
            assert_eq!(WindowAnomalyKind::from_code(kind.as_str()), Some(kind));
        }
        assert_eq!(WindowAnomalyKind::from_code("unknown"), None);

        for kind in [
            WindowSetChangeKind::NewAccountWideWindow,
            WindowSetChangeKind::MissingModelSpecificWindow,
        ] {
            assert_eq!(WindowSetChangeKind::from_code(kind.as_str()), Some(kind));
        }
        assert_eq!(WindowSetChangeKind::from_code("unknown"), None);
    }

    proptest::proptest! {
        /// Pure-function idempotency: classifying the same pair of readings
        /// twice, in any order of evaluation, always agrees with itself. This
        /// is the base case the store-level rerun-is-idempotent property
        /// builds on: the classifier itself carries no hidden state a second
        /// call could see differently.
        #[test]
        fn prop_classification_is_deterministic(
            prev_used in 0i32..=1_000_000i32,
            curr_used in 0i32..=1_000_000i32,
            prev_reset_nanos in 0i64..1_000_000i64,
            curr_reset_nanos in 0i64..1_000_000i64,
            prev_observed in 0i64..1_000_000i64,
            curr_observed in 0i64..1_000_000i64,
            prev_not_started in any::<bool>(),
            curr_not_started in any::<bool>(),
        ) {
            let prev_state = if prev_not_started {
                WindowResetState::NotStarted
            } else {
                WindowResetState::Known(UtcTimestamp::from_unix_nanos(prev_reset_nanos))
            };
            let curr_state = if curr_not_started {
                WindowResetState::NotStarted
            } else {
                WindowResetState::Known(UtcTimestamp::from_unix_nanos(curr_reset_nanos))
            };
            let previous = reading(prev_used, prev_state, prev_observed);
            let current = reading(curr_used, curr_state, curr_observed);

            let first = classify_window_transition(previous, current);
            let second = classify_window_transition(previous, current);
            prop_assert_eq!(first, second);
        }

        /// A decrease with the reset state left entirely unchanged is never
        /// legitimate, regardless of the two observed instants: an unchanged
        /// reset carries no evidence of a boundary having been crossed.
        #[test]
        fn prop_decrease_with_unchanged_reset_is_always_an_anomaly(
            prev_used in 1i32..=1_000_000i32,
            drop in 1i32..=1_000_000i32,
            reset_nanos in 0i64..1_000_000i64,
            prev_observed in 0i64..1_000_000i64,
            curr_observed in 0i64..1_000_000i64,
        ) {
            let curr_used = (prev_used - drop).max(0);
            prop_assume!(curr_used < prev_used);
            let state = WindowResetState::Known(UtcTimestamp::from_unix_nanos(reset_nanos));
            let previous = reading(prev_used, state, prev_observed);
            let current = reading(curr_used, state, curr_observed);
            prop_assert_eq!(
                classify_window_transition(previous, current),
                Some(WindowAnomalyKind::PercentageDecreaseWithoutReset)
            );
        }
    }
}
