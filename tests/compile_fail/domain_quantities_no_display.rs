// Compile-fail: no ordinary Phase 0 quantity implements a free-standing Display. A bare
// scalar must not be able to escape into a user interface; rendering goes through a
// presentation helper that takes explicit context (unit, coverage, evidence quality,
// freshness, precision policy). Money<Usd> has its own dedicated case
// (money_display.rs, aub-rif.4); this fixture covers every other type
// docs/domain-quantity-inventory.md lists as owing a Display case here.
//
// Values come from helper functions whose bodies are `unreachable!()` (never executed;
// `!` coerces to the declared return type): a compile-fail fixture is only ever
// type-checked, and `{}` fails to resolve at the type level regardless of whether the
// value the format machinery would print ever exists at runtime.

use agent_usage_book::domain::credits::Credits;
use agent_usage_book::domain::money::{MoneyPerMillionTokens, Usd};
use agent_usage_book::domain::quota::{PercentagePoints, QuotaFractionPpm, QuotaRemaining, QuotaUsed};
use agent_usage_book::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, TokenCount,
    UsageVector,
};

fn input_tokens() -> InputTokens {
    unreachable!()
}
fn output_tokens() -> OutputTokens {
    unreachable!()
}
fn cache_read_tokens() -> CacheReadTokens {
    unreachable!()
}
fn cache_write_tokens() -> CacheWriteTokens {
    unreachable!()
}
fn token_count() -> TokenCount {
    unreachable!()
}
fn known_token_vector() -> KnownTokenVector {
    unreachable!()
}
fn usage_vector() -> UsageVector {
    unreachable!()
}
fn credits() -> Credits {
    unreachable!()
}
fn quota_fraction_ppm() -> QuotaFractionPpm {
    unreachable!()
}
fn quota_used() -> QuotaUsed {
    unreachable!()
}
fn quota_remaining() -> QuotaRemaining {
    unreachable!()
}
fn percentage_points() -> PercentagePoints {
    unreachable!()
}
fn money_per_million_tokens() -> MoneyPerMillionTokens<Usd> {
    unreachable!()
}

fn main() {
    println!("{}", input_tokens());
    println!("{}", output_tokens());
    println!("{}", cache_read_tokens());
    println!("{}", cache_write_tokens());
    println!("{}", token_count());
    println!("{}", known_token_vector());
    println!("{}", usage_vector());
    println!("{}", credits());
    println!("{}", quota_fraction_ppm());
    println!("{}", quota_used());
    println!("{}", quota_remaining());
    println!("{}", percentage_points());
    println!("{}", money_per_million_tokens());
}
