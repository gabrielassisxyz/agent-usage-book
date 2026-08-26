// Compile-fail: production construction of CreditsPerPercentagePoint is pub(crate),
// restricted to the agent_usage_book crate (in practice, the calibration and store
// modules; see src/domain/credits.rs's module documentation for why pub(crate) is the
// tightest boundary expressible from that file). A trybuild fixture always compiles as
// its own separate crate, so this is a real "outside the crate" call site, not a
// simulated one: the same private-item visibility error a genuinely external consumer
// would hit.

use agent_usage_book::domain::credits::CreditsPerPercentagePoint;

fn main() {
    let _ = CreditsPerPercentagePoint::from_micros_per_point(1);
}
