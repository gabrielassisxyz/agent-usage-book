// Compile-fail: production construction of PointsPerMillionTokens is pub(crate),
// restricted to the agent_usage_book crate (in practice, the valuation module reading
// a stored rate card; see src/domain/credits.rs's module documentation for why
// pub(crate) is the tightest boundary expressible from a domain file). A trybuild
// fixture always compiles as its own separate crate, so this is a real "outside the
// crate" call site: a consumer that could mint this coefficient could state a window
// movement no rate card ever claimed.

use agent_usage_book::domain::quota::PointsPerMillionTokens;

fn main() {
    let _ = PointsPerMillionTokens::from_micro_points_per_million(1);
}
