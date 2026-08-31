// Compile-fail: `Option<Quantity>::unwrap_or_default()` fails for the same missing
// Default impl `domain_quantities_no_default.rs` proves via the direct associated-
// function call. `unwrap_or_default()` resolves the `Default` bound through a generic
// method rather than a direct `Type::default()` call, so this proves that path fails
// too rather than assuming it must, given the direct call already does.
//
// A representative sample, not every type: the missing bound is the same one
// `domain_quantities_no_default.rs` already proves exhaustively; this fixture exists to
// prove the *generic-bound* code path fails the same way, once per distinct owning
// file, not to re-enumerate every type a second time.

use agent_usage_book::domain::credits::Credits;
use agent_usage_book::domain::interval::Interval;
use agent_usage_book::domain::money::{Money, Usd};
use agent_usage_book::domain::quota::{PercentagePoints, QuotaFractionPpm};
use agent_usage_book::domain::render::Precision;
use agent_usage_book::domain::rows::RowCount;
use agent_usage_book::domain::time::UtcTimestamp;
use agent_usage_book::domain::tokens::{InputTokens, TokenCount};

fn main() {
    let a: Option<InputTokens> = None;
    let _ = a.unwrap_or_default();

    let b: Option<Credits> = None;
    let _ = b.unwrap_or_default();

    let c: Option<QuotaFractionPpm> = None;
    let _ = c.unwrap_or_default();

    let d: Option<Money<Usd>> = None;
    let _ = d.unwrap_or_default();

    let e: Option<PercentagePoints> = None;
    let _ = e.unwrap_or_default();

    let f: Option<RowCount> = None;
    let _ = f.unwrap_or_default();

    let g: Option<Precision> = None;
    let _ = g.unwrap_or_default();

    let h: Option<UtcTimestamp> = None;
    let _ = h.unwrap_or_default();

    let i: Option<Interval<TokenCount>> = None;
    let _ = i.unwrap_or_default();
}
