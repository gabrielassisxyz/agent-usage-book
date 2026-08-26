//! A rendering helper invoked without its precision policy must not compile: a
//! quantity cannot reach a user-visible surface without a precision policy.

use agent_usage_book::presentation::render_quantity;
use agent_usage_book::presentation::vocabulary::Qualification;

fn main() {
    let _ = render_quantity("42", "tokens", Qualification::Complete);
}
