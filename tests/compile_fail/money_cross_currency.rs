// Adding two Money values in different currencies must not compile: the currency is a
// type parameter, so Money<Usd> + Money<Eur> has no Add implementation.

use agent_usage_book::domain::money::{Eur, Money, Usd};

fn main() {
    let usd = Money::<Usd>::from_micros(1_000_000);
    let eur = Money::<Eur>::from_micros(1_000_000);
    let _sum = usd + eur;
}
