// Passing a QuotaUsed where a QuotaRemaining is expected must not compile: the two are
// complements, and mixing them inverts a decision without changing the shape of
// anything.

use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaRemaining, QuotaUsed};

fn takes_remaining(_: QuotaRemaining) {}

fn main() {
    let used = QuotaUsed::new(QuotaFractionPpm::new(500_000).unwrap());
    takes_remaining(used);
}
