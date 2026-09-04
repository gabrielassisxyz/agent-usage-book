//! The verdict of comparing one provider adapter's reading against the
//! provider's own authoritative usage surface, and the pure boundary check
//! behind it (aub-eun.12, PLAN.md sections 34.8, 34.30, 45 provider semantic
//! drift).
//!
//! Every other test in this system proves the code does what the code was
//! written to mean. None proves that what it was written to mean is what the
//! provider means, because a fixture is sanitized from a response an adapter
//! already interpreted. Closing that gap is a deliberately manual, recurring
//! comparison against a human-facing page outside this machine. What lives
//! here is only the rule that turns two recorded fractions and the surface's
//! documented granularity into one of exactly two verdicts.
//!
//! The known closed-source 41-percent-versus-70-percent discrepancy is not a
//! tolerance precedent: there is no configurable slack anywhere in this file.
//! The only quantity the verdict depends on beyond the two readings is the
//! surface's own documented granularity.
//!
//! May not depend on:
//! - SQLite, HTTP, or terminal-formatting crates
//! - any adapter, workflow, or presentation layer

use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};

/// The granularity the authoritative surface documents or visibly displays:
/// the smallest difference that surface is able to express. Anthropic's usage
/// page shows whole percentage points, so its documented granularity is
/// 10_000 parts per million.
///
/// This is a property of the surface, recorded once in the validation
/// procedure, never a knob a caller may widen to make a disagreement pass. It
/// is a distinct newtype from [`crate::domain::window::ReportedResolution`],
/// which is the resolution one provider *response* claimed for itself: the two
/// are different quantities and there is no conversion between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentedGranularity(QuotaFractionPpm);

impl DocumentedGranularity {
    /// A granularity of `ppm` parts per million. A zero granularity means the
    /// surface expresses an exact fraction and any difference at all is a
    /// mismatch; a caller that means that passes `QuotaFractionPpm::new(0)`.
    pub fn new(ppm: QuotaFractionPpm) -> Self {
        Self(ppm)
    }

    pub fn as_ppm(self) -> QuotaFractionPpm {
        self.0
    }
}

/// The verdict of one comparison. Exactly two outcomes exist by construction:
/// there is no "unknown", no "within tolerance", and no third arm a tolerance
/// parameter could reach. A difference is either inside the surface's own
/// documented granularity, or it is an unresolved mismatch a human must
/// explain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthoritativeComparisonVerdict {
    /// The adapter's reading and the authoritative surface differ by no more
    /// than the surface's documented granularity.
    AgreesWithinGranularity,
    /// The two disagree by more than the documented granularity. This is a
    /// finding, not a note: it names the window, both values and the
    /// observation, and stays open until a human explains it.
    UnresolvedMismatch,
}

impl AuthoritativeComparisonVerdict {
    /// The stable wire spelling stored in the database. One definition here.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AgreesWithinGranularity => "agrees_within_granularity",
            Self::UnresolvedMismatch => "unresolved_mismatch",
        }
    }

    /// Parses the stable wire spelling back, rejecting anything that is not one
    /// of the two known outcomes.
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "agrees_within_granularity" => Some(Self::AgreesWithinGranularity),
            "unresolved_mismatch" => Some(Self::UnresolvedMismatch),
            _ => None,
        }
    }
}

/// Compares one adapter reading against one authoritative-surface reading of
/// the same window. The verdict is agreement exactly when the absolute
/// difference in parts per million is at or below the documented granularity;
/// one part per million beyond it is a mismatch.
pub fn compare_against_authoritative_surface(
    adapter_reading: QuotaUsed,
    authoritative_reading: QuotaUsed,
    documented_granularity: DocumentedGranularity,
) -> AuthoritativeComparisonVerdict {
    let adapter_ppm = i64::from(adapter_reading.as_ppm().get());
    let authoritative_ppm = i64::from(authoritative_reading.as_ppm().get());
    let difference = (adapter_ppm - authoritative_ppm).unsigned_abs();
    if difference <= u64::from(documented_granularity.as_ppm().get()) {
        AuthoritativeComparisonVerdict::AgreesWithinGranularity
    } else {
        AuthoritativeComparisonVerdict::UnresolvedMismatch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn used(ppm: i32) -> QuotaUsed {
        QuotaUsed::new(QuotaFractionPpm::new(ppm).expect("test ppm in range"))
    }

    fn granularity(ppm: i32) -> DocumentedGranularity {
        DocumentedGranularity::new(QuotaFractionPpm::new(ppm).expect("test ppm in range"))
    }

    /// The verdict enum has exactly two outcomes and the wire spelling round
    /// trips. The planted negative: `from_code` rejects any third spelling, so
    /// adding a `WithinTolerance` (or any other) arm and teaching `from_code`
    /// to accept it fails this assertion.
    #[test]
    fn the_verdict_has_exactly_two_outcomes_and_no_third() {
        let all = [
            AuthoritativeComparisonVerdict::AgreesWithinGranularity,
            AuthoritativeComparisonVerdict::UnresolvedMismatch,
        ];
        for verdict in all {
            assert_eq!(
                AuthoritativeComparisonVerdict::from_code(verdict.as_str()),
                Some(verdict)
            );
        }
        for third in ["within_tolerance", "unknown", "agrees", "mismatch", ""] {
            assert_eq!(
                AuthoritativeComparisonVerdict::from_code(third),
                None,
                "{third:?} is not one of the two outcomes"
            );
        }
    }

    /// A difference inside the documented granularity is agreement, and a
    /// difference exactly one part per million outside it is a mismatch, so the
    /// boundary is tested rather than assumed.
    #[test]
    fn the_granularity_boundary_is_exact() {
        let g = granularity(10_000);

        // Zero difference: agreement.
        assert_eq!(
            compare_against_authoritative_surface(used(410_000), used(410_000), g),
            AuthoritativeComparisonVerdict::AgreesWithinGranularity
        );
        // Exactly at the granularity, in each direction: agreement.
        assert_eq!(
            compare_against_authoritative_surface(used(420_000), used(410_000), g),
            AuthoritativeComparisonVerdict::AgreesWithinGranularity
        );
        assert_eq!(
            compare_against_authoritative_surface(used(400_000), used(410_000), g),
            AuthoritativeComparisonVerdict::AgreesWithinGranularity
        );
        // One part per million past the granularity, in each direction: mismatch.
        assert_eq!(
            compare_against_authoritative_surface(used(420_001), used(410_000), g),
            AuthoritativeComparisonVerdict::UnresolvedMismatch
        );
        assert_eq!(
            compare_against_authoritative_surface(used(399_999), used(410_000), g),
            AuthoritativeComparisonVerdict::UnresolvedMismatch
        );
    }

    /// The 41-versus-70 discrepancy: a 29 percentage-point gap is a mismatch
    /// under any sane display granularity, and no granularity a caller could
    /// pass short of 290_000 ppm turns it into agreement.
    #[test]
    fn a_large_discrepancy_is_never_silently_agreement() {
        let verdict = compare_against_authoritative_surface(
            used(410_000),
            used(700_000),
            granularity(10_000),
        );
        assert_eq!(verdict, AuthoritativeComparisonVerdict::UnresolvedMismatch);
    }

    /// A zero granularity means the surface is exact: any nonzero difference is
    /// a mismatch.
    #[test]
    fn a_zero_granularity_admits_no_difference() {
        assert_eq!(
            compare_against_authoritative_surface(used(410_001), used(410_000), granularity(0)),
            AuthoritativeComparisonVerdict::UnresolvedMismatch
        );
        assert_eq!(
            compare_against_authoritative_surface(used(410_000), used(410_000), granularity(0)),
            AuthoritativeComparisonVerdict::AgreesWithinGranularity
        );
    }
}
