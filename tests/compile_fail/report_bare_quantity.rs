// A report model must not accept a bare domain quantity: the spend group's usage is
// a UsageVector carrying its own coverage and evidence quality, and a bare TokenCount
// is rejected at compile time.

use agent_usage_book::domain::provenance::{DerivationId, ProvenanceManifest, QuerySemantics};
use agent_usage_book::domain::tokens::TokenCount;
use agent_usage_book::evidence::Provenance;
use agent_usage_book::logging::LogicalName;
use agent_usage_book::report::SpendGroup;

fn main() {
    let bare = TokenCount::new(100);
    let _group = SpendGroup::new(
        LogicalName::new("by-day"),
        bare,
        Provenance::new([]),
        DerivationId::from_manifest(&ProvenanceManifest::new(
            [],
            [],
            QuerySemantics::new("by-day", "all"),
        )),
    );
}
