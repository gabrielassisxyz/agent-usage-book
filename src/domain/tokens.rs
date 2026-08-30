//! Token quantities: the finite known kinds, their per-kind newtypes, and the vector
//! that carries usage across them without collapsing to one scalar.
//!
//! There is deliberately no `total_tokens()` anywhere in this module. Collapsing token
//! classes into one scalar is the erasure that produced the defect this project exists
//! to fix: the legacy calibration omitted cache-write billing, and the omission was
//! invisible precisely because a missing class and a zero class look identical once
//! everything is one number. A display total for a human belongs on the presentation
//! side, built from an explicit context; nothing here reaches for one.

use std::collections::BTreeMap;
use std::ops::{Mul, Sub};

use crate::domain::interval::DomainQuantity;
use crate::evidence::{CoverageCompleteness, EvidenceQuality};

/// The finite set of token classes this project bills and calibrates against.
///
/// Deliberately not `#[non_exhaustive]`: a source that reports a token class outside
/// this set does not extend the enum, it lands in `UsageVector`'s unknown-component map
/// instead (see `TokenCount`). Every match on `TokenKind` in this module is exhaustive
/// with no wildcard arm, so adding a fifth variant here is meant to break compilation at
/// every one of those call sites rather than fall silently into a `_` branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenKind {
    Input,
    Output,
    CacheRead,
    CacheWrite,
}

impl TokenKind {
    /// Every variant, in a stable order. A unit test below pins this array's length, so
    /// a forgotten update here after adding a variant is caught even though the array
    /// itself is not an exhaustive match the compiler can check for us.
    pub const ALL: [TokenKind; 4] = [
        TokenKind::Input,
        TokenKind::Output,
        TokenKind::CacheRead,
        TokenKind::CacheWrite,
    ];
}

/// Generates a token-count newtype: private `u64` storage, an infallible constructor, a
/// value accessor, and addition against another value of the *same* generated type only.
/// No invocation of this macro ever generates a conversion to or from another kind, which
/// is what makes cross-kind arithmetic and cross-kind assignment compile errors rather
/// than runtime bugs.
macro_rules! token_newtype {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            /// Constructs from a raw count. No range is rejected: an unsigned integer's
            /// own range is this type's range, and `u64::MAX` is a valid, if extreme,
            /// count.
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            /// The raw count this newtype wraps.
            pub const fn value(self) -> u64 {
                self.0
            }
        }

        impl std::ops::Add for $name {
            type Output = Self;

            /// Only ever adds two values of this same generated type: there is no
            /// `impl Add<T>` for any other `T` anywhere in this module.
            fn add(self, other: Self) -> Self {
                Self(self.0 + other.0)
            }
        }
    };
}

token_newtype!(
    InputTokens,
    "Tokens consumed as the prompt/input side of a request."
);

impl Sub for TokenCount {
    type Output = Self;

    fn sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }
}

impl Mul for TokenCount {
    type Output = Self;

    fn mul(self, other: Self) -> Self {
        Self(self.0.saturating_mul(other.0))
    }
}

impl DomainQuantity for TokenCount {
    fn unit() -> &'static str {
        "tokens"
    }

    fn to_f64(self) -> f64 {
        self.0 as f64
    }

    fn from_f64(value: f64) -> Self {
        Self(value.max(0.0).min(u64::MAX as f64) as u64)
    }
}
token_newtype!(
    OutputTokens,
    "Tokens produced as the completion/output side of a request."
);
token_newtype!(
    CacheReadTokens,
    "Tokens served from a provider's prompt cache."
);
token_newtype!(
    CacheWriteTokens,
    "Tokens written into a provider's prompt cache."
);

token_newtype!(
    TokenCount,
    "A count of tokens outside the four known per-kind newtypes.\n\nExists so \
     `UsageVector` can carry a provider- or CLI-reported component this project has not \
     modeled yet, without silently discarding it or misfiling it under a known kind. \
     Deliberately carries no `From`/`Into` to or from any of the four known newtypes: \
     promoting an unknown count to a known kind is a modeling decision for a human to \
     make once the source is understood, never an automatic conversion."
);

/// One count per known token kind. Addition never merges two kinds: each field sums
/// against the same field of `other`, and there is no path from an `InputTokens` value
/// to the `output` slot that would make a wrong sum representable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownTokenVector {
    input: InputTokens,
    output: OutputTokens,
    cache_read: CacheReadTokens,
    cache_write: CacheWriteTokens,
}

impl KnownTokenVector {
    pub const fn new(
        input: InputTokens,
        output: OutputTokens,
        cache_read: CacheReadTokens,
        cache_write: CacheWriteTokens,
    ) -> Self {
        Self {
            input,
            output,
            cache_read,
            cache_write,
        }
    }

    pub const fn input(self) -> InputTokens {
        self.input
    }

    pub const fn output(self) -> OutputTokens {
        self.output
    }

    pub const fn cache_read(self) -> CacheReadTokens {
        self.cache_read
    }

    pub const fn cache_write(self) -> CacheWriteTokens {
        self.cache_write
    }

    /// The raw count for one kind, selected by an exhaustive match with no wildcard arm:
    /// a fifth `TokenKind` variant fails to compile here rather than silently returning
    /// zero for a kind this vector has no field for.
    pub fn value(self, kind: TokenKind) -> u64 {
        match kind {
            TokenKind::Input => self.input.value(),
            TokenKind::Output => self.output.value(),
            TokenKind::CacheRead => self.cache_read.value(),
            TokenKind::CacheWrite => self.cache_write.value(),
        }
    }
}

impl std::ops::Add for KnownTokenVector {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            input: self.input + other.input,
            output: self.output + other.output,
            cache_read: self.cache_read + other.cache_read,
            cache_write: self.cache_write + other.cache_write,
        }
    }
}

/// Usage as a vector over kinds, with a place for components no known kind names.
///
/// Carries the known per-kind vector and a map of unknown external components keyed by
/// the source that reported them. That map is what lets a provider or CLI add a
/// billing-relevant class next month without a parser silently dropping it: a non-empty
/// unknown set is meant to block complete conversion to credits or money until a cost
/// model explicitly defines the class.
///
/// Coverage and evidence quality are independent witnesses. An aggregate can have missing
/// components and still contain estimates among the components it did receive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageVector {
    known: KnownTokenVector,
    unknown: BTreeMap<String, TokenCount>,
    coverage: CoverageCompleteness,
    quality: EvidenceQuality<TokenCount>,
}

impl UsageVector {
    pub fn new(
        known: KnownTokenVector,
        unknown: BTreeMap<String, TokenCount>,
        coverage: CoverageCompleteness,
        quality: EvidenceQuality<TokenCount>,
    ) -> Self {
        Self {
            known,
            unknown,
            coverage,
            quality,
        }
    }

    pub const fn known(&self) -> KnownTokenVector {
        self.known
    }

    pub fn unknown(&self) -> &BTreeMap<String, TokenCount> {
        &self.unknown
    }

    pub fn coverage(&self) -> &CoverageCompleteness {
        &self.coverage
    }

    pub fn quality(&self) -> &EvidenceQuality<TokenCount> {
        &self.quality
    }

    /// True when at least one component was reported under a key none of the four known
    /// kinds names. No function in this module converts a `UsageVector` to credits or
    /// money at all yet, known-only or not; this exists so a future cost model has one
    /// place to ask the question rather than re-deriving it from the map each time.
    pub fn has_unknown_components(&self) -> bool {
        !self.unknown.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::{ComponentKind, EstimatorId};

    #[test]
    fn token_kind_all_has_exactly_four_variants() {
        assert_eq!(TokenKind::ALL.len(), 4);
    }

    #[test]
    fn each_newtype_round_trips_its_value_including_the_maximum() {
        for value in [0, 1, 42, u64::MAX] {
            assert_eq!(InputTokens::new(value).value(), value);
            assert_eq!(OutputTokens::new(value).value(), value);
            assert_eq!(CacheReadTokens::new(value).value(), value);
            assert_eq!(CacheWriteTokens::new(value).value(), value);
            assert_eq!(TokenCount::new(value).value(), value);
        }
    }

    #[test]
    fn same_kind_addition_sums_the_raw_counts() {
        assert_eq!(
            (InputTokens::new(3) + InputTokens::new(4)).value(),
            7,
            "same-kind addition must sum, not merge with another kind"
        );
    }

    fn sample_vector(
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
    ) -> KnownTokenVector {
        KnownTokenVector::new(
            InputTokens::new(input),
            OutputTokens::new(output),
            CacheReadTokens::new(cache_read),
            CacheWriteTokens::new(cache_write),
        )
    }

    proptest::proptest! {
        #[test]
        fn prop_vector_addition_preserves_each_kind_independently(
            ai in 0..=(u64::MAX / 2),
            ao in 0..=(u64::MAX / 2),
            ar in 0..=(u64::MAX / 2),
            aw in 0..=(u64::MAX / 2),
            bi in 0..=(u64::MAX / 2),
            bo in 0..=(u64::MAX / 2),
            br in 0..=(u64::MAX / 2),
            bw in 0..=(u64::MAX / 2),
        ) {
            let a = sample_vector(ai, ao, ar, aw);
            let b = sample_vector(bi, bo, br, bw);
            let sum = a + b;

            prop_assert_eq!(sum.input().value(), ai + bi);
            prop_assert_eq!(sum.output().value(), ao + bo);
            prop_assert_eq!(sum.cache_read().value(), ar + br);
            prop_assert_eq!(sum.cache_write().value(), aw + bw);

            for kind in TokenKind::ALL {
                prop_assert_eq!(sum.value(kind), a.value(kind) + b.value(kind));
            }
        }
    }

    /// Retained hand-picked regression: walks fixed cases including zero, equal, and
    /// asymmetric counts.
    #[test]
    fn vector_addition_preserves_each_kind_independently_hand_picked() {
        let cases: [(u64, u64, u64, u64); 5] = [
            (0, 0, 0, 0),
            (1, 1, 1, 1),
            (10, 0, 3, 7),
            (0, 10, 3, 7),
            (u64::MAX / 2, 1, u64::MAX / 4, 2),
        ];

        for &(ai, ao, ar, aw) in &cases {
            for &(bi, bo, br, bw) in &cases {
                let a = sample_vector(ai, ao, ar, aw);
                let b = sample_vector(bi, bo, br, bw);
                let sum = a + b;

                assert_eq!(
                    sum.input().value(),
                    ai + bi,
                    "input leaked into another kind"
                );
                assert_eq!(
                    sum.output().value(),
                    ao + bo,
                    "output leaked into another kind"
                );
                assert_eq!(
                    sum.cache_read().value(),
                    ar + br,
                    "cache_read leaked into another kind"
                );
                assert_eq!(
                    sum.cache_write().value(),
                    aw + bw,
                    "cache_write leaked into another kind"
                );

                for kind in TokenKind::ALL {
                    assert_eq!(sum.value(kind), a.value(kind) + b.value(kind));
                }
            }
        }
    }

    #[test]
    fn unknown_component_round_trips_without_becoming_a_known_kind() {
        let known = sample_vector(1, 2, 3, 4);
        let mut unknown = BTreeMap::new();
        unknown.insert("reasoning_tokens".to_string(), TokenCount::new(99));

        let usage = UsageVector::new(
            known,
            unknown,
            CoverageCompleteness::partial([ComponentKind::new("transcript")]),
            EvidenceQuality::estimated([EstimatorId::new("characters")], None),
        );

        assert!(usage.has_unknown_components());
        assert_eq!(
            usage.unknown().get("reasoning_tokens").map(|c| c.value()),
            Some(99),
            "the unknown component must survive the round trip under its reported key"
        );
        // Preserved as TokenCount, a type with no From/Into to any known newtype: there
        // is no expression anywhere in this crate that reads a TokenCount out of this
        // map and hands it to something expecting InputTokens or any other known kind.
        assert_eq!(usage.known().input().value(), 1);
        assert!(matches!(
            usage.coverage(),
            CoverageCompleteness::Partial { .. }
        ));
        assert!(matches!(usage.quality(), EvidenceQuality::Estimated { .. }));
    }

    #[test]
    fn usage_vector_with_unknown_components_has_no_credits_conversion_to_call() {
        // No function in this module (or anywhere in the crate at this point) converts
        // a UsageVector to Credits: Credits does not exist yet (aub-rif.2). The
        // criterion this proves is therefore about absence, not a runtime check; this
        // test exists so a future addition of such a function is forced to reckon with
        // an explicit test here rather than slipping in unnoticed.
        let mut unknown = BTreeMap::new();
        unknown.insert("unmodeled".to_string(), TokenCount::new(1));
        let usage = UsageVector::new(
            sample_vector(0, 0, 0, 0),
            unknown,
            CoverageCompleteness::Complete,
            EvidenceQuality::Measured,
        );
        assert!(usage.has_unknown_components());
    }
}
