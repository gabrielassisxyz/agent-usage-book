// A report model must not accept a bare domain quantity: the spend group's token
// count is a Qualified<TokenCount>, and a bare TokenCount is rejected at compile time.

use agent_usage_book::domain::provenance::{DerivationId, ProvenanceManifest, QuerySemantics};
use agent_usage_book::domain::tokens::TokenCount;
use agent_usage_book::logging::LogicalName;
use agent_usage_book::report::SpendGroup;

fn main() {
    let bare = TokenCount::new(100);
    let _group = SpendGroup::new(
        LogicalName::new("by-day"),
        bare,
        DerivationId::from_manifest(&ProvenanceManifest::new(
            [],
            [],
            QuerySemantics::new("by-day", "all"),
        )),
    );
}
