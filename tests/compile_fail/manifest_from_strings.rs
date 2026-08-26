// Compile-fail: a manifest is constructible only from typed evidence
// identifiers, never from strings.

use agent_usage_book::domain::provenance::{ProvenanceManifest, QuerySemantics, WitnessId};

fn main() {
    let _ = ProvenanceManifest::new(
        vec!["session-1".to_string()],
        std::iter::empty::<WitnessId>(),
        QuerySemantics::new("by-account", "last-7-days"),
    );
}
