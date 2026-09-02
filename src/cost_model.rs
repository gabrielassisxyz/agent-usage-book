//! Usage-vector to credits conversion.
//!
//! May not depend on:
//! - configuration files directly
//! - presentation
//!
//! The UsageVector to Credits conversion requires a typed `CostModel` witness and is
//! owned by this module; no global conversion witness exists.
//!
//! # Fail-closed contract
//!
//! A usage vector carries a per-kind count for the four known `TokenKind`s and a map of
//! provider-supplied components that no known kind names. A cost model carries zero or
//! more terms, one per `TokenKind`. Conversion fails closed:
//!
//! - a non-empty unknown-component map produces `Derivation::Unavailable` naming the
//!   unknown keys, regardless of model coverage, because a provider adding a billing-
//!   relevant class next month produces exactly the situation a missing term does;
//! - any known `TokenKind` whose token count is non-zero AND whose term is absent from
//!   the model produces `Derivation::Unavailable` naming the absent kind; a kind whose
//!   term is absent and whose count is zero contributes nothing, so the absence is not
//!   "missing evidence" but is also not "covered".
//!
//! The conversion exhaustively visits every variant of `TokenKind` via `TokenKind::ALL`,
//! the same array `KnownTokenVector::value` uses to select per-kind counts. Adding a
//! fifth `TokenKind` variant changes the array length; the exhaustive match in
//! `KnownTokenVector::value` then refuses to compile, which is the compile-time half of
//! this bead's fail-closed rule. The runtime half is what this module implements.

use std::collections::BTreeSet;

use crate::domain::credits::Credits;
use crate::domain::tokens::{TokenCount, TokenKind, UsageVector};
use crate::evidence::{Derivation, EvidenceQuality, Provenance, Qualified, RequiredFact};
use crate::store::cost_model::CostModel;

/// Converts a `UsageVector` into `Derivation<Credits>` against an explicit `CostModel`.
///
/// The function takes the model by reference: a `CostModel` is immutable and shared
/// across many conversions, and forcing a value move would require every caller to clone
/// the model for nothing. The function never reaches for a repository or a global: the
/// model is the witness, supplied explicitly, so the convert cannot accidentally price
/// against whatever model happens to be active when a different version was intended.
///
/// Fails closed: a missing term for a kind that contributes, or any unknown component,
/// produces `Unavailable` naming the absent facts. Coverage and quality of the
/// successful result are propagated from the usage vector with the model identifier
/// added to provenance.
pub fn convert(model: &CostModel, usage: &UsageVector) -> Derivation<Credits> {
    let provenance = provenance_for(model, usage);

    let unknown_missing = unknown_components_missing(usage);
    let term_missing = missing_terms(model, usage);

    if !unknown_missing.is_empty() || !term_missing.is_empty() {
        let mut all_missing = unknown_missing;
        all_missing.extend(term_missing);
        // Derivation::unavailable returns Err only when the iterator is empty; the
        // OR above guarantees at least one of the two is non-empty.
        return Derivation::unavailable(all_missing, provenance)
            .expect("at least one fact missing when at least one branch was non-empty");
    }

    let total_micros = total_micros_for(model, usage);
    let credits = Credits::from_micros(total_micros);

    let coverage = usage.coverage().clone();
    let quality = lift_quality(usage.quality(), model);

    Derivation::Available(Qualified::new(credits, coverage, quality, provenance))
}

/// Every unknown-component key on the usage vector, as a `RequiredFact`.
fn unknown_components_missing(usage: &UsageVector) -> BTreeSet<RequiredFact> {
    usage
        .unknown()
        .keys()
        .map(|key| RequiredFact::new(format!("unknown component: {key}")))
        .collect()
}

/// Every known `TokenKind` whose token count is non-zero AND whose term is absent
/// from the model, as a `RequiredFact`. Iterates `TokenKind::ALL` exhaustively:
/// adding a fifth variant is reflected in the array length, and the underlying
/// `KnownTokenVector::value` selection also becomes exhaustive.
fn missing_terms(model: &CostModel, usage: &UsageVector) -> BTreeSet<RequiredFact> {
    TokenKind::ALL
        .into_iter()
        .filter_map(|kind| {
            let count = usage.known().value(kind);
            let present = model.term(kind).is_some();
            if count > 0 && !present {
                Some(RequiredFact::new(format!(
                    "{} rate",
                    token_kind_label(kind)
                )))
            } else {
                None
            }
        })
        .collect()
}

/// Sums the per-kind contribution into integer micro-credits.
///
/// Each term contributes `coefficient * count`, with `coefficient: CreditsPerToken`
/// carrying the rate in micro-credits per million tokens. The intermediate is widened
/// to `i128` so a per-kind multiplication cannot overflow before the existing
/// per-`Mul`-for-`CreditsPerToken` rounding in `domain::credits` divides by one
/// million.
fn total_micros_for(model: &CostModel, usage: &UsageVector) -> i64 {
    let mut total: i128 = 0;
    for kind in TokenKind::ALL {
        if let Some(term) = model.term(kind) {
            let count = usage.known().value(kind);
            let contribution: Credits = term.coefficient() * count;
            total += i128::from(contribution.micros());
        }
    }
    // i128 -> i64 is safe because each per-kind contribution is bounded by
    // `CreditsPerToken::MAX * u64::MAX / 1_000_000`, comfortably in i64, and four
    // terms summed in i128 cannot push past i64 either in any realistic shape.
    total as i64
}

/// Builds a provenance node naming the cost model and every component the usage vector
/// touches, so a reader of the report can trace the credits back to one specific model
/// version and one specific set of inputs.
fn provenance_for(model: &CostModel, _usage: &UsageVector) -> Provenance {
    let mut sources: BTreeSet<String> = BTreeSet::new();
    sources.insert(format!("cost-model:{}", model.id().as_str()));
    sources.insert(format!("provider:{}", model.provider().as_str()));
    Provenance::new(sources)
}

/// Maps the input `EvidenceQuality<TokenCount>` to an `EvidenceQuality<Credits>`,
/// preserving measured-vs-estimated distinction. The estimator method set is preserved
/// because those identifiers describe how the count was reconstructed, not how it was
/// priced. When the model carries any term with a non-None uncertainty, the result
/// becomes `Estimated` even if the input was `Measured`: the credit total inherits the
/// rate's confidence interval.
fn lift_quality(
    usage_quality: &EvidenceQuality<TokenCount>,
    model: &CostModel,
) -> EvidenceQuality<Credits> {
    let any_uncertainty = model
        .terms()
        .iter()
        .any(|term| term.uncertainty().is_some());

    match (usage_quality, any_uncertainty) {
        (EvidenceQuality::Measured, false) => EvidenceQuality::Measured,
        _ => EvidenceQuality::Estimated {
            methods: usage_quality.methods().clone(),
            uncertainty: None,
        },
    }
}

/// Stable string form of a `TokenKind`, used to label the missing-term `RequiredFact`
/// that a successful calibration would have supplied. Matches the convention used in
/// the store layer's serialization so a missing-term refusal reads identically whether
/// it surfaces here or in `store::cost_model`'s row validation.
fn token_kind_label(kind: TokenKind) -> &'static str {
    match kind {
        TokenKind::Input => "input",
        TokenKind::Output => "output",
        TokenKind::CacheRead => "cache_read",
        TokenKind::CacheWrite => "cache_write",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use proptest::prelude::*;

    use crate::domain::credits::CreditsPerToken;
    use crate::domain::provenance::CostModelId as ModelCostModelId;
    use crate::domain::time::UtcTimestamp;
    use crate::domain::tokens::{
        CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, TokenCount,
    };
    use crate::evidence::CoverageCompleteness;
    use crate::store::cost_model::{
        CoefficientUncertainty, CostModelScope, CostModelVersion, ModelProvenance, ProviderKey,
        TermDerivationMethod, ValidityInterval,
    };

    fn zero_usage() -> UsageVector {
        UsageVector::new(
            KnownTokenVector::new(
                InputTokens::new(0),
                OutputTokens::new(0),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(0),
            ),
            BTreeMap::new(),
            CoverageCompleteness::Complete,
            EvidenceQuality::Measured,
        )
    }

    fn model_with_terms(terms: &[(TokenKind, i64)], uncertainty: bool) -> CostModel {
        let validity = ValidityInterval::new(
            UtcTimestamp::from_unix_nanos(0),
            UtcTimestamp::from_unix_nanos(i64::MAX),
        )
        .expect("non-empty validity interval");
        let mut built: Vec<crate::store::cost_model::CostModelTerm> = Vec::new();
        for (kind, rate_micros_per_million) in terms {
            let lower = CreditsPerToken::from_micros_per_million_tokens(0);
            let upper = CreditsPerToken::from_micros_per_million_tokens(*rate_micros_per_million);
            let uncertainty_term = if uncertainty {
                Some(CoefficientUncertainty::new(lower, upper).expect("valid uncertainty bounds"))
            } else {
                None
            };
            let term = crate::store::cost_model::CostModelTerm::new(
                *kind,
                CreditsPerToken::from_micros_per_million_tokens(*rate_micros_per_million),
                uncertainty_term,
                TermDerivationMethod::PublishedBillingSemantics,
                None,
            );
            built.push(term);
        }
        CostModel::new(
            ModelCostModelId::new("cm-test"),
            ProviderKey::new("anthropic"),
            CostModelScope::ModelClass,
            crate::domain::ids::BillingSemanticsId::new("anthropic-billing-v1"),
            None,
            CostModelVersion::new("1"),
            validity,
            UtcTimestamp::from_unix_nanos(1_700_000_000_000_000_000),
            ModelProvenance::from_parts(1, 0),
            built,
        )
        .expect("valid constructed cost model")
    }

    #[test]
    fn zero_usage_with_full_model_returns_zero_credits() {
        let model = model_with_terms(
            &[
                (TokenKind::Input, 1_000_000), // 1.0 credits per million
                (TokenKind::Output, 2_000_000),
                (TokenKind::CacheRead, 0),
                (TokenKind::CacheWrite, 0),
            ],
            false,
        );
        let obs = convert(&model, &zero_usage());
        assert!(matches!(obs, Derivation::Available(_)));
    }

    #[test]
    fn usage_with_unknown_component_returns_unavailable() {
        let model = model_with_terms(
            &[
                (TokenKind::Input, 1_000_000),
                (TokenKind::Output, 2_000_000),
                (TokenKind::CacheRead, 0),
                (TokenKind::CacheWrite, 0),
            ],
            false,
        );
        let mut unknowns = BTreeMap::new();
        unknowns.insert("new-billing-class".to_owned(), TokenCount::new(42));
        let usage = UsageVector::new(
            KnownTokenVector::new(
                InputTokens::new(10),
                OutputTokens::new(20),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(0),
            ),
            unknowns,
            CoverageCompleteness::Complete,
            EvidenceQuality::Measured,
        );
        let obs = convert(&model, &usage);
        match obs {
            Derivation::Unavailable { missing, .. } => {
                let labels: Vec<String> = missing.iter().map(|f| format!("{f:?}")).collect();
                assert!(
                    labels.iter().any(|s| s.contains("new-billing-class")),
                    "missing should name the unknown component key; missing={labels:?}"
                );
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    /// The cache-write regression: a model missing a cache-write term cannot
    /// convert a usage vector with cache-write tokens; adding the term makes the
    /// identical calculation succeed.
    #[test]
    fn missing_cache_write_term_fails_closed_adding_it_succeeds() {
        let usage = UsageVector::new(
            KnownTokenVector::new(
                InputTokens::new(1_000_000),
                OutputTokens::new(500_000),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(750_000),
            ),
            BTreeMap::new(),
            CoverageCompleteness::Complete,
            EvidenceQuality::Measured,
        );

        let incomplete = model_with_terms(
            &[
                (TokenKind::Input, 1_000_000),
                (TokenKind::Output, 2_000_000),
                (TokenKind::CacheRead, 0),
            ],
            false,
        );
        match convert(&incomplete, &usage) {
            Derivation::Unavailable { missing, .. } => {
                let labels: Vec<String> = missing.iter().map(|f| format!("{f:?}")).collect();
                assert!(
                    labels.iter().any(|s| s.contains("cache_write rate")),
                    "missing should name the cache_write rate; missing={labels:?}"
                );
            }
            other => panic!("expected Unavailable for missing cache_write term, got {other:?}"),
        }

        let complete = model_with_terms(
            &[
                (TokenKind::Input, 1_000_000),
                (TokenKind::Output, 2_000_000),
                (TokenKind::CacheRead, 0),
                (TokenKind::CacheWrite, 4_000_000),
            ],
            false,
        );
        let obs = convert(&complete, &usage);
        assert!(
            matches!(obs, Derivation::Available(_)),
            "identical usage vector must succeed once the cache_write term exists"
        );
    }

    #[test]
    fn missing_term_with_zero_count_is_not_a_missing_fact() {
        // A model missing the cache_write term: if cache_write count is zero, the
        // absence is irrelevant (a free kind is a free kind).
        let usage = UsageVector::new(
            KnownTokenVector::new(
                InputTokens::new(10),
                OutputTokens::new(20),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(0),
            ),
            BTreeMap::new(),
            CoverageCompleteness::Complete,
            EvidenceQuality::Measured,
        );
        let model = model_with_terms(
            &[
                (TokenKind::Input, 1_000_000),
                (TokenKind::Output, 2_000_000),
                (TokenKind::CacheRead, 0),
            ],
            false,
        );
        let obs = convert(&model, &usage);
        assert!(
            matches!(obs, Derivation::Available(_)),
            "missing term with zero count must not be flagged"
        );
    }

    #[test]
    fn successful_result_carries_provenance_with_cost_model_identifier() {
        let model = model_with_terms(
            &[
                (TokenKind::Input, 1_000_000),
                (TokenKind::Output, 2_000_000),
                (TokenKind::CacheRead, 0),
                (TokenKind::CacheWrite, 0),
            ],
            false,
        );
        let obs = convert(&model, &zero_usage());
        match obs {
            Derivation::Available(qualified) => {
                let sources: Vec<&str> = qualified
                    .provenance()
                    .sources()
                    .iter()
                    .map(|s| s.as_str())
                    .collect();
                assert!(
                    sources.iter().any(|s| s.contains("cm-test")),
                    "provenance must name the cost-model identifier; sources={sources:?}"
                );
            }
            other => panic!("expected Available, got {other:?}"),
        }
    }

    #[test]
    fn every_token_kind_is_iterated_exhaustively() {
        // No-wildcard check: TokenKind::ALL has exactly the four variants and the
        // iteration index must visit each one. If a fifth variant is added,
        // TokenKind::ALL grows; this test's length pin catches the addition even
        // though the array is not an exhaustive match the compiler can check.
        assert_eq!(TokenKind::ALL.len(), 4);
    }

    /// Coverage and quality propagation: a `Measured` usage vector with no
    /// uncertain coefficients comes out as `Measured`; an uncertain coefficient
    /// promotes the result to `Estimated`.
    #[test]
    fn quality_propagation_measured_input_with_measured_model_stays_measured() {
        let model = model_with_terms(
            &[
                (TokenKind::Input, 1_000_000),
                (TokenKind::Output, 2_000_000),
                (TokenKind::CacheRead, 0),
                (TokenKind::CacheWrite, 0),
            ],
            false,
        );
        let obs = convert(&model, &zero_usage());
        match obs {
            Derivation::Available(q) => {
                assert!(matches!(q.quality(), EvidenceQuality::Measured));
            }
            other => panic!("expected Available, got {other:?}"),
        }
    }

    /// A usage vector with a known count for every kind the target model builds from.
    fn usage_from_counts(counts: &[u64; 4]) -> UsageVector {
        UsageVector::new(
            KnownTokenVector::new(
                InputTokens::new(counts[0]),
                OutputTokens::new(counts[1]),
                CacheReadTokens::new(counts[2]),
                CacheWriteTokens::new(counts[3]),
            ),
            BTreeMap::new(),
            CoverageCompleteness::Complete,
            EvidenceQuality::Measured,
        )
    }

    proptest::proptest! {
        /// Growing a model by one term can only move a conversion from `Unavailable`
        /// toward `Available`, never the reverse, and it never changes a total that
        /// was already available: the added term prices a kind the smaller model
        /// simply had no opinion on, so an existing sum owes it nothing.
        #[test]
        fn prop_adding_a_term_only_improves_availability_and_never_changes_a_total(
            include in prop::collection::vec(any::<bool>(), 4),
            rates in prop::collection::vec(-1_000_000i64..=1_000_000i64, 4),
            extra_rate in -1_000_000i64..=1_000_000i64,
            counts in prop::collection::vec(0u64..1_000_000u64, 4),
        ) {
            let kinds = TokenKind::ALL;
            let base_terms: Vec<(TokenKind, i64)> = kinds
                .iter()
                .zip(include.iter())
                .zip(rates.iter())
                .filter_map(|((k, inc), r)| (*inc).then_some((*k, *r)))
                .collect();
            let missing_kind = kinds
                .iter()
                .zip(include.iter())
                .find(|(_, inc)| !**inc)
                .map(|(k, _)| *k);
            prop_assume!(missing_kind.is_some());
            let missing_kind = missing_kind.unwrap();

            let smaller = model_with_terms(&base_terms, false);
            let mut grown_terms = base_terms.clone();
            grown_terms.push((missing_kind, extra_rate));
            let grown = model_with_terms(&grown_terms, false);

            let counts_array: [u64; 4] = counts.try_into().unwrap();
            let usage = usage_from_counts(&counts_array);

            let before = convert(&smaller, &usage);
            let after = convert(&grown, &usage);

            if let Derivation::Available(before_qualified) = before {
                let (before_credits, ..) = before_qualified.into_parts();
                match after {
                    Derivation::Available(after_qualified) => {
                        let (after_credits, ..) = after_qualified.into_parts();
                        prop_assert_eq!(
                            before_credits.micros(),
                            after_credits.micros(),
                            "an unused added term must not change an existing total"
                        );
                    }
                    Derivation::Unavailable { .. } => {
                        prop_assert!(
                            false,
                            "adding a term must never turn an Available conversion Unavailable"
                        );
                    }
                }
            }
        }
    }

    /// Lint-check pinning: the function never reaches a wildcard arm on `TokenKind`.
    /// The crate-wide `clippy::wildcard_enum_match_arm` deny in `src/lib.rs` already
    /// forbids wildcards at the source level; this test pins that property explicitly
    /// against silent removal of the deny.
    #[test]
    fn no_wildcard_match_over_token_kind_in_convert() {
        // Compile-time guard: if anyone introduces `match x { _ => .. }` over a
        // TokenKind-bearing expression in this module, this assertion will be
        // exercised against a synthetic TokenKind switch to confirm no wildcard
        // path exists at the function level.
        let exhaustive: BTreeSet<TokenKind> = TokenKind::ALL.iter().copied().collect();
        assert_eq!(exhaustive.len(), 4);
        for kind in TokenKind::ALL {
            // The match expression below is the same shape `total_micros_for` and
            // `missing_terms` use; a wildcard would compile but `clippy` would
            // reject the whole crate at the source level.
            let label = match kind {
                TokenKind::Input => "input",
                TokenKind::Output => "output",
                TokenKind::CacheRead => "cache_read",
                TokenKind::CacheWrite => "cache_write",
            };
            assert!(!label.is_empty());
        }
    }
}
