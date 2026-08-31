// Compile-fail: no ordinary Phase 0 quantity implements Default. `unwrap_or_default()`
// is the single most natural way to turn a missing measurement into a confident zero,
// and this project's design refuses that: a failed source never produces zero. Credits
// has its own dedicated case (credits_default.rs, aub-rif.2); this fixture covers every
// other type docs/domain-quantity-inventory.md lists as owing a Default case here.

use agent_usage_book::domain::interval::Interval;
use agent_usage_book::domain::money::{Money, MoneyPerMillionTokens, Usd};
use agent_usage_book::domain::quota::{PercentagePoints, QuotaFractionPpm, QuotaRemaining, QuotaUsed};
use agent_usage_book::domain::render::Precision;
use agent_usage_book::domain::rows::RowCount;
use agent_usage_book::domain::time::{
    Age, ClockSkewEnvelope, MonotonicDuration, MonotonicInstant, ProviderObservedAt, ReceivedAt,
    Timeout, UtcDate, UtcTimestamp,
};
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
    let _ = RowCount::default();
    let _ = Precision::default();
    let _ = UtcTimestamp::default();
    let _ = UtcDate::default();
    let _ = ProviderObservedAt::default();
    let _ = ReceivedAt::default();
    let _ = MonotonicDuration::default();
    let _ = MonotonicInstant::default();
    let _ = Age::default();
    let _ = ClockSkewEnvelope::default();
    let _ = Timeout::default();
    let _ = Interval::<TokenCount>::default();
}
