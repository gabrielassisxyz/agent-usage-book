// Compile-fail: production construction of CreditsPerToken is pub(crate), restricted
// to the agent_usage_book crate (in practice, the calibration and store modules; see
// src/domain/credits.rs's module documentation for why pub(crate) is the tightest
// boundary expressible from that file). A trybuild fixture always compiles as its own
// separate crate, so this is a real "outside the crate" call site, not a simulated one.
// Mirrors credits_per_percentage_point_construction_outside_boundary.rs (aub-rif.2),
// which covers the other coefficient type.

use agent_usage_book::domain::credits::CreditsPerToken;

fn main() {
    let _ = CreditsPerToken::from_micros_per_million_tokens(1);
}
