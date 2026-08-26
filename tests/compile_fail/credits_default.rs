// Compile-fail: Credits does not implement Default. A default credit amount would be a
// silent, meaningless zero standing in for "nobody set this yet", which is exactly the
// kind of unjustified number this project's correctness invariants refuse to print.

use agent_usage_book::domain::credits::Credits;

fn main() {
    let _ = Credits::default();
}
