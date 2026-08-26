// Adding two quota-fraction levels must not compile: a level plus a level is a category
// error, and the absence of an Add implementation is what makes it a compile error.

use agent_usage_book::domain::quota::QuotaFractionPpm;

fn main() {
    let a = QuotaFractionPpm::new(100).unwrap();
    let b = QuotaFractionPpm::new(200).unwrap();
    let _sum = a + b;
}
