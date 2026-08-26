// Adding a token count to a credit count must not compile: the two are
// distinct quantities, and no Add implementation exists between them.

use agent_usage_book::domain::credits::Credits;
use agent_usage_book::domain::tokens::TokenCount;

fn main() {
    let tokens = TokenCount::new(10);
    let credits = Credits::from_micros(5);
    let _sum = tokens + credits;
}
