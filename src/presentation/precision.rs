//! Precision policy per quantity kind.
//!
//! A percentage and a credit count do not share an arbitrary default: each quantity
//! kind names its own precision here, and a renderer takes the policy explicitly
//! rather than reaching for a global default.

use crate::domain::render::Precision;

/// Percentages render with two fractional digits.
pub const PERCENT: Precision = Precision::new(2);

/// Credits render with two fractional digits.
pub const CREDITS: Precision = Precision::new(2);

/// Token counts render with no fractional digits.
pub const TOKENS: Precision = Precision::new(0);

/// Money renders with two fractional digits.
pub const MONEY: Precision = Precision::new(2);
