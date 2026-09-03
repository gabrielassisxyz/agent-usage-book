//! Parsing and normalization of the pre-`aub` quota ledger.
//!
//! The old JSONL series records hook-time timestamps. Its records are useful
//! evidence, but they are not provider-observed samples and must retain that
//! distinction when they enter the durable ledger.

use std::path::Path;

use sha2::{Digest, Sha256};

use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
use crate::domain::time::{MeasurementBasis, UtcTimestamp};
use crate::error::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyWindow {
    pub semantic_key: &'static str,
    pub quota_used: QuotaUsed,
    pub resets_at: UtcTimestamp,
    pub nominal_duration_nanos: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyMeterRecord {
    pub source_line: u64,
    pub timestamp: UtcTimestamp,
    pub measurement_basis: MeasurementBasis,
    pub session_id: String,
    pub account: String,
    pub tier: Option<String>,
    pub windows: Vec<LegacyWindow>,
    pub evidence_capsule: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLegacyMeterSource {
    pub content_digest: String,
    pub records: Vec<LegacyMeterRecord>,
    pub records_read: u64,
    pub records_quarantined: u64,
}

/// Reads the legacy JSONL format. A malformed line is quarantined from this
/// import rather than guessed into an observation; a later corrected source
/// has a different digest and can be imported deliberately.
pub fn read_source(path: &Path) -> Result<ParsedLegacyMeterSource, Error> {
    let bytes = std::fs::read(path).map_err(|error| {
        Error::IngestIncomplete(format!("cannot read legacy meter source: {error}"))
    })?;
    let content_digest = sha256_hex(&bytes);
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| Error::IngestIncomplete("legacy meter source is not UTF-8 JSONL".into()))?;
    let mut records = Vec::new();
    let mut records_read = 0;
    let mut records_quarantined = 0;
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        records_read += 1;
        match parse_record(index as u64 + 1, line) {
            Some(record) => records.push(record),
            None => records_quarantined += 1,
        }
    }
    Ok(ParsedLegacyMeterSource {
        content_digest,
        records,
        records_read,
        records_quarantined,
    })
}

fn parse_record(source_line: u64, line: &str) -> Option<LegacyMeterRecord> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let object = value.as_object()?;
    let timestamp = UtcTimestamp::parse_rfc3339(object.get("ts")?.as_str()?)?;
    let session_id = non_empty(object.get("session_id")?.as_str()?)?;
    let account = non_empty(object.get("account")?.as_str()?)?;
    let tier = object
        .get("tier")
        .and_then(serde_json::Value::as_str)
        .and_then(non_empty);
    let measurement_basis = match object
        .get("timestamp_kind")
        .and_then(serde_json::Value::as_str)
    {
        Some("provider_observed") => MeasurementBasis::ProviderObserved,
        Some("older_of_the_two") => MeasurementBasis::OlderOfTheTwo,
        // The old ledger writes its hook timestamp as `ts`; its lack of an
        // explicit kind must never upgrade it to a provider timestamp.
        Some("hook_time") | Some("locally_received") | None => MeasurementBasis::LocallyReceived,
        Some(_) => return None,
    };
    let five = window(
        object,
        "five_hour",
        "five_resets_at",
        "five_hour",
        5 * 60 * 60 * 1_000_000_000,
    )?;
    let seven = window(
        object,
        "seven_day",
        "seven_resets_at",
        "seven_day",
        7 * 24 * 60 * 60 * 1_000_000_000,
    )?;
    let evidence_capsule = serde_json::json!({
        "format": "quota-ledger-jsonl-v1",
        "timestamp": object.get("ts")?,
        "session_id": session_id,
        "account": account,
        "tier": tier,
        "five_hour": object.get("five_hour")?,
        "seven_day": object.get("seven_day")?,
        "five_resets_at": object.get("five_resets_at")?,
        "seven_resets_at": object.get("seven_resets_at")?,
        "timestamp_kind": object.get("timestamp_kind"),
    })
    .to_string();
    Some(LegacyMeterRecord {
        source_line,
        timestamp,
        measurement_basis,
        session_id,
        account,
        tier,
        windows: vec![five, seven],
        evidence_capsule,
    })
}

fn window(
    object: &serde_json::Map<String, serde_json::Value>,
    percentage_key: &str,
    reset_key: &str,
    semantic_key: &'static str,
    nominal_duration_nanos: u64,
) -> Option<LegacyWindow> {
    let used = percentage_to_used(object.get(percentage_key)?)?;
    let resets_at = parse_reset_timestamp(object.get(reset_key)?)?;
    Some(LegacyWindow {
        semantic_key,
        quota_used: used,
        resets_at,
        nominal_duration_nanos,
    })
}

fn parse_reset_timestamp(value: &serde_json::Value) -> Option<UtcTimestamp> {
    if let Some(text) = value.as_str() {
        if let Some(ts) = UtcTimestamp::parse_rfc3339(text) {
            return Some(ts);
        }
        if let Ok(secs) = text.parse::<i64>() {
            return Some(UtcTimestamp::from_unix_nanos(
                secs.checked_mul(1_000_000_000)?,
            ));
        }
        return None;
    }
    if let Some(secs) = value.as_i64() {
        return Some(UtcTimestamp::from_unix_nanos(
            secs.checked_mul(1_000_000_000)?,
        ));
    }
    None
}

fn percentage_to_used(value: &serde_json::Value) -> Option<QuotaUsed> {
    let percentage = value.as_f64()?;
    if !percentage.is_finite() {
        return None;
    }
    // The legacy source itself used binary floating point, so its spelling can
    // contain 28.000000000000004 for an exact displayed 28%. Round only to
    // the source's documented one-percent resolution before constructing the
    // closed quota type.
    let percentage = percentage.round();
    (0.0..=100.0).contains(&percentage).then_some(())?;
    let ppm = (percentage as i32).checked_mul(10_000)?;
    QuotaFractionPpm::new(ppm).map(QuotaUsed::new)
}

fn non_empty(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.to_owned())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_time_is_locally_received_and_float_noise_rounds_to_the_displayed_percent() {
        let record = parse_record(1, r#"{"ts":"2026-08-15T18:40:38Z","session_id":"s","account":"a","five_hour":28.000000000000004,"seven_day":44,"five_resets_at":"2026-08-15T20:00:00Z","seven_resets_at":"2026-08-22T00:00:00Z"}"#).unwrap();
        assert_eq!(record.measurement_basis, MeasurementBasis::LocallyReceived);
        assert_eq!(record.windows[0].quota_used.as_ppm().get(), 280_000);
    }

    #[test]
    fn measurement_basis_assigned_per_timestamp_kind() {
        let make_json = |kind: Option<&str>| {
            let kind_field = match kind {
                Some(k) => format!(r#","timestamp_kind":"{k}""#),
                None => String::new(),
            };
            format!(
                r#"{{"ts":"2026-08-15T18:40:38Z","session_id":"s","account":"a","five_hour":28,"seven_day":44,"five_resets_at":"2026-08-15T20:00:00Z","seven_resets_at":"2026-08-22T00:00:00Z"{kind_field}}}"#
            )
        };
        assert_eq!(
            parse_record(1, &make_json(Some("provider_observed")))
                .unwrap()
                .measurement_basis,
            MeasurementBasis::ProviderObserved,
        );
        assert_eq!(
            parse_record(1, &make_json(Some("older_of_the_two")))
                .unwrap()
                .measurement_basis,
            MeasurementBasis::OlderOfTheTwo,
        );
        assert_eq!(
            parse_record(1, &make_json(Some("hook_time")))
                .unwrap()
                .measurement_basis,
            MeasurementBasis::LocallyReceived,
        );
        assert_eq!(
            parse_record(1, &make_json(Some("locally_received")))
                .unwrap()
                .measurement_basis,
            MeasurementBasis::LocallyReceived,
        );
        assert_eq!(
            parse_record(1, &make_json(None)).unwrap().measurement_basis,
            MeasurementBasis::LocallyReceived,
        );
        assert!(parse_record(1, &make_json(Some("unknown_kind"))).is_none());
    }

    #[test]
    fn epoch_second_resets_are_parsed_correctly() {
        let record = parse_record(
            1,
            r#"{"ts":"2026-08-15T18:23:29Z","session_id":"s","account":"a","five_hour":7,"seven_day":63,"five_resets_at":"1786834200","seven_resets_at":1787148000}"#,
        )
        .unwrap();
        assert_eq!(
            record.windows[0].resets_at,
            UtcTimestamp::from_unix_nanos(1_786_834_200 * 1_000_000_000)
        );
        assert_eq!(
            record.windows[1].resets_at,
            UtcTimestamp::from_unix_nanos(1_787_148_000 * 1_000_000_000)
        );
    }

    #[test]
    fn a_missing_reset_is_quarantined_instead_of_inventing_one() {
        assert!(parse_record(1, r#"{"ts":"2026-08-15T18:40:38Z","session_id":"s","account":"a","five_hour":28,"seven_day":44,"five_resets_at":null,"seven_resets_at":"2026-08-22T00:00:00Z"}"#).is_none());
    }
}
