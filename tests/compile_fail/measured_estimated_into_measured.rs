// Combining a Measured and an Estimated evidence quality must not be labelled
// Measured: the combination is Mixed, and binding it as Measured is a
// refutable pattern that does not compile.

use agent_usage_book::domain::tokens::TokenCount;
use agent_usage_book::evidence::{EstimatorId, EvidenceQuality};

fn main() {
    let measured: EvidenceQuality<TokenCount> = EvidenceQuality::Measured;
    let estimated: EvidenceQuality<TokenCount> =
        EvidenceQuality::estimated([EstimatorId::new("characters")], None);
    let combined = measured.combine(&estimated);
    let EvidenceQuality::Measured = combined;
}
