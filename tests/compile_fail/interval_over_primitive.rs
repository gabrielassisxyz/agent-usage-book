// Compile-fail: `Interval<T>` requires `T` to be an ordered domain quantity,
// so instantiating it over a bare primitive must not compile. A bare `f64`
// carries no unit and no closed arithmetic, which is exactly what the
// interval exists to prevent.

use agent_usage_book::domain::interval::Interval;

fn main() {
    let _ = Interval::<f64>::new(0.0, 1.0);
}
