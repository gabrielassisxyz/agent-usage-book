use std::collections::BTreeSet;

use agent_usage_book::domain::tokens::TokenCount;
use agent_usage_book::evidence::{Derivation, Provenance, RequiredFact};

fn main() {
    let unavailable = Derivation::<TokenCount>::unavailable(
        BTreeSet::from([RequiredFact::new("cache-write rate")]),
        Provenance::new(["fixture".to_owned()]),
    )
    .unwrap();
    let _: TokenCount = unavailable.value();
}
