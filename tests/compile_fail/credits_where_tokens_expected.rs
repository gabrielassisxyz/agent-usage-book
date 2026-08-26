// Passing a Credits value where a token count is expected must not compile:
// the two are distinct quantities, and no conversion exists between them.

use agent_usage_book::domain::credits::Credits;
use agent_usage_book::domain::tokens::TokenCount;

fn takes_tokens(_tokens: TokenCount) {}

fn main() {
    let credits = Credits::from_micros(5);
    takes_tokens(credits);
}
