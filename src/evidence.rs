//! Coverage and evidence-quality propagation, provenance graphs, and source qualification.
//!
//! May not depend on:
//! - SQLite, HTTP, or terminal-formatting crates
//! - transcript locations
//! - any adapter, workflow, or presentation layer

use std::collections::BTreeSet;

use crate::domain::interval::{DomainQuantity, Interval};

/// A source component that was required but did not contribute evidence.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ComponentKind(String);

impl ComponentKind {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The stable name of this component, as it renders and digests.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An algorithm that reconstructed rather than measured a value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EstimatorId(String);

impl EstimatorId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The stable identifier of this estimator, as it renders and digests.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A fact required to perform a requested derivation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RequiredFact(String);

impl RequiredFact {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Stable references to the evidence used to produce a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    sources: BTreeSet<String>,
}

impl Provenance {
    pub fn new(sources: impl IntoIterator<Item = String>) -> Self {
        Self {
            sources: sources.into_iter().collect(),
        }
    }

    pub fn sources(&self) -> &BTreeSet<String> {
        &self.sources
    }
}

/// Whether every required component contributed evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverageCompleteness {
    Complete,
    Partial { missing: BTreeSet<ComponentKind> },
}

impl CoverageCompleteness {
    pub fn partial(missing: impl IntoIterator<Item = ComponentKind>) -> Self {
        Self::Partial {
            missing: missing.into_iter().collect(),
        }
    }

    pub fn combine(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::Complete, Self::Complete) => Self::Complete,
            (Self::Partial { missing }, Self::Complete)
            | (Self::Complete, Self::Partial { missing }) => Self::Partial {
                missing: missing.clone(),
            },
            (Self::Partial { missing: left }, Self::Partial { missing: right }) => Self::Partial {
                missing: left.union(right).cloned().collect(),
            },
        }
    }

    pub fn missing(&self) -> Option<&BTreeSet<ComponentKind>> {
        match self {
            Self::Complete => None,
            Self::Partial { missing } => Some(missing),
        }
    }
}

/// How present evidence was obtained. Combining can only retain or degrade quality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceQuality<T: DomainQuantity> {
    Measured,
    Estimated {
        methods: BTreeSet<EstimatorId>,
        uncertainty: Option<Interval<T>>,
    },
    Mixed {
        methods: BTreeSet<EstimatorId>,
        uncertainty: Option<Interval<T>>,
    },
}

impl<T: DomainQuantity> EvidenceQuality<T> {
    pub fn estimated(
        methods: impl IntoIterator<Item = EstimatorId>,
        uncertainty: Option<Interval<T>>,
    ) -> Self {
        Self::Estimated {
            methods: methods.into_iter().collect(),
            uncertainty,
        }
    }

    pub fn combine(&self, other: &Self) -> Self {
        let methods = methods(self)
            .union(methods(other))
            .cloned()
            .collect::<BTreeSet<_>>();
        let uncertainty = combine_uncertainty(uncertainty(self), uncertainty(other));
        match (self, other) {
            (Self::Measured, Self::Measured) => Self::Measured,
            (Self::Estimated { .. }, Self::Estimated { .. }) => Self::Estimated {
                methods,
                uncertainty,
            },
            _ => Self::Mixed {
                methods,
                uncertainty,
            },
        }
    }

    pub fn methods(&self) -> &BTreeSet<EstimatorId> {
        methods(self)
    }

    pub fn uncertainty(&self) -> Option<&Interval<T>> {
        uncertainty(self)
    }
}

fn methods<T: DomainQuantity>(quality: &EvidenceQuality<T>) -> &BTreeSet<EstimatorId> {
    match quality {
        EvidenceQuality::Measured => empty_methods(),
        EvidenceQuality::Estimated { methods, .. } | EvidenceQuality::Mixed { methods, .. } => {
            methods
        }
    }
}

fn empty_methods() -> &'static BTreeSet<EstimatorId> {
    static EMPTY: std::sync::OnceLock<BTreeSet<EstimatorId>> = std::sync::OnceLock::new();
    EMPTY.get_or_init(BTreeSet::new)
}

fn uncertainty<T: DomainQuantity>(quality: &EvidenceQuality<T>) -> Option<&Interval<T>> {
    match quality {
        EvidenceQuality::Measured => None,
        EvidenceQuality::Estimated { uncertainty, .. }
        | EvidenceQuality::Mixed { uncertainty, .. } => uncertainty.as_ref(),
    }
}

fn combine_uncertainty<T: DomainQuantity>(
    left: Option<&Interval<T>>,
    right: Option<&Interval<T>>,
) -> Option<Interval<T>> {
    match (left, right) {
        (Some(left), Some(right)) => Some(
            Interval::new(
                left.lower().min(right.lower()),
                left.upper().max(right.upper()),
            )
            .expect("minimum lower cannot exceed maximum upper"),
        ),
        (Some(interval), None) | (None, Some(interval)) => Some(*interval),
        (None, None) => None,
    }
}

/// A value with independent coverage, evidence-quality, and provenance witnesses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Qualified<T: DomainQuantity> {
    value: T,
    coverage: CoverageCompleteness,
    quality: EvidenceQuality<T>,
    provenance: Provenance,
}

impl<T: DomainQuantity> Qualified<T> {
    pub fn new(
        value: T,
        coverage: CoverageCompleteness,
        quality: EvidenceQuality<T>,
        provenance: Provenance,
    ) -> Self {
        Self {
            value,
            coverage,
            quality,
            provenance,
        }
    }

    /// Returns every witness with the value; there is intentionally no value-only accessor.
    pub fn into_parts(self) -> (T, CoverageCompleteness, EvidenceQuality<T>, Provenance) {
        (self.value, self.coverage, self.quality, self.provenance)
    }

    pub fn coverage(&self) -> &CoverageCompleteness {
        &self.coverage
    }

    pub fn quality(&self) -> &EvidenceQuality<T> {
        &self.quality
    }

    pub fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// Either a qualified result or a truthful refusal naming what is absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Derivation<T: DomainQuantity> {
    Available(Qualified<T>),
    Unavailable {
        missing: BTreeSet<RequiredFact>,
        provenance: Provenance,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissingRequiredFacts;

impl<T: DomainQuantity> Derivation<T> {
    pub fn unavailable(
        missing: impl IntoIterator<Item = RequiredFact>,
        provenance: Provenance,
    ) -> Result<Self, MissingRequiredFacts> {
        let missing = missing.into_iter().collect::<BTreeSet<_>>();
        if missing.is_empty() {
            return Err(MissingRequiredFacts);
        }
        Ok(Self::Unavailable {
            missing,
            provenance,
        })
    }

    pub fn missing(&self) -> Option<&BTreeSet<RequiredFact>> {
        match self {
            Self::Available(_) => None,
            Self::Unavailable { missing, .. } => Some(missing),
        }
    }

    pub fn provenance(&self) -> &Provenance {
        match self {
            Self::Available(qualified) => qualified.provenance(),
            Self::Unavailable { provenance, .. } => provenance,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::tokens::TokenCount;
    use test_support::{Rng, Seed, check_property};

    fn provenance() -> Provenance {
        Provenance::new(["fixture".to_owned()])
    }

    fn component(value: &str) -> ComponentKind {
        ComponentKind::new(value)
    }

    fn estimator(value: &str) -> EstimatorId {
        EstimatorId::new(value)
    }

    #[test]
    fn six_data_quality_examples_keep_both_dimensions() {
        let measured: EvidenceQuality<TokenCount> = EvidenceQuality::Measured;
        let estimated: EvidenceQuality<TokenCount> =
            EvidenceQuality::estimated([estimator("characters")], None);
        let missing = CoverageCompleteness::partial([component("cache-write")]);

        let exact = Qualified::new(
            TokenCount::new(10),
            CoverageCompleteness::Complete,
            measured.clone(),
            provenance(),
        );
        assert!(matches!(exact.coverage(), CoverageCompleteness::Complete));
        assert!(matches!(exact.quality(), EvidenceQuality::Measured));

        let parser_gap = Qualified::new(
            TokenCount::new(10),
            missing.clone(),
            measured.clone(),
            provenance(),
        );
        assert!(matches!(
            parser_gap.coverage(),
            CoverageCompleteness::Partial { .. }
        ));
        assert!(matches!(parser_gap.quality(), EvidenceQuality::Measured));

        let reconstruction = Qualified::new(
            TokenCount::new(10),
            CoverageCompleteness::Complete,
            estimated.clone(),
            provenance(),
        );
        assert!(matches!(
            reconstruction.coverage(),
            CoverageCompleteness::Complete
        ));
        assert!(matches!(
            reconstruction.quality(),
            EvidenceQuality::Estimated { .. }
        ));

        let mixed = measured.combine(&estimated);
        let aggregate = Qualified::new(
            TokenCount::new(20),
            CoverageCompleteness::Complete,
            mixed.clone(),
            provenance(),
        );
        assert!(matches!(
            aggregate.coverage(),
            CoverageCompleteness::Complete
        ));
        assert!(matches!(aggregate.quality(), EvidenceQuality::Mixed { .. }));

        let partial_aggregate = Qualified::new(TokenCount::new(20), missing, mixed, provenance());
        assert!(matches!(
            partial_aggregate.coverage(),
            CoverageCompleteness::Partial { .. }
        ));
        assert!(matches!(
            partial_aggregate.quality(),
            EvidenceQuality::Mixed { .. }
        ));

        let unavailable = Derivation::<TokenCount>::unavailable(
            [RequiredFact::new("cache-write rate")],
            provenance(),
        )
        .expect("a named missing fact makes refusal useful");
        assert_eq!(unavailable.missing().map(BTreeSet::len), Some(1));
    }

    #[test]
    fn unavailable_requires_a_named_missing_fact() {
        assert_eq!(
            Derivation::<TokenCount>::unavailable([], provenance()),
            Err(MissingRequiredFacts)
        );
    }

    #[test]
    fn quality_combine_measured_and_estimated_is_mixed() {
        let estimated: EvidenceQuality<TokenCount> =
            EvidenceQuality::estimated([estimator("characters")], None);
        assert!(matches!(
            EvidenceQuality::Measured.combine(&estimated),
            EvidenceQuality::Mixed { .. }
        ));
    }

    #[test]
    fn coverage_combine_never_recovers_complete_from_partial() {
        check_property("coverage monotonicity", 0..256, |seed| {
            let mut rng = Rng::new(Seed(seed));
            let mut combined = CoverageCompleteness::Complete;
            let mut saw_partial = false;
            for step in 0..8 {
                let next = if rng.next_below(2) == 0 {
                    CoverageCompleteness::Complete
                } else {
                    saw_partial = true;
                    CoverageCompleteness::partial([component(&format!("missing-{step}"))])
                };
                combined = combined.combine(&next);
            }
            !saw_partial || matches!(combined, CoverageCompleteness::Partial { .. })
        });
    }

    #[test]
    fn quality_combine_never_recovers_measured_from_estimated() {
        check_property("quality monotonicity", 0..256, |seed| {
            let mut rng = Rng::new(Seed(seed));
            let mut combined: EvidenceQuality<TokenCount> = EvidenceQuality::Measured;
            let mut saw_estimated = false;
            for step in 0..8 {
                let next = if rng.next_below(2) == 0 {
                    EvidenceQuality::Measured
                } else {
                    saw_estimated = true;
                    EvidenceQuality::estimated([estimator(&format!("method-{step}"))], None)
                };
                combined = combined.combine(&next);
            }
            !saw_estimated || !matches!(combined, EvidenceQuality::Measured)
        });
    }
}
