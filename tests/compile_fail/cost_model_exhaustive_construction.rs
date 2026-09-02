// Compile-fail: a production `CostModel` cannot be constructed from outside the
// crate, neither through a struct literal (its fields are private: this is the
// "incomplete fields" half, because a literal must name every field) nor through
// its constructor (which is pub(crate): this is the "primitive fields" half,
// because assembling one from raw parts is exactly what the repository boundary
// exists to prevent). A trybuild fixture always compiles as its own separate
// crate, so both attempts are real "outside the crate" call sites, not simulated
// ones. The same private-construction boundary is asserted for the store rows by
// `credits_per_token_construction_outside_boundary.rs` for the coefficient type
// (aub-rif.2); this fixture is the model-level half (aub-ai3.1).

use agent_usage_book::store::cost_model::CostModel;

fn main() {
    // A struct literal is impossible without naming every field, and every field
    // is private.
    let _literal = CostModel {};

    // The validating constructor is pub(crate), so an external crate cannot reach
    // it either.
    let _constructed = CostModel::new(
        agent_usage_book::domain::provenance::CostModelId::new("cm-x"),
        agent_usage_book::store::cost_model::ProviderKey::new("provider"),
        agent_usage_book::store::cost_model::CostModelScope::ModelClass,
        agent_usage_book::domain::ids::BillingSemanticsId::new("billing-v1"),
        None,
        agent_usage_book::store::cost_model::CostModelVersion::new("1"),
        agent_usage_book::store::cost_model::ValidityInterval::new(
            agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(1),
            agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(2),
        )
        .unwrap(),
        agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(3),
        agent_usage_book::store::cost_model::ModelProvenance::from_parts(1, 0),
        Vec::new(),
    );
}
