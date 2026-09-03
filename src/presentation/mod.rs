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
    coverage_and_quality_json, coverage_json, doctor_drift_json, error_envelope_json, explain_json,
    freshness_json, interval_from_json, interval_json, provenance_from_json, provenance_json,
    spend_json, spend_json_with_explain, status_json, status_json_with_explain,
    validate_coverage_report_json, validate_doctor_drift_report_json, validate_envelope_strict,
    validate_spend_report_json, validate_status_report_json,
};
pub use precision::{CREDITS, MONEY, PERCENT, TOKENS};
pub use render::{
    ExplainMode, format_number, render_age, render_coverage_report, render_doctor_drift_report,
    render_explain, render_failure_class, render_meter_reading, render_percentage, render_quantity,
    render_spend_report, render_spend_report_with_explain, render_stale_reason,
    render_status_report, render_status_report_with_explain, render_total,
};
pub use vocabulary::{Qualification, coverage_term, quality_term};
