// Compile-fail: constructing an ordinary Phase 0 quantity by naming its private
// representation directly, bypassing the validated public smart constructor, must not
// compile. A representative sample across the shapes that occur in this crate (a
// tuple struct over a primitive, a tuple struct over another newtype, a multi-field
// brace struct, and a generic brace struct), not every type: the mechanism is Rust's
// ordinary field-privacy rule and does not vary by which quantity owns the field, only
// by the struct's shape.

use std::marker::PhantomData;

use agent_usage_book::domain::money::{Money, Usd};
use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaUsed};
use agent_usage_book::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens,
};

fn main() {
    // Tuple struct over a primitive.
    let _ = InputTokens(5);

    // Tuple struct over another newtype.
    let inner = QuotaFractionPpm::new(500_000).unwrap();
    let _ = QuotaUsed(inner);

    // Multi-field brace struct.
    let _ = KnownTokenVector {
        input: InputTokens::new(1),
        output: OutputTokens::new(2),
        cache_read: CacheReadTokens::new(3),
        cache_write: CacheWriteTokens::new(4),
    };

    // Generic brace struct.
    let _ = Money::<Usd> {
        micros: 1_000_000,
        _currency: PhantomData,
    };
}
