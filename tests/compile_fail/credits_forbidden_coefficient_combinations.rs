// Compile-fail: the two coefficient types (CreditsPerToken, CreditsPerPercentagePoint)
// and their respective multipliers (a raw token count, PercentagePoints) form exactly
// two valid pairings. Every cross-wiring of the wrong coefficient with the other's
// multiplier, and the two coefficients multiplied against each other, must not compile.
// One fixture, four independent error sites, since all four are the same class of
// mistake: reaching for the wrong coefficient.
//
// This fixture is about the Mul operators, not construction: the real constructors are
// pub(crate) and unreachable from here regardless (that is what
// credits_per_percentage_point_construction_outside_boundary.rs proves on its own), so
// values of the coefficient types are obtained through functions whose bodies never
// run, relying on `!`'s coercion to any return type. Nothing here executes; a
// compile-fail fixture is only ever type-checked.

use agent_usage_book::domain::credits::{CreditsPerPercentagePoint, CreditsPerToken};
use agent_usage_book::domain::quota::PercentagePoints;

fn per_token() -> CreditsPerToken {
    unreachable!()
}

fn per_point() -> CreditsPerPercentagePoint {
    unreachable!()
}

fn points() -> PercentagePoints {
    unreachable!()
}

fn main() {
    // CreditsPerToken only multiplies by a raw token count, not by PercentagePoints.
    let _wrong_a = per_token() * points();

    // CreditsPerPercentagePoint only multiplies by PercentagePoints, not by a raw
    // token count.
    let _wrong_b = per_point() * 1u64;

    // The two coefficients never multiply against each other, in either order.
    let _wrong_c = per_token() * per_point();
    let _wrong_d = per_point() * per_token();
}
