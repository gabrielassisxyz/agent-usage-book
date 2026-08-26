// Passing a meter-semantics identifier where a billing-semantics identifier is
// expected must not compile: the two are distinct opaque types, and the
// absence of a conversion between them is what makes it a compile error.

use agent_usage_book::domain::ids::{BillingSemanticsId, MeterSemanticsId};

fn takes_billing(id: BillingSemanticsId) {
    let _ = id;
}

fn main() {
    let meter = MeterSemanticsId::new("account-5h-v2");
    takes_billing(meter);
}
