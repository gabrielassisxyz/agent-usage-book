// Compile-fail: AttemptOutcome is a separate enum from Freshness, and no From/Into
// exists between them in either direction. What was recorded about an attempt and what
// a user is told about freshness are reconstructed from attempt history by a state
// machine in a separate bead, never derived by a type conversion here.

use agent_usage_book::domain::attempt::AttemptOutcome;
use agent_usage_book::domain::freshness::Freshness;

fn main() {
    let outcome = AttemptOutcome::Success;
    let _: Freshness<u64> = outcome.into();
}
