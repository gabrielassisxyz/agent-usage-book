// Compile-fail: a derived quantity requires a provenance manifest. The only
// constructor takes both a value and a manifest, so a derived quantity with no
// provenance cannot be constructed.

use agent_usage_book::domain::provenance::Derived;

fn main() {
    let _ = Derived::new(42u64);
}
