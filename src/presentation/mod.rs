//! Human and machine-readable output.
//!
//! May not depend on:
//! - provider adapters
//! - physical-unit arithmetic (it consumes typed report models and never performs it)
//!
//! Rendering helpers require explicit context: a unit label, a qualification and a
//! precision policy, plus freshness where the value is a meter reading. A bare scalar
//! cannot reach a user-visible surface because no helper here accepts one.

pub mod export_jsonl;
pub mod json;
pub mod precision;
pub mod render;
pub mod vocabulary;

pub use json::{
    JsonContractError, JsonEnvelope, ParsedEnvelope, Quantity, SCHEMA_VERSION,
    coverage_and_quality_json, error_envelope_json, freshness_json, interval_from_json,
    interval_json, provenance_from_json, provenance_json, spend_json, status_json,
    validate_envelope_strict, validate_spend_report_json, validate_status_report_json,
};
pub use precision::{CREDITS, MONEY, PERCENT, TOKENS};
pub use render::{
    format_number, render_age, render_failure_class, render_meter_reading, render_percentage,
    render_quantity, render_spend_report, render_stale_reason, render_status_report, render_total,
};
pub use vocabulary::{Qualification, coverage_term, quality_term};
