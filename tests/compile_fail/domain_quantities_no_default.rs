// Compile-fail: no ordinary Phase 0 quantity implements Default. `unwrap_or_default()`
// is the single most natural way to turn a missing measurement into a confident zero,
// and this project's design refuses that: a failed source never produces zero. Credits
// has its own dedicated case (credits_default.rs, aub-rif.2); this fixture covers every
// other type docs/domain-quantity-inventory.md lists as owing a Default case here.

use agent_usage_book::domain::money::{Money, MoneyPerMillionTokens, Usd};
use agent_usage_book::domain::quota::{PercentagePoints, QuotaFractionPpm, QuotaRemaining, QuotaUsed};
use agent_usage_book::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, TokenCount,
    UsageVector,
};

fn main() {
    let _ = InputTokens::default();
    let _ = OutputTokens::default();
    let _ = CacheReadTokens::default();
    let _ = CacheWriteTokens::default();
    let _ = TokenCount::default();
    let _ = KnownTokenVector::default();
    let _ = UsageVector::default();
    let _ = QuotaFractionPpm::default();
    let _ = QuotaUsed::default();
    let _ = QuotaRemaining::default();
    let _ = PercentagePoints::default();
    let _ = Money::<Usd>::default();
    let _ = MoneyPerMillionTokens::<Usd>::default();
}
