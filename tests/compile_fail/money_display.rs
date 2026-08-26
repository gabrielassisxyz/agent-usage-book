// Printing Money with a bare Display must not compile: Money deliberately implements no
// Display, so rendering requires an explicit helper.

use agent_usage_book::domain::money::{Money, Usd};

fn main() {
    let usd = Money::<Usd>::from_micros(1_000_000);
    println!("{}", usd);
}
