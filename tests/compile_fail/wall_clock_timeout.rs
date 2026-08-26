// Compile-fail: a wall-clock duration must not be accepted by the timeout
// interface. Timeouts are measured on the monotonic clock, so a clock
// adjustment mid-request cannot cancel or extend a blocking operation.

use agent_usage_book::domain::time::Timeout;

fn main() {
    let _ = Timeout::new(std::time::Duration::from_secs(5));
}
