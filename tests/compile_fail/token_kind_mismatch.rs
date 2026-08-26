// Compile-fail: the four per-kind token newtypes are distinct types with no implicit
// conversion between them. Passing an InputTokens where an OutputTokens is expected
// must not compile.

use agent_usage_book::domain::tokens::{InputTokens, OutputTokens};

fn takes_output(_tokens: OutputTokens) {}

fn main() {
    let input = InputTokens::new(10);
    takes_output(input);
}
