//! Parsing and validation of the legacy regression-fit source.
//!
//! May not depend on:
//! - provider adapters directly
//! - presentation
//!
//! The legacy fit is real evidence about a real experiment and the direct
//! ancestor of the defect this project exists to fix: its cost model omitted
//! cache-write billing. The source format therefore carries both the fitted
//! coefficient and the experiment provenance that distinguishes a measured
//! fit from a hardcoded copy. A source that parses but names no experiment
//! is refused as a hardcoded copy rather than guessed into history.

use std::path::Path;

use sha2::{Digest, Sha256};

use crate::domain::time::UtcTimestamp;
use crate::error::Error;

/// The one source format this importer accepts.
pub const LEGACY_CALIBRATION_FORMAT: &str = "legacy-calibration-v1";

/// Experiment provenance a fit-evidence source must carry: the experiment
/// the fit was measured from and the evidence it consumed. A hardcoded copy
/// carries the same coefficient with none of this, which is why its absence
/// refuses the import rather than quarantining a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyFitExperiment {
    pub experiment_id: String,
    pub method: String,
    pub evidence_ids: Vec<String>,
}

/// Provenance the original regression fit carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyFitProvenance {
    pub origin: String,
    pub note: String,
}

/// One validated legacy fit: the coefficient, its date and whatever
/// provenance the original has, plus the experiment evidence that makes it
/// importable rather than a hardcoded copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyCalibrationFit {
    pub calibration_id: String,
    pub provider: String,
    pub plan_tier: String,
    pub window_semantic_key: String,
    pub fitted_micros_per_point: i64,
    pub fit_timestamp: UtcTimestamp,
    pub provenance: LegacyFitProvenance,
    pub experiment: LegacyFitExperiment,
}

/// A parsed legacy-calibration source: one record at most, identified by its
/// content digest rather than by path. A malformed document quarantines
/// rather than guessing a coefficient into history; a well-formed document
/// that lacks experiment evidence is refused outright as a hardcoded copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLegacyCalibrationSource {
    pub content_digest: String,
    pub records_read: u64,
    pub records_quarantined: u64,
    pub record: Option<LegacyCalibrationFit>,
}

/// Reads the legacy-calibration source at `path`.
///
/// File-system and encoding failures are errors. A document that is not a
/// well-formed fit record quarantines. A document that is well-formed except
/// for missing experiment evidence is refused as a hardcoded copy, because
/// the coefficient alone is not evidence.
pub fn read_source(path: &Path) -> Result<ParsedLegacyCalibrationSource, Error> {
    let bytes = std::fs::read(path).map_err(|error| {
        Error::IngestIncomplete(format!("cannot read legacy calibration source: {error}"))
    })?;
    let content_digest = sha256_hex(&bytes);
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| Error::IngestIncomplete("legacy calibration source is not UTF-8".into()))?;
    if text.trim().is_empty() {
        return Ok(ParsedLegacyCalibrationSource {
            content_digest,
            records_read: 0,
            records_quarantined: 0,
            record: None,
        });
    }
    match parse_document(text) {
        Ok(fit) => Ok(ParsedLegacyCalibrationSource {
            content_digest,
            records_read: 1,
            records_quarantined: 0,
            record: Some(fit),
        }),
        Err(ParseOutcome::Quarantined) => Ok(ParsedLegacyCalibrationSource {
            content_digest,
            records_read: 1,
            records_quarantined: 1,
            record: None,
        }),
        Err(ParseOutcome::HardcodedCopy(reason)) => Err(Error::IngestIncomplete(reason)),
    }
}

enum ParseOutcome {
    Quarantined,
    HardcodedCopy(String),
}

impl From<ParseOutcome> for Error {
    fn from(_: ParseOutcome) -> Self {
        Error::IngestIncomplete("legacy calibration source is malformed".into())
    }
}

fn parse_document(text: &str) -> Result<LegacyCalibrationFit, ParseOutcome> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| ParseOutcome::Quarantined)?;
    let object = value.as_object().ok_or(ParseOutcome::Quarantined)?;

    let format = object
        .get("format")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if format != LEGACY_CALIBRATION_FORMAT {
        return Err(ParseOutcome::Quarantined);
    }

    let calibration_id = non_empty(
        object
            .get("calibration_id")
            .and_then(serde_json::Value::as_str),
    )
    .ok_or(ParseOutcome::Quarantined)?;
    let provider = non_empty(object.get("provider").and_then(serde_json::Value::as_str))
        .ok_or(ParseOutcome::Quarantined)?;
    let plan_tier = non_empty(object.get("plan_tier").and_then(serde_json::Value::as_str))
        .ok_or(ParseOutcome::Quarantined)?;
    let window = non_empty(object.get("window").and_then(serde_json::Value::as_str))
        .ok_or(ParseOutcome::Quarantined)?;

    let fitted = object
        .get("fitted_micros_per_point")
        .ok_or(ParseOutcome::Quarantined)?;
    let fitted_micros = fitted.as_i64().ok_or(ParseOutcome::Quarantined)?;
    if fitted_micros <= 0 {
        return Err(ParseOutcome::Quarantined);
    }

    let timestamp_raw = object
        .get("fit_timestamp")
        .and_then(serde_json::Value::as_str)
        .ok_or(ParseOutcome::Quarantined)?;
    let fit_timestamp =
        UtcTimestamp::parse_rfc3339(timestamp_raw).ok_or(ParseOutcome::Quarantined)?;

    let provenance_value = object.get("provenance").ok_or(ParseOutcome::Quarantined)?;
    let provenance_object = provenance_value
        .as_object()
        .ok_or(ParseOutcome::Quarantined)?;
    let origin = non_empty(
        provenance_object
            .get("origin")
            .and_then(serde_json::Value::as_str),
    )
    .ok_or(ParseOutcome::Quarantined)?;
    let note = provenance_object
        .get("note")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();

    let experiment_value = object.get("experiment");
    let Some(experiment_object) = experiment_value.and_then(serde_json::Value::as_object) else {
        return Err(ParseOutcome::HardcodedCopy(format!(
            "refused hardcoded copy of calibration '{calibration_id}': \
             it carries the fitted coefficient but no experiment evidence; \
             only a fit measured from recorded experiment evidence imports as history"
        )));
    };
    let experiment_id = non_empty(
        experiment_object
            .get("experiment_id")
            .and_then(serde_json::Value::as_str),
    );
    let Some(experiment_id) = experiment_id else {
        return Err(ParseOutcome::HardcodedCopy(format!(
            "refused hardcoded copy of calibration '{calibration_id}': \
             it carries the fitted coefficient but no experiment evidence; \
             only a fit measured from recorded experiment evidence imports as history"
        )));
    };
    let evidence_ids = experiment_object
        .get("evidence_ids")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if evidence_ids.is_empty() {
        return Err(ParseOutcome::HardcodedCopy(format!(
            "refused hardcoded copy of calibration '{calibration_id}': \
             experiment '{experiment_id}' names no evidence; \
             only a fit measured from recorded experiment evidence imports as history"
        )));
    }
    let method = experiment_object
        .get("method")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("ordinary-least-squares")
        .to_string();

    Ok(LegacyCalibrationFit {
        calibration_id,
        provider,
        plan_tier,
        window_semantic_key: window,
        fitted_micros_per_point: fitted_micros,
        fit_timestamp,
        provenance: LegacyFitProvenance { origin, note },
        experiment: LegacyFitExperiment {
            experiment_id,
            method,
            evidence_ids,
        },
    })
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
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

    fn fit_json(calibration_id: &str, fitted: i64, with_experiment: bool) -> String {
        let experiment = if with_experiment {
            r#","experiment":{"experiment_id":"legacy-fit-evidence-1","method":"ordinary-least-squares","evidence_ids":["legacy:obs-1","legacy:obs-2"]}"#.to_string()
        } else {
            String::new()
        };
        format!(
            r#"{{"format":"legacy-calibration-v1","calibration_id":"{calibration_id}","provider":"anthropic","plan_tier":"default","window":"five_hour","fitted_micros_per_point":{fitted},"fit_timestamp":"2026-07-01T00:00:00Z","provenance":{{"origin":"legacy-regression-fit","note":"pre-rewrite regression"}} {experiment}}}"#
        )
    }

    #[test]
    fn fit_evidence_source_parses_with_coefficient_date_and_provenance() {
        let dir = std::env::temp_dir().join(format!("aub-legacy-cal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fit.json");
        std::fs::write(&path, fit_json("legacy-fit-1", 480_000, true)).unwrap();
        let parsed = read_source(&path).unwrap();
        assert_eq!(parsed.records_read, 1);
        assert_eq!(parsed.records_quarantined, 0);
        let record = parsed.record.unwrap();
        assert_eq!(record.calibration_id, "legacy-fit-1");
        assert_eq!(record.fitted_micros_per_point, 480_000);
        assert_eq!(
            record.fit_timestamp,
            UtcTimestamp::parse_rfc3339("2026-07-01T00:00:00Z").unwrap()
        );
        assert_eq!(record.provenance.origin, "legacy-regression-fit");
        assert_eq!(record.experiment.experiment_id, "legacy-fit-evidence-1");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hardcoded_copy_with_identical_coefficient_is_refused_for_missing_experiment() {
        let dir = std::env::temp_dir().join(format!("aub-legacy-cal-{}.1", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("copy.json");
        std::fs::write(&path, fit_json("legacy-fit-1", 480_000, false)).unwrap();
        let error = read_source(&path).unwrap_err();
        assert!(
            error.to_string().contains("hardcoded copy"),
            "unexpected: {error}"
        );
        assert!(
            error.to_string().contains("experiment evidence"),
            "unexpected: {error}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn malformed_document_quarantines_rather_than_guessing_a_coefficient() {
        let dir = std::env::temp_dir().join(format!("aub-legacy-cal-{}.2", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.json");
        std::fs::write(&path, "not-json").unwrap();
        let parsed = read_source(&path).unwrap();
        assert_eq!(parsed.records_read, 1);
        assert_eq!(parsed.records_quarantined, 1);
        assert!(parsed.record.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
