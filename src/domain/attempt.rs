//! The collection-attempt lifecycle: what was persisted about one attempt to reach a
//! provider, recorded as a two-stage append-only sequence rather than a single row an
//! outcome could arrive into after the fact.
//!
//! An outcome written only after the network returns is an outcome a crash can erase.
//! `AttemptStarted` is durable before any network I/O begins; `AttemptResult` is a
//! separate, later fact. A start with no result past the command's maximum execution
//! horizon reads as collector interruption, not as "no attempt occurred" and never as a
//! network timeout it never actually observed.

use super::failure::FailureClass;
use super::time::{MonotonicDuration, UtcTimestamp};

/// Identifies one collection attempt across its two-stage lifecycle, correlating a
/// start with its eventual result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AttemptId(u64);

impl AttemptId {
    /// Constructs from a raw sequence value (the attempt row's identity in storage).
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw sequence value.
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// What was actually persisted about one collection attempt: success, a credential
/// problem, or an unreachable source with a failure class.
///
/// A separate type from [`super::freshness::Freshness`] by design, with no `From` in
/// either direction: what was recorded about an attempt and what a user is told about
/// freshness are reconstructed from attempt history at read time by a state machine
/// that lives in a separate bead (the synthetic-sampler epic), not derived by a type
/// conversion here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttemptOutcome {
    Success,
    AuthRequired,
    Unreachable(FailureClass),
}

/// Why an account was due for an attempt: the four-value vocabulary the due
/// decision produces and the attempt persists (`aub-me5.3`, PLAN.md 14.4).
/// One definition, in the domain, because the decision that mints it (meter)
/// and the persistence that spells it for storage (store) are different layers
/// that must never grow private copies of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DueReason {
    /// The account's ordinary sampling interval expired.
    OrdinaryCadence,
    /// A known reset is approaching within the configured edge lead and no
    /// sufficiently recent pre-reset sample exists.
    ResetEdge,
    /// A known reset has passed and no attempt has observed the post-reset
    /// state yet.
    PostResetConfirmation,
    /// An explicit operator or hook request, due regardless of history.
    ForcedOrManual,
}

/// Durable before any network I/O begins.
///
/// Absence of a terminal [`AttemptResult`] past the command's maximum execution
/// horizon means the collector was interrupted. That is never rewritten as a network
/// timeout, and never as "no attempt occurred": the start itself is evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttemptStarted {
    attempt_id: AttemptId,
    started_at: UtcTimestamp,
}

impl AttemptStarted {
    pub const fn new(attempt_id: AttemptId, started_at: UtcTimestamp) -> Self {
        Self {
            attempt_id,
            started_at,
        }
    }

    pub const fn attempt_id(self) -> AttemptId {
        self.attempt_id
    }

    pub const fn started_at(self) -> UtcTimestamp {
        self.started_at
    }
}

/// The terminal fact about an attempt, written after the network returns (or the
/// command gives up). A separate type from [`AttemptStarted`]: nothing models the
/// outcome as a nullable field on the start, which is exactly the shape that would let
/// a crash between start and result look identical to an attempt that never began.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttemptResult {
    attempt_id: AttemptId,
    finished_at: UtcTimestamp,
    elapsed: MonotonicDuration,
    outcome: AttemptOutcome,
}

impl AttemptResult {
    pub const fn new(
        attempt_id: AttemptId,
        finished_at: UtcTimestamp,
        elapsed: MonotonicDuration,
        outcome: AttemptOutcome,
    ) -> Self {
        Self {
            attempt_id,
            finished_at,
            elapsed,
            outcome,
        }
    }

    pub const fn attempt_id(self) -> AttemptId {
        self.attempt_id
    }

    pub const fn finished_at(self) -> UtcTimestamp {
        self.finished_at
    }

    pub const fn elapsed(self) -> MonotonicDuration {
        self.elapsed
    }

    pub const fn outcome(self) -> AttemptOutcome {
        self.outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempt_id_round_trips_its_value() {
        assert_eq!(AttemptId::new(42).value(), 42);
    }

    #[test]
    fn started_and_result_are_separate_types_correlated_by_attempt_id() {
        let id = AttemptId::new(7);
        let started = AttemptStarted::new(id, UtcTimestamp::from_unix_nanos(1_000));
        let result = AttemptResult::new(
            id,
            UtcTimestamp::from_unix_nanos(2_000),
            MonotonicDuration::from_nanos(1_000),
            AttemptOutcome::Success,
        );

        assert_eq!(started.attempt_id(), result.attempt_id());
        // A result exists independently of the start it correlates with; nothing
        // requires one to hold a reference to the other's full value.
        assert_eq!(result.outcome(), AttemptOutcome::Success);
    }

    #[test]
    fn a_started_attempt_with_no_result_is_representable() {
        // The whole point of the two-stage lifecycle: a start can exist on its own,
        // and that state (collector interruption) is not modeled as any AttemptOutcome
        // variant, including Unreachable - it is the ABSENCE of an AttemptResult for a
        // given AttemptId, which this type system does not try to represent as a third
        // party's data structure (that reconstruction is the freshness state machine's
        // job, in a separate bead).
        let started = AttemptStarted::new(AttemptId::new(1), UtcTimestamp::from_unix_nanos(0));
        assert_eq!(started.attempt_id(), AttemptId::new(1));
    }

    #[test]
    fn debug_output_never_names_an_is_fresh_or_is_stale_field() {
        let outcomes = [
            AttemptOutcome::Success,
            AttemptOutcome::AuthRequired,
            AttemptOutcome::Unreachable(FailureClass::ConnectTimeout),
            AttemptOutcome::Unreachable(FailureClass::MalformedBody),
            AttemptOutcome::Unreachable(FailureClass::DnsFailure),
        ];
        for outcome in outcomes {
            let rendered = format!("{outcome:?}").to_lowercase();
            assert!(!rendered.contains("is_fresh"), "{rendered}");
            assert!(!rendered.contains("is_stale"), "{rendered}");
        }
    }
}
