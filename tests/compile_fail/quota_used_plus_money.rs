// Adding a quota quantity to a monetary amount must not compile: a quota
// fraction and a monetary amount are different quantities, and no Add
// implementation exists between them.

use agent_usage_book::domain::money::{Money, Usd};
use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaUsed};

fn main() {
    let used = QuotaUsed::new(QuotaFractionPpm::new(300_000).unwrap());
    let money = Money::<Usd>::from_micros(1_000_000);
    let _sum = used + money;
}
