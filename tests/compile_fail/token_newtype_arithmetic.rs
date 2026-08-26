// Compile-fail: no arithmetic operator exists between two different token newtypes.
// Adding an OutputTokens to an InputTokens must not compile, proving the real per-kind
// newtypes (each with `Add` implemented only against itself), not a placeholder pair of
// structs standing in for them.

use agent_usage_book::domain::tokens::{InputTokens, OutputTokens};

fn main() {
    let input = InputTokens::new(1);
    let output = OutputTokens::new(2);
    let _sum = input + output;
}
