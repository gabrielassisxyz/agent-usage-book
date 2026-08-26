// Compile-fail: constructing a Fresh variant with no latest attempt reference must not
// compile. Every Freshness variant carries latest_attempt; omitting it is a missing
// required field, not a privacy violation, since Rust does not support private fields
// on an enum struct-variant the way it does on a struct.

use agent_usage_book::domain::freshness::{Freshness, Observed};
use agent_usage_book::domain::time::{MeasurementBasis, ReceivedAt, UtcTimestamp};

fn main() {
    let observed: Observed<u64> = Observed::new(
        1,
        None,
        ReceivedAt::new(UtcTimestamp::from_unix_nanos(0)),
        MeasurementBasis::ProviderObserved,
    );
    let _ = Freshness::Fresh { observed };
}
