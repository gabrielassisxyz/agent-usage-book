// Ordering two semantic identifiers must not compile: an identifier has no
// ordering, and the absence of a PartialOrd implementation is what makes it a
// compile error.

use agent_usage_book::domain::ids::MeterSemanticsId;

fn main() {
    let a = MeterSemanticsId::new("account-5h-v2");
    let b = MeterSemanticsId::new("account-5h-v3");
    let _ordered = a < b;
}
