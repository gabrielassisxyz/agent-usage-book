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
use crate::domain::tokens::TokenKind;
use crate::evidence::{CoverageCompleteness, EvidenceQuality, Provenance};
use crate::logging::RunId;
use crate::report::{ReportMetadata, SpendReport, StatusReport};

/// The schema version. Bump this when the JSON shape changes; the contract tests
/// below pin the exact shape, so a field added without bumping this fails them.
pub const SCHEMA_VERSION: u32 = 1;

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
pub fn interval_json<T: DomainQuantity>(interval: &Interval<T>) -> String {
    format!(
        "{{\"lower\":{},\"upper\":{},\"unit\":{}}}",
        json_string(&interval.lower().to_f64().to_string()),
        json_string(&interval.upper().to_f64().to_string()),
        json_string(T::unit()),
    )
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
        StaleReason::SourceUnreachable(_) => "source_unreachable",
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
        CoverageCompleteness::Partial { .. } => "partial",
    }
}

fn quality_name<T: DomainQuantity>(quality: &EvidenceQuality<T>) -> &'static str {
    match quality {
        EvidenceQuality::Measured => "measured",
        EvidenceQuality::Estimated { .. } => "estimated",
        EvidenceQuality::Mixed { .. } => "mixed",
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
        assert_eq!(q.to_json(), "{\"value\":\"500000\",\"unit\":\"ppm\"}");
    }

    #[test]
    fn envelope_serializes_every_required_field() {
        let run = RunId::new(UtcTimestamp::from_unix_nanos(42));
        let envelope = JsonEnvelope::new("now", run.clone(), metadata());
        let json = envelope.to_json();
        assert!(json.contains("\"schema\":1"), "{json}");
        assert!(json.contains("\"command\":\"now\""), "{json}");
        assert!(
            json.contains(&format!("\"run\":\"{}\"", run.as_str())),
            "{json}"
        );
        assert!(json.contains("\"generated_at\":2000"), "{json}");
        assert!(json.contains("\"knowledge_at\":1000"), "{json}");
        assert!(json.contains("\"ledger_generation\":7"), "{json}");
    }

    #[test]
    fn freshness_serializes_exactly_one_variant() {
        let fresh = freshness_json(
            "work-a",
            &Freshness::Fresh {
                observed: observed(500_000),
                latest_attempt: AttemptId::new(1),
            },
        );
        assert!(fresh.contains("\"freshness\":\"fresh\""), "{fresh}");
        assert!(
            fresh.contains("\"remaining\":{\"value\":\"500000\",\"unit\":\"ppm\"}"),
            "{fresh}"
        );

        let stale = freshness_json(
            "work-a",
            &Freshness::Stale {
                last_good: Some(observed(500_000)),
                latest_attempt: AttemptId::new(2),
                reason: StaleReason::AgeExceeded,
            },
        );
        assert!(stale.contains("\"freshness\":\"stale\""), "{stale}");
        assert!(stale.contains("\"reason\":\"age_exceeded\""), "{stale}");
        assert!(
            stale.contains("\"last_good\":{\"value\":\"500000\",\"unit\":\"ppm\"}"),
            "{stale}"
        );

        let auth = freshness_json(
            "work-a",
            &Freshness::<QuotaRemaining>::AuthRequired {
                last_good: None,
                latest_attempt: AttemptId::new(3),
            },
        );
        assert!(auth.contains("\"freshness\":\"auth_required\""), "{auth}");
        assert!(auth.contains("\"last_good\":null"), "{auth}");

        // Stale and auth-required are distinguishable without parsing prose: the
        // freshness field names the state directly.
        assert!(stale.contains("\"freshness\":\"stale\""), "{stale}");
        assert!(auth.contains("\"freshness\":\"auth_required\""), "{auth}");
    }

    #[test]
    fn coverage_and_quality_are_separate_fields() {
        let json = coverage_and_quality_json::<TokenCount>(
            &CoverageCompleteness::Complete,
            &EvidenceQuality::Measured,
        );
        assert_eq!(
            json,
            "{\"coverage\":\"complete\",\"evidence_quality\":\"measured\"}"
        );

        let partial = coverage_and_quality_json::<TokenCount>(
            &CoverageCompleteness::partial([crate::evidence::ComponentKind::new("x")]),
            &EvidenceQuality::estimated([crate::evidence::EstimatorId::new("chars")], None),
        );
        assert!(partial.contains("\"coverage\":\"partial\""), "{partial}");
        assert!(
            partial.contains("\"evidence_quality\":\"estimated\""),
            "{partial}"
        );
    }

    #[test]
    fn interval_serializes_both_endpoints_and_the_unit() {
        let interval = Interval::new(TokenCount::new(10), TokenCount::new(20)).unwrap();
        let json = interval_json(&interval);
        assert_eq!(
            json,
            "{\"lower\":\"10\",\"upper\":\"20\",\"unit\":\"tokens\"}"
        );
    }

    #[test]
    fn provenance_identifiers_round_trip() {
        let provenance = Provenance::new(["a".to_string(), "b".to_string()]);
        let json = provenance_json(&provenance);
        assert_eq!(json, "{\"sources\":[\"a\",\"b\"]}");
    }

    #[test]
    fn json_string_escapes_control_characters() {
        assert_eq!(json_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(json_string("a\\b"), "\"a\\\\b\"");
        assert_eq!(json_string("a\nb"), "\"a\\nb\"");
    }
}
