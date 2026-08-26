//! A rendering helper invoked without its qualification argument must not compile:
//! a quantity cannot reach a user-visible surface without its qualification.

use agent_usage_book::presentation::precision::TOKENS;
use agent_usage_book::presentation::render_quantity;

fn main() {
    let _ = render_quantity("42", "tokens", TOKENS);
}
