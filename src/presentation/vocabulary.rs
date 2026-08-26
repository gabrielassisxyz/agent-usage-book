//! The five qualification terms, the only wording a renderer may use for
//! qualification.
//!
//! The vocabulary is fixed rather than free text: complete, partial, estimated, known
//! subtotal, floor. A renderer that invents its own wording produces reports that
//! cannot be compared across commands, so the terms are defined here in one place and
//! every renderer reaches for them instead of a string literal.

use crate::domain::interval::DomainQuantity;
use crate::evidence::{CoverageCompleteness, EvidenceQuality};

/// One of the five qualification terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Qualification {
    Complete,
    Partial,
    Estimated,
    KnownSubtotal,
    Floor,
}

impl Qualification {
    /// The human-readable term.
    pub fn term(self) -> &'static str {
        match self {
            Qualification::Complete => "complete",
            Qualification::Partial => "partial",
            Qualification::Estimated => "estimated",
            Qualification::KnownSubtotal => "known subtotal",
            Qualification::Floor => "floor",
        }
    }
}

/// The coverage term: complete or partial.
pub fn coverage_term(coverage: &CoverageCompleteness) -> Qualification {
    match coverage {
        CoverageCompleteness::Complete => Qualification::Complete,
        CoverageCompleteness::Partial { .. } => Qualification::Partial,
    }
}

/// The quality term: estimated when the value is not measured, otherwise none.
pub fn quality_term<T: DomainQuantity>(quality: &EvidenceQuality<T>) -> Option<Qualification> {
    match quality {
        EvidenceQuality::Measured => None,
        EvidenceQuality::Estimated { .. } | EvidenceQuality::Mixed { .. } => {
            Some(Qualification::Estimated)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The five terms are defined in one place and are the only wording used for
    /// qualification: a grep over the vocabulary module finds no near-synonym a
    /// renderer might reach for instead.
    #[test]
    fn the_five_terms_are_the_only_qualification_wording() {
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/presentation/vocabulary.rs"
        ))
        .expect("vocabulary.rs must be readable");
        // Grep only the definition, not this test module, which names the
        // near-synonyms in order to reject them.
        let definition = source.split("#[cfg(test)]").next().unwrap_or(&source);

        for term in [
            "complete",
            "partial",
            "estimated",
            "known subtotal",
            "floor",
        ] {
            assert!(
                definition.contains(&format!("\"{term}\"")),
                "term {term:?} must be defined in one place"
            );
        }

        for synonym in [
            "incomplete",
            "approximate",
            "approx",
            "roughly",
            "lower bound",
        ] {
            assert!(
                !definition.contains(synonym),
                "near-synonym {synonym:?} must not be used for qualification"
            );
        }
    }

    #[test]
    fn coverage_and_quality_map_to_the_fixed_terms() {
        assert_eq!(
            coverage_term(&CoverageCompleteness::Complete),
            Qualification::Complete
        );
        assert_eq!(
            coverage_term(&CoverageCompleteness::partial([
                crate::evidence::ComponentKind::new("x")
            ])),
            Qualification::Partial
        );
        assert_eq!(
            quality_term::<crate::domain::tokens::TokenCount>(&EvidenceQuality::Measured),
            None
        );
        assert_eq!(
            quality_term::<crate::domain::tokens::TokenCount>(&EvidenceQuality::estimated(
                [crate::evidence::EstimatorId::new("chars")],
                None
            )),
            Some(Qualification::Estimated)
        );
    }
}
