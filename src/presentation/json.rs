//! The versioned JSON contract.
//!
//! JSON output is a public contract, not a debug dump. Every quantity serializes as
//! an object carrying a value and a unit, with the value as a string where exactness
//! matters, and every report serializes under a shared envelope carrying the schema
//! version, the command, the run identifier, the generation time, the knowledge time
//! and the ledger generation. A consumer never has to infer a unit or a freshness
//! state from a bare number or a timestamp.

use crate::domain::freshness::{Freshness, StaleReason};
use crate::domain::interval::{DomainQuantity, Interval};
use crate::domain::quota::QuotaRemaining;
use crate::domain::time::UtcTimestamp;
use crate::domain::tokens::TokenKind;
use crate::evidence::{CoverageCompleteness, EvidenceQuality, Provenance};
use crate::logging::RunId;
use crate::report::{LedgerGeneration, ReportMetadata, SpendReport, StatusReport};

/// The schema version. Bump this when the JSON shape changes; the contract tests
/// below pin the exact shape, so a field added without bumping this fails them.
pub const SCHEMA_VERSION: u32 = 1;

/// An error during JSON contract validation or deserialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonContractError {
    /// Invalid JSON syntax.
    InvalidJson(String),
    /// Missing a required field.
    MissingField(&'static str),
    /// Schema version mismatch or unversioned schema change.
    SchemaVersionMismatch { expected: u32, actual: u32 },
    /// An unknown / unexpected field was encountered in a strict envelope or object.
    UnexpectedField(String),
    /// Field has an unexpected type or format.
    InvalidFormat {
        field: &'static str,
        message: String,
    },
    /// Unit mismatch for a typed quantity.
    UnitMismatch {
        expected: &'static str,
        actual: String,
    },
}

impl std::fmt::Display for JsonContractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidJson(msg) => write!(f, "invalid JSON: {msg}"),
            Self::MissingField(field) => write!(f, "missing required field: {field}"),
            Self::SchemaVersionMismatch { expected, actual } => {
                write!(
                    f,
                    "schema version mismatch: expected {expected}, got {actual}"
                )
            }
            Self::UnexpectedField(field) => write!(f, "unexpected field: {field}"),
            Self::InvalidFormat { field, message } => {
                write!(f, "invalid format for field '{field}': {message}")
            }
            Self::UnitMismatch { expected, actual } => {
                write!(f, "unit mismatch: expected '{expected}', got '{actual}'")
            }
        }
    }
}

impl std::error::Error for JsonContractError {}

/// A quantity with explicit unit semantics and an exact value representation.
///
/// The value is a string so a fixed-point integer or a decimal is never rounded
/// through a float on its way to a consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quantity {
    value: String,
    unit: &'static str,
}

impl Quantity {
    pub fn new(value: impl Into<String>, unit: &'static str) -> Self {
        Self {
            value: value.into(),
            unit,
        }
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn unit(&self) -> &'static str {
        self.unit
    }

    /// The JSON object `{"value": "...", "unit": "..."}`.
    pub fn to_json(&self) -> String {
        format!(
            "{{\"value\":{},\"unit\":{}}}",
            json_string(&self.value),
            json_string(self.unit)
        )
    }

    /// Deserializes a quantity from its JSON string representation.
    pub fn from_json(json_str: &str) -> Result<Self, JsonContractError> {
        let value: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| JsonContractError::InvalidJson(e.to_string()))?;
        Self::from_value(&value)
    }

    /// Extracts a quantity from a JSON object `{"value": "...", "unit": "..."}`.
    pub fn from_value(val: &serde_json::Value) -> Result<Self, JsonContractError> {
        let obj = val
            .as_object()
            .ok_or_else(|| JsonContractError::InvalidFormat {
                field: "quantity",
                message: "expected JSON object".to_string(),
            })?;
        for key in obj.keys() {
            if key != "value" && key != "unit" {
                return Err(JsonContractError::UnexpectedField(key.clone()));
            }
        }
        let value_val = obj
            .get("value")
            .ok_or(JsonContractError::MissingField("value"))?;
        let value_str = match value_val {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Array(_)
            | serde_json::Value::Object(_) => {
                return Err(JsonContractError::InvalidFormat {
                    field: "value",
                    message: "expected string or number".to_string(),
                });
            }
        };
        let unit_str = obj
            .get("unit")
            .and_then(serde_json::Value::as_str)
            .ok_or(JsonContractError::MissingField("unit"))?;
        let leaked_unit: &'static str = match unit_str {
            "ppm" => "ppm",
            "tokens" => "tokens",
            "credits" => "credits",
            "rows" => "rows",
            "usd" => "usd",
            _ => Box::leak(unit_str.to_string().into_boxed_str()),
        };
        Ok(Self::new(value_str, leaked_unit))
    }
}

/// Parsed metadata extracted from a JSON envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedEnvelope {
    pub schema: u32,
    pub command: String,
    pub run: RunId,
    pub generated_at: UtcTimestamp,
    pub knowledge_at: UtcTimestamp,
    pub ledger_generation: LedgerGeneration,
}

/// The shared envelope every report serializes under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonEnvelope {
    command: &'static str,
    run: RunId,
    metadata: ReportMetadata,
}

impl JsonEnvelope {
    pub fn new(command: &'static str, run: RunId, metadata: ReportMetadata) -> Self {
        Self {
            command,
            run,
            metadata,
        }
    }

    pub fn command(&self) -> &'static str {
        self.command
    }

    pub fn run(&self) -> &RunId {
        &self.run
    }

    pub fn metadata(&self) -> &ReportMetadata {
        &self.metadata
    }

    /// The envelope object: schema version, command, run identifier, generation time,
    /// knowledge time and ledger generation.
    pub fn to_json(&self) -> String {
        format!("{{{}}}", self.fields())
    }

    /// The envelope with the report's own fields appended after the shared ones,
    /// so every command's JSON opens with the same envelope keys in the same order.
    pub fn to_json_with(&self, body: &str) -> String {
        format!("{{{},{body}}}", self.fields())
    }

    fn fields(&self) -> String {
        format!(
            "\"schema\":{},\"command\":{},\"run\":{},\"generated_at\":{},\"knowledge_at\":{},\"ledger_generation\":{}",
            SCHEMA_VERSION,
            json_string(self.command),
            json_string(self.run.as_str()),
            self.metadata.generated_at.unix_nanos(),
            self.metadata.knowledge_at.unix_nanos(),
            self.metadata.ledger_generation.get(),
        )
    }

    /// Parses and validates the envelope fields from a JSON string, returning the
    /// parsed envelope and the full JSON object.
    pub fn parse(json_str: &str) -> Result<(ParsedEnvelope, serde_json::Value), JsonContractError> {
        let value: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| JsonContractError::InvalidJson(e.to_string()))?;
        let obj = value
            .as_object()
            .ok_or_else(|| JsonContractError::InvalidFormat {
                field: "root",
                message: "expected root JSON object".to_string(),
            })?;

        let schema = obj
            .get("schema")
            .and_then(serde_json::Value::as_u64)
            .ok_or(JsonContractError::MissingField("schema"))? as u32;
        if schema != SCHEMA_VERSION {
            return Err(JsonContractError::SchemaVersionMismatch {
                expected: SCHEMA_VERSION,
                actual: schema,
            });
        }

        let command = obj
            .get("command")
            .and_then(serde_json::Value::as_str)
            .ok_or(JsonContractError::MissingField("command"))?
            .to_string();

        let run_str = obj
            .get("run")
            .and_then(serde_json::Value::as_str)
            .ok_or(JsonContractError::MissingField("run"))?;
        let run = RunId::from_string(run_str.to_string());

        let gen_nanos = obj
            .get("generated_at")
            .and_then(serde_json::Value::as_i64)
            .ok_or(JsonContractError::MissingField("generated_at"))?;
        let generated_at = UtcTimestamp::from_unix_nanos(gen_nanos);

        let know_nanos = obj
            .get("knowledge_at")
            .and_then(serde_json::Value::as_i64)
            .ok_or(JsonContractError::MissingField("knowledge_at"))?;
        let knowledge_at = UtcTimestamp::from_unix_nanos(know_nanos);

        let ledger_gen_val = obj
            .get("ledger_generation")
            .and_then(serde_json::Value::as_u64)
            .ok_or(JsonContractError::MissingField("ledger_generation"))?;
        let ledger_generation = LedgerGeneration::new(ledger_gen_val);

        let parsed = ParsedEnvelope {
            schema,
            command,
            run,
            generated_at,
            knowledge_at,
            ledger_generation,
        };
        Ok((parsed, value))
    }
}

/// Strict validation of a standalone JSON envelope: verifies all 6 envelope keys are
/// present and that NO extra or unversioned field is included.
pub fn validate_envelope_strict(json_str: &str) -> Result<ParsedEnvelope, JsonContractError> {
    let (parsed, value) = JsonEnvelope::parse(json_str)?;
    let obj = value
        .as_object()
        .ok_or_else(|| JsonContractError::InvalidFormat {
            field: "root",
            message: "expected object".to_string(),
        })?;
    const KNOWN_ENVELOPE_KEYS: [&str; 6] = [
        "schema",
        "command",
        "run",
        "generated_at",
        "knowledge_at",
        "ledger_generation",
    ];
    for key in obj.keys() {
        if !KNOWN_ENVELOPE_KEYS.contains(&key.as_str()) {
            return Err(JsonContractError::UnexpectedField(key.clone()));
        }
    }
    Ok(parsed)
}

/// Validates that a status report JSON string strictly conforms to schema version 1.
pub fn validate_status_report_json(json_str: &str) -> Result<ParsedEnvelope, JsonContractError> {
    let (parsed, value) = JsonEnvelope::parse(json_str)?;
    if parsed.command != "status" {
        return Err(JsonContractError::InvalidFormat {
            field: "command",
            message: format!("expected 'status', got '{}'", parsed.command),
        });
    }
    let obj = value
        .as_object()
        .ok_or_else(|| JsonContractError::InvalidFormat {
            field: "root",
            message: "expected object".to_string(),
        })?;
    const KNOWN_STATUS_KEYS: [&str; 7] = [
        "schema",
        "command",
        "run",
        "generated_at",
        "knowledge_at",
        "ledger_generation",
        "accounts",
    ];
    for key in obj.keys() {
        if !KNOWN_STATUS_KEYS.contains(&key.as_str()) {
            return Err(JsonContractError::UnexpectedField(key.clone()));
        }
    }
    let accounts = obj
        .get("accounts")
        .and_then(serde_json::Value::as_array)
        .ok_or(JsonContractError::MissingField("accounts"))?;
    for account in accounts {
        let acc_obj = account
            .as_object()
            .ok_or_else(|| JsonContractError::InvalidFormat {
                field: "accounts[]",
                message: "expected account object".to_string(),
            })?;
        if !acc_obj.contains_key("account") {
            return Err(JsonContractError::MissingField("account"));
        }
        let freshness = acc_obj
            .get("freshness")
            .and_then(serde_json::Value::as_str)
            .ok_or(JsonContractError::MissingField("freshness"))?;
        match freshness {
            "fresh" => {
                if !acc_obj.contains_key("remaining") {
                    return Err(JsonContractError::MissingField("remaining"));
                }
                if !acc_obj.contains_key("latest_attempt") {
                    return Err(JsonContractError::MissingField("latest_attempt"));
                }
            }
            "stale" => {
                if !acc_obj.contains_key("reason") {
                    return Err(JsonContractError::MissingField("reason"));
                }
                if !acc_obj.contains_key("last_good") {
                    return Err(JsonContractError::MissingField("last_good"));
                }
                if !acc_obj.contains_key("latest_attempt") {
                    return Err(JsonContractError::MissingField("latest_attempt"));
                }
            }
            "auth_required" => {
                if !acc_obj.contains_key("last_good") {
                    return Err(JsonContractError::MissingField("last_good"));
                }
                if !acc_obj.contains_key("latest_attempt") {
                    return Err(JsonContractError::MissingField("latest_attempt"));
                }
            }
            other => {
                return Err(JsonContractError::InvalidFormat {
                    field: "freshness",
                    message: format!("unknown freshness state '{other}'"),
                });
            }
        }
    }
    Ok(parsed)
}

/// Validates that a spend report JSON string strictly conforms to schema version 1.
pub fn validate_spend_report_json(json_str: &str) -> Result<ParsedEnvelope, JsonContractError> {
    let (parsed, value) = JsonEnvelope::parse(json_str)?;
    if parsed.command != "spend" {
        return Err(JsonContractError::InvalidFormat {
            field: "command",
            message: format!("expected 'spend', got '{}'", parsed.command),
        });
    }
    let obj = value
        .as_object()
        .ok_or_else(|| JsonContractError::InvalidFormat {
            field: "root",
            message: "expected object".to_string(),
        })?;
    const KNOWN_SPEND_KEYS: [&str; 9] = [
        "schema",
        "command",
        "run",
        "generated_at",
        "knowledge_at",
        "ledger_generation",
        "window",
        "groups",
        "ingest",
    ];
    for key in obj.keys() {
        if !KNOWN_SPEND_KEYS.contains(&key.as_str()) {
            return Err(JsonContractError::UnexpectedField(key.clone()));
        }
    }
    if !obj.contains_key("window") {
        return Err(JsonContractError::MissingField("window"));
    }
    if !obj.contains_key("groups") {
        return Err(JsonContractError::MissingField("groups"));
    }
    if !obj.contains_key("ingest") {
        return Err(JsonContractError::MissingField("ingest"));
    }
    Ok(parsed)
}

/// The status report under the envelope: one object per account with its freshness.
pub fn status_json(report: &StatusReport, run: RunId) -> String {
    let accounts = report
        .accounts
        .iter()
        .map(|account| freshness_json(account.account.as_str(), &account.reading))
        .collect::<Vec<_>>()
        .join(",");
    JsonEnvelope::new("status", run, report.metadata.clone())
        .to_json_with(&format!("\"accounts\":[{accounts}]"))
}

/// The spend report under the envelope: the window, one object per group with a
/// `{value, unit}` per token kind, and the ingest summary, so a consumer can read
/// the counts and what qualifies them from one document.
pub fn spend_json(report: &SpendReport, run: RunId) -> String {
    let groups = report
        .groups
        .iter()
        .map(|group| {
            let known = group.usage.known();
            let kinds = TokenKind::ALL
                .iter()
                .map(|kind| {
                    format!(
                        "{}:{}",
                        json_string(token_kind_key(*kind)),
                        quantity_json(&known.value(*kind).to_string(), "tokens")
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            let unknown = group
                .usage
                .unknown()
                .iter()
                .map(|(name, count)| {
                    format!(
                        "{}:{}",
                        json_string(name),
                        quantity_json(&count.value().to_string(), "tokens")
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "{{\"key\":{},\"tokens\":{{{kinds}}},\"unknown_components\":{{{unknown}}},{},\"provenance\":{}}}",
                json_string(group.key.as_str()),
                coverage_and_quality_json(group.usage.coverage(), group.usage.quality())
                    .trim_matches(|c| c == '{' || c == '}'),
                provenance_json(&group.provenance),
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let ingest = &report.ingest;
    let quarantined = ingest
        .quarantined_by_class
        .iter()
        .map(|(class, count)| format!("{}:{count}", json_string(class)))
        .collect::<Vec<_>>()
        .join(",");
    let unreadable = ingest
        .unreadable_files
        .iter()
        .map(|file| json_string(file))
        .collect::<Vec<_>>()
        .join(",");
    let body = format!(
        "\"window\":{{\"since\":{},\"until\":{},\"calendar\":\"utc\"}},\"groups\":[{groups}],\"ingest\":{{\"files_read\":{},\"files_skipped_before_window\":{},\"unreadable_files\":[{unreadable}],\"quarantined\":{{{quarantined}}},\"replayed_occurrences\":{},\"collisions\":{},\"without_identity\":{},\"undated_events\":{},\"events_outside_window\":{},\"events_in_window\":{}}}",
        json_string(&report.since.iso()),
        json_string(&report.until.iso()),
        ingest.files_read,
        ingest.files_skipped_before_window,
        ingest.replayed_occurrences,
        ingest.collisions,
        ingest.without_identity,
        ingest.undated_events,
        ingest.events_outside_window,
        ingest.events_in_window,
    );
    JsonEnvelope::new("spend", run, report.metadata.clone()).to_json_with(&body)
}

fn token_kind_key(kind: TokenKind) -> &'static str {
    match kind {
        TokenKind::Input => "input",
        TokenKind::Output => "output",
        TokenKind::CacheRead => "cache_read",
        TokenKind::CacheWrite => "cache_write",
    }
}

/// Serializes a meter account's freshness: exactly one variant, machine-readable, so
/// stale and auth-required are distinguishable without parsing prose.
pub fn freshness_json(account: &str, freshness: &Freshness<QuotaRemaining>) -> String {
    match freshness {
        Freshness::Fresh {
            observed,
            latest_attempt,
        } => format!(
            "{{\"account\":{},\"freshness\":\"fresh\",\"remaining\":{},\"latest_attempt\":{}}}",
            json_string(account),
            quantity_json(&observed.value().as_ppm().get().to_string(), "ppm"),
            latest_attempt.value(),
        ),
        Freshness::Stale {
            last_good,
            latest_attempt,
            reason,
        } => format!(
            "{{\"account\":{},\"freshness\":\"stale\",\"reason\":{},\"last_good\":{},\"latest_attempt\":{}}}",
            json_string(account),
            json_string(stale_reason_name(reason)),
            last_good_json(last_good),
            latest_attempt.value(),
        ),
        Freshness::AuthRequired {
            last_good,
            latest_attempt,
        } => format!(
            "{{\"account\":{},\"freshness\":\"auth_required\",\"last_good\":{},\"latest_attempt\":{}}}",
            json_string(account),
            last_good_json(last_good),
            latest_attempt.value(),
        ),
    }
}

/// Serializes coverage and evidence quality as two separate, independently readable
/// fields.
pub fn coverage_and_quality_json<T: DomainQuantity>(
    coverage: &CoverageCompleteness,
    quality: &EvidenceQuality<T>,
) -> String {
    format!(
        "{{\"coverage\":{},\"evidence_quality\":{}}}",
        json_string(coverage_name(coverage)),
        json_string(quality_name(quality)),
    )
}

/// Serializes an interval with both endpoints and the unit of its element type.
/// Uses exact string representations of the endpoints to prevent float rounding.
pub fn interval_json<T: DomainQuantity>(interval: &Interval<T>) -> String {
    format!(
        "{{\"lower\":{},\"upper\":{},\"unit\":{}}}",
        json_string(&interval.lower().to_exact_string()),
        json_string(&interval.upper().to_exact_string()),
        json_string(T::unit()),
    )
}

/// Deserializes an interval from JSON object `{"lower": "...", "upper": "...", "unit": "..."}`.
pub fn interval_from_json<T: DomainQuantity>(
    json_str: &str,
) -> Result<Interval<T>, JsonContractError> {
    let val: serde_json::Value = serde_json::from_str(json_str)
        .map_err(|e| JsonContractError::InvalidJson(e.to_string()))?;
    let obj = val
        .as_object()
        .ok_or_else(|| JsonContractError::InvalidFormat {
            field: "interval",
            message: "expected object".to_string(),
        })?;

    for key in obj.keys() {
        if key != "lower" && key != "upper" && key != "unit" {
            return Err(JsonContractError::UnexpectedField(key.clone()));
        }
    }

    let unit = obj
        .get("unit")
        .and_then(serde_json::Value::as_str)
        .ok_or(JsonContractError::MissingField("unit"))?;
    if unit != T::unit() {
        return Err(JsonContractError::UnitMismatch {
            expected: T::unit(),
            actual: unit.to_string(),
        });
    }

    let lower_str = obj
        .get("lower")
        .and_then(serde_json::Value::as_str)
        .ok_or(JsonContractError::MissingField("lower"))?;
    let lower = T::from_exact_str(lower_str).ok_or_else(|| JsonContractError::InvalidFormat {
        field: "lower",
        message: format!("failed to parse '{lower_str}' into quantity"),
    })?;

    let upper_str = obj
        .get("upper")
        .and_then(serde_json::Value::as_str)
        .ok_or(JsonContractError::MissingField("upper"))?;
    let upper = T::from_exact_str(upper_str).ok_or_else(|| JsonContractError::InvalidFormat {
        field: "upper",
        message: format!("failed to parse '{upper_str}' into quantity"),
    })?;

    Interval::new(lower, upper).map_err(|e| JsonContractError::InvalidFormat {
        field: "interval",
        message: format!("{e:?}"),
    })
}

/// Serializes provenance identifiers.
pub fn provenance_json(provenance: &Provenance) -> String {
    let sources = provenance
        .sources()
        .iter()
        .map(|s| json_string(s))
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"sources\":[{}]}}", sources)
}

/// Deserializes provenance from JSON object `{"sources": ["..."]}`.
pub fn provenance_from_json(json_str: &str) -> Result<Provenance, JsonContractError> {
    let val: serde_json::Value = serde_json::from_str(json_str)
        .map_err(|e| JsonContractError::InvalidJson(e.to_string()))?;
    let obj = val
        .as_object()
        .ok_or_else(|| JsonContractError::InvalidFormat {
            field: "provenance",
            message: "expected object".to_string(),
        })?;

    for key in obj.keys() {
        if key != "sources" {
            return Err(JsonContractError::UnexpectedField(key.clone()));
        }
    }

    let sources_val = obj
        .get("sources")
        .and_then(serde_json::Value::as_array)
        .ok_or(JsonContractError::MissingField("sources"))?;

    let mut sources = Vec::with_capacity(sources_val.len());
    for s in sources_val {
        let src_str = s.as_str().ok_or_else(|| JsonContractError::InvalidFormat {
            field: "sources[]",
            message: "expected string element".to_string(),
        })?;
        sources.push(src_str.to_string());
    }

    Ok(Provenance::new(sources))
}

fn quantity_json(value: &str, unit: &str) -> String {
    format!(
        "{{\"value\":{},\"unit\":{}}}",
        json_string(value),
        json_string(unit)
    )
}

fn last_good_json(
    last_good: &Option<crate::domain::freshness::Observed<QuotaRemaining>>,
) -> String {
    match last_good {
        Some(observed) => quantity_json(&observed.value().as_ppm().get().to_string(), "ppm"),
        None => "null".to_string(),
    }
}

fn stale_reason_name(reason: &StaleReason) -> &'static str {
    match reason {
        StaleReason::AgeExceeded => "age_exceeded",
        StaleReason::NoSuccessfulObservation => "no_successful_observation",
        StaleReason::SourceUnreachable(_source) => "source_unreachable",
        StaleReason::MalformedProviderResponse => "malformed_provider_response",
        StaleReason::RateLimited => "rate_limited",
        StaleReason::SamplingGap => "sampling_gap",
        StaleReason::ClockAnomaly => "clock_anomaly",
        StaleReason::CollectorInterrupted => "collector_interrupted",
        StaleReason::CredentialChangedUnverified => "credential_changed_unverified",
    }
}

fn coverage_name(coverage: &CoverageCompleteness) -> &'static str {
    match coverage {
        CoverageCompleteness::Complete => "complete",
        CoverageCompleteness::Partial { missing: _ } => "partial",
    }
}

fn quality_name<T: DomainQuantity>(quality: &EvidenceQuality<T>) -> &'static str {
    match quality {
        EvidenceQuality::Measured => "measured",
        EvidenceQuality::Estimated {
            methods: _,
            uncertainty: _,
        } => "estimated",
        EvidenceQuality::Mixed {
            methods: _,
            uncertainty: _,
        } => "mixed",
    }
}

/// Escapes a string for a JSON string literal.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attempt::AttemptId;
    use crate::domain::freshness::Observed;
    use crate::domain::quota::QuotaFractionPpm;
    use crate::domain::time::{MeasurementBasis, ReceivedAt, UtcTimestamp};
    use crate::domain::tokens::TokenCount;
    use crate::report::{LedgerGeneration, ReportMetadata};

    fn metadata() -> ReportMetadata {
        ReportMetadata::new(
            UtcTimestamp::from_unix_nanos(2_000),
            UtcTimestamp::from_unix_nanos(1_000),
            LedgerGeneration::new(7),
            None,
        )
    }

    fn remaining(ppm: u32) -> QuotaRemaining {
        QuotaRemaining::new(QuotaFractionPpm::new(ppm as i32).unwrap())
    }

    fn observed(ppm: u32) -> Observed<QuotaRemaining> {
        Observed::new(
            remaining(ppm),
            None,
            ReceivedAt::new(UtcTimestamp::from_unix_nanos(1)),
            MeasurementBasis::ProviderObserved,
        )
    }

    #[test]
    fn quantity_serializes_with_value_and_unit() {
        let q = Quantity::new("500000", "ppm");
        let json = q.to_json();
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON expected");
        assert_eq!(
            parsed,
            serde_json::json!({
                "value": "500000",
                "unit": "ppm"
            })
        );
        let round_trip = Quantity::from_json(&json).expect("parse back");
        assert_eq!(round_trip, q);
    }

    #[test]
    fn envelope_serializes_every_required_field_with_exact_schema() {
        let run = RunId::new(UtcTimestamp::from_unix_nanos(42));
        let envelope = JsonEnvelope::new("now", run.clone(), metadata());
        let json = envelope.to_json();

        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON envelope");
        let expected = serde_json::json!({
            "schema": 1,
            "command": "now",
            "run": run.as_str(),
            "generated_at": 2000,
            "knowledge_at": 1000,
            "ledger_generation": 7
        });
        assert_eq!(parsed, expected);

        let validated = validate_envelope_strict(&json).expect("envelope validates strictly");
        assert_eq!(validated.schema, 1);
        assert_eq!(validated.command, "now");
        assert_eq!(validated.run.as_str(), run.as_str());
        assert_eq!(validated.generated_at.unix_nanos(), 2000);
        assert_eq!(validated.knowledge_at.unix_nanos(), 1000);
        assert_eq!(validated.ledger_generation.get(), 7);
    }

    #[test]
    fn adding_field_without_version_bump_fails_strict_validation() {
        let run = RunId::new(UtcTimestamp::from_unix_nanos(42));
        let envelope = JsonEnvelope::new("now", run.clone(), metadata());
        let unversioned_json = format!(
            "{{{},\"extra_unversioned_field\":\"surprise\"}}",
            envelope.fields()
        );

        let err = validate_envelope_strict(&unversioned_json)
            .expect_err("adding an unversioned field must fail strict contract validation");
        assert_eq!(
            err,
            JsonContractError::UnexpectedField("extra_unversioned_field".to_string())
        );
    }

    #[test]
    fn bumping_schema_version_without_schema_update_fails_contract() {
        let run = RunId::new(UtcTimestamp::from_unix_nanos(42));
        let invalid_version_json = format!(
            "\"schema\":2,\"command\":\"now\",\"run\":{},\"generated_at\":2000,\"knowledge_at\":1000,\"ledger_generation\":7",
            json_string(run.as_str())
        );
        let wrapped = format!("{{{invalid_version_json}}}");

        let err = validate_envelope_strict(&wrapped)
            .expect_err("bumped schema version without contract update must fail");
        assert_eq!(
            err,
            JsonContractError::SchemaVersionMismatch {
                expected: 1,
                actual: 2
            }
        );
    }

    #[test]
    fn freshness_serializes_exactly_one_variant() {
        let fresh_str = freshness_json(
            "work-a",
            &Freshness::Fresh {
                observed: observed(500_000),
                latest_attempt: AttemptId::new(1),
            },
        );
        let fresh_parsed: serde_json::Value =
            serde_json::from_str(&fresh_str).expect("valid fresh JSON");
        assert_eq!(
            fresh_parsed,
            serde_json::json!({
                "account": "work-a",
                "freshness": "fresh",
                "remaining": {
                    "value": "500000",
                    "unit": "ppm"
                },
                "latest_attempt": 1
            })
        );

        let stale_str = freshness_json(
            "work-a",
            &Freshness::Stale {
                last_good: Some(observed(500_000)),
                latest_attempt: AttemptId::new(2),
                reason: StaleReason::AgeExceeded,
            },
        );
        let stale_parsed: serde_json::Value =
            serde_json::from_str(&stale_str).expect("valid stale JSON");
        assert_eq!(
            stale_parsed,
            serde_json::json!({
                "account": "work-a",
                "freshness": "stale",
                "reason": "age_exceeded",
                "last_good": {
                    "value": "500000",
                    "unit": "ppm"
                },
                "latest_attempt": 2
            })
        );

        let auth_str = freshness_json(
            "work-a",
            &Freshness::<QuotaRemaining>::AuthRequired {
                last_good: None,
                latest_attempt: AttemptId::new(3),
            },
        );
        let auth_parsed: serde_json::Value =
            serde_json::from_str(&auth_str).expect("valid auth JSON");
        assert_eq!(
            auth_parsed,
            serde_json::json!({
                "account": "work-a",
                "freshness": "auth_required",
                "last_good": null,
                "latest_attempt": 3
            })
        );
    }

    #[test]
    fn coverage_and_quality_are_separate_fields() {
        let json = coverage_and_quality_json::<TokenCount>(
            &CoverageCompleteness::Complete,
            &EvidenceQuality::Measured,
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("valid coverage/quality JSON");
        assert_eq!(
            parsed,
            serde_json::json!({
                "coverage": "complete",
                "evidence_quality": "measured"
            })
        );

        let partial = coverage_and_quality_json::<TokenCount>(
            &CoverageCompleteness::partial([crate::evidence::ComponentKind::new("x")]),
            &EvidenceQuality::estimated([crate::evidence::EstimatorId::new("chars")], None),
        );
        let partial_parsed: serde_json::Value =
            serde_json::from_str(&partial).expect("valid partial coverage/quality JSON");
        assert_eq!(
            partial_parsed,
            serde_json::json!({
                "coverage": "partial",
                "evidence_quality": "estimated"
            })
        );
    }

    #[test]
    fn interval_serializes_both_endpoints_exact_and_the_unit() {
        // Test with a large u64 value that would lose precision if routed through f64
        let large_lower = TokenCount::new(9_007_199_254_740_993); // 2^53 + 1
        let large_upper = TokenCount::new(9_007_199_254_740_995); // 2^53 + 3
        let interval = Interval::new(large_lower, large_upper).unwrap();
        let json = interval_json(&interval);

        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid interval JSON");
        assert_eq!(
            parsed,
            serde_json::json!({
                "lower": "9007199254740993",
                "upper": "9007199254740995",
                "unit": "tokens"
            })
        );

        let round_trip: Interval<TokenCount> =
            interval_from_json(&json).expect("interval round-trip");
        assert_eq!(round_trip, interval);
    }

    #[test]
    fn provenance_identifiers_round_trip() {
        let provenance = Provenance::new(["a".to_string(), "b".to_string()]);
        let json = provenance_json(&provenance);
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid provenance JSON");
        assert_eq!(
            parsed,
            serde_json::json!({
                "sources": ["a", "b"]
            })
        );

        let round_trip = provenance_from_json(&json).expect("provenance round-trip");
        assert_eq!(round_trip, provenance);
    }

    #[test]
    fn json_string_escapes_control_characters() {
        assert_eq!(json_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(json_string("a\\b"), "\"a\\\\b\"");
        assert_eq!(json_string("a\nb"), "\"a\\nb\"");
        assert_eq!(json_string("a\rb"), "\"a\\rb\"");
        assert_eq!(json_string("a\tb"), "\"a\\tb\"");
        assert_eq!(json_string("a\u{0000}b"), "\"a\\u0000b\"");
        assert_eq!(json_string("a\u{001f}b"), "\"a\\u001fb\"");
    }
}
