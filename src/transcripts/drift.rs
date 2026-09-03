//! Transcript format drift detection against the live corpus (aub-lqe.17).
//!
//! Compares record shapes, field sets, record kinds and evidence classifications
//! discovered in live transcripts against each parser's committed fixture corpus.
//! Reports any uncovered shape, field or evidence class alongside quarantine counts
//! per parser and failure class, without exposing transcript text content.
//!
//! May not depend on:
//! - calibration
//! - cost models
//! - subscription window capacity, API pricing, task history, or meter percentages

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::domain::time::UtcTimestamp;
use crate::error::Error;
use crate::report::{LedgerGeneration, ReportMetadata};
use crate::transcripts::discovery::{DiscoveryOptions, discover};
use crate::transcripts::parser::{ParserVersion, SourceLocation};
use crate::transcripts::parser_for_format;

/// JSON structural value type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ValueType {
    Null,
    Bool,
    Integer,
    Float,
    String,
    Array,
    Object,
}

impl ValueType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool => "bool",
            Self::Integer => "integer",
            Self::Float => "float",
            Self::String => "string",
            Self::Array => "array",
            Self::Object => "object",
        }
    }
}

/// Structural representation of a single transcript record.
/// Contains only schema metadata (paths, types, kinds, hash); no string values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordShape {
    pub shape_hash: String,
    pub record_kind: Option<String>,
    pub field_paths: BTreeSet<String>,
    pub occurrence_count: u64,
}

/// Compact shape summary for reports and JSON envelopes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShapeSummary {
    pub shape_hash: String,
    pub record_kind: Option<String>,
    pub field_count: usize,
    pub occurrence_count: u64,
}

impl From<&RecordShape> for ShapeSummary {
    fn from(shape: &RecordShape) -> Self {
        Self {
            shape_hash: shape.shape_hash.clone(),
            record_kind: shape.record_kind.clone(),
            field_count: shape.field_paths.len(),
            occurrence_count: shape.occurrence_count,
        }
    }
}

/// Aggregated structural shape of a parser's committed fixture corpus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixtureCorpusShape {
    pub format: String,
    pub shapes: BTreeMap<String, RecordShape>,
    pub fields: BTreeSet<String>,
    pub record_kinds: BTreeSet<String>,
    pub evidence_classes: BTreeSet<String>,
}

/// Drift detection report for one configured transcript source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDriftReport {
    pub source: String,
    pub format: String,
    pub parser_version: ParserVersion,
    pub files_scanned: usize,
    pub records_scanned: usize,
    pub quarantined_records: u64,
    pub quarantine_by_class: BTreeMap<String, u64>,
    pub shapes_seen: Vec<ShapeSummary>,
    pub uncovered_fields: BTreeSet<String>,
    pub uncovered_record_kinds: BTreeSet<String>,
    pub uncovered_evidence_classes: BTreeSet<String>,
    pub uncovered_shapes: Vec<ShapeSummary>,
    pub drift_detected: bool,
    pub remediation: Option<String>,
}

/// The complete transcript format drift report for `aub doctor --transcript-format-drift`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptDriftReport {
    pub metadata: ReportMetadata,
    pub has_configured_roots: bool,
    pub sources: Vec<SourceDriftReport>,
    pub overall_drift_detected: bool,
    pub remediation: Option<String>,
}

/// The documented fixture capture and sanitization procedure reference.
pub const FIXTURE_CAPTURE_PROCEDURE_DOC: &str = "tests/fixtures/transcripts/README.md";

/// Default fixture directory relative to the crate root.
pub const DEFAULT_FIXTURE_DIR: &str = "tests/fixtures/transcripts/native";

/// Recursively traverses a JSON value to collect all field paths and their types.
pub fn collect_field_paths(value: &Value, prefix: &str, fields: &mut BTreeMap<String, ValueType>) {
    match value {
        Value::Null => {
            if !prefix.is_empty() {
                fields.insert(prefix.to_string(), ValueType::Null);
            }
        }
        Value::Bool(_) => {
            if !prefix.is_empty() {
                fields.insert(prefix.to_string(), ValueType::Bool);
            }
        }
        Value::Number(n) => {
            if !prefix.is_empty() {
                let vt = if n.is_u64() || n.is_i64() {
                    ValueType::Integer
                } else {
                    ValueType::Float
                };
                fields.insert(prefix.to_string(), vt);
            }
        }
        Value::String(_) => {
            if !prefix.is_empty() {
                fields.insert(prefix.to_string(), ValueType::String);
            }
        }
        Value::Array(items) => {
            if !prefix.is_empty() {
                fields.insert(prefix.to_string(), ValueType::Array);
            }
            for item in items {
                let item_prefix = if prefix.is_empty() {
                    "[]".to_string()
                } else {
                    format!("{prefix}[]")
                };
                collect_field_paths(item, &item_prefix, fields);
            }
        }
        Value::Object(map) => {
            if !prefix.is_empty() {
                fields.insert(prefix.to_string(), ValueType::Object);
            }
            for (k, v) in map {
                let child_prefix = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                collect_field_paths(v, &child_prefix, fields);
            }
        }
    }
}

/// Extracts the record kind/type discriminator from a JSON record if present.
pub fn extract_record_kind(value: &Value) -> Option<String> {
    if let Some(kind) = value.get("type").and_then(Value::as_str) {
        if kind == "event_msg" {
            let payload_type = value
                .get("payload")
                .and_then(|p| p.get("type"))
                .and_then(Value::as_str);
            if let Some(pt) = payload_type {
                return Some(pt.to_string());
            }
        }
        return Some(kind.to_string());
    }
    value
        .get("payload")
        .and_then(|p| p.get("type"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

/// Computes the structural shape of a single JSON record.
/// Purely structural: field names and types only, no string value contents.
pub fn extract_record_shape(value: &Value) -> RecordShape {
    let mut field_types = BTreeMap::new();
    collect_field_paths(value, "", &mut field_types);

    let mut sig_lines = Vec::with_capacity(field_types.len());
    let mut field_paths = BTreeSet::new();
    for (path, vt) in &field_types {
        field_paths.insert(path.clone());
        sig_lines.push(format!("{path}:{}", vt.as_str()));
    }
    sig_lines.sort();

    let mut hasher = Sha256::new();
    for line in &sig_lines {
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
    }
    let shape_hash = format!("{:x}", hasher.finalize());
    let record_kind = extract_record_kind(value);

    RecordShape {
        shape_hash,
        record_kind,
        field_paths,
        occurrence_count: 1,
    }
}

/// Resolves the base fixture directory: the caller's explicit directory, or
/// the default relative path.
///
/// This used to fall back to `option_env!("CARGO_MANIFEST_DIR")` joined onto
/// the default path. That macro bakes the absolute path of whoever's checkout
/// built the binary into a compile-time string constant, present in every
/// release binary's `.rodata` regardless of whether this branch ever runs
/// (aub-n27.4: the identity/privacy scan is what caught it). It bought
/// nothing production needs: `cargo test` already runs with the package root
/// as its working directory, so the relative path alone resolves the fixture
/// corpus correctly for every test that calls this with `custom: None`, and a
/// released binary never ships that corpus at all.
fn resolve_fixture_base(custom: Option<&Path>) -> PathBuf {
    custom
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_FIXTURE_DIR))
}

/// Loads and extracts all structural shapes from the fixture corpus for a specific format.
pub fn load_fixture_corpus(
    format: &str,
    fixture_dir: Option<&Path>,
) -> Result<FixtureCorpusShape, Error> {
    let base_dir = resolve_fixture_base(fixture_dir);
    let parser = parser_for_format(format)
        .ok_or_else(|| Error::Usage(format!("unsupported transcript format: {format}")))?;

    let mut all_shapes: BTreeMap<String, RecordShape> = BTreeMap::new();
    let mut all_fields: BTreeSet<String> = BTreeSet::new();
    let mut all_record_kinds: BTreeSet<String> = BTreeSet::new();
    let mut all_evidence_classes: BTreeSet<String> = BTreeSet::new();

    // First, check declared fixtures for the format
    let manifest_path = base_dir.join("MANIFEST.json");
    let mut fixture_files = Vec::new();
    let format_entry_opt = fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|c| serde_json::from_str::<Value>(&c).ok())
        .and_then(|manifest| manifest.get("formats")?.get(format)?.as_object().cloned());
    if let Some(format_entry) = format_entry_opt {
        if let Some(real_capture) = format_entry.get("real_capture").and_then(Value::as_str) {
            fixture_files.push(base_dir.join(real_capture));
        }
        if let Some(shapes) = format_entry.get("shapes").and_then(Value::as_object) {
            for entry in shapes.values() {
                if let Some(fixture) = entry.get("fixture").and_then(Value::as_str) {
                    fixture_files.push(base_dir.join(fixture));
                }
            }
        }
    }

    // If no manifest entries found, scan directory for <format>-*.jsonl
    if fixture_files.is_empty() {
        let entries_res = fs::read_dir(&base_dir);
        if let Ok(entries) = entries_res {
            for entry in entries.flatten() {
                let path = entry.path();
                let matches_format = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| name.starts_with(format) && name.ends_with(".jsonl"));
                if matches_format {
                    fixture_files.push(path);
                }
            }
        }
    }

    for file_path in &fixture_files {
        if !file_path.exists() {
            continue;
        }
        let content = fs::read_to_string(file_path).map_err(|e| {
            Error::Store(format!(
                "cannot read fixture file {}: {e}",
                file_path.display()
            ))
        })?;

        let location = SourceLocation::new(file_path.to_string_lossy(), 1);
        let parse_out = parser.parse(&content, &location);
        for event in parse_out.events() {
            let class_name = match event.classification() {
                crate::transcripts::EvidenceClassification::Reported => "reported".to_string(),
                crate::transcripts::EvidenceClassification::Derived => "derived".to_string(),
                crate::transcripts::EvidenceClassification::Reconstructed {
                    estimator,
                    version,
                } => {
                    format!("reconstructed:{}:{}", estimator.as_str(), version.as_str())
                }
            };
            all_evidence_classes.insert(class_name);
        }

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(val) = serde_json::from_str::<Value>(line) {
                let shape = extract_record_shape(&val);
                for fp in &shape.field_paths {
                    all_fields.insert(fp.clone());
                }
                if let Some(ref rk) = shape.record_kind {
                    all_record_kinds.insert(rk.clone());
                }
                all_shapes
                    .entry(shape.shape_hash.clone())
                    .and_modify(|s| s.occurrence_count += 1)
                    .or_insert(shape);
            }
        }
    }

    Ok(FixtureCorpusShape {
        format: format.to_string(),
        shapes: all_shapes,
        fields: all_fields,
        record_kinds: all_record_kinds,
        evidence_classes: all_evidence_classes,
    })
}

/// Detects transcript format drift by scanning all configured transcript roots and
/// comparing seen shapes against committed fixture shapes.
pub fn detect_drift(
    config: &Config,
    fixture_dir: Option<&Path>,
    generated_at: UtcTimestamp,
    db_quarantine_summary: Option<&[crate::store::ingest_quarantine::QuarantineSummaryGroup]>,
) -> Result<TranscriptDriftReport, Error> {
    if config.transcripts.is_empty() {
        return Ok(TranscriptDriftReport {
            metadata: ReportMetadata::new(
                generated_at,
                generated_at,
                LedgerGeneration::new(0),
                None,
            ),
            has_configured_roots: false,
            sources: Vec::new(),
            overall_drift_detected: false,
            remediation: None,
        });
    }

    let mut source_reports = Vec::new();
    let mut overall_drift = false;

    for source_cfg in &config.transcripts {
        let format_name = source_cfg.format.as_deref().unwrap_or(&source_cfg.name);
        let parser = match parser_for_format(format_name) {
            Some(p) => p,
            None => {
                return Err(Error::Usage(format!(
                    "source {} declared format {:?} which has no parser adapter",
                    source_cfg.name, source_cfg.format
                )));
            }
        };

        let fixture_corpus = load_fixture_corpus(format_name, fixture_dir)?;

        let discovery = discover(
            std::slice::from_ref(source_cfg),
            &DiscoveryOptions::default(),
        )
        .map_err(|e| {
            Error::Store(format!(
                "discovery failed for source {}: {e:?}",
                source_cfg.name
            ))
        })?;

        let mut files_scanned = 0;
        let mut records_scanned = 0;
        let mut live_shapes: BTreeMap<String, RecordShape> = BTreeMap::new();
        let mut live_fields: BTreeSet<String> = BTreeSet::new();
        let mut live_record_kinds: BTreeSet<String> = BTreeSet::new();
        let mut live_evidence_classes: BTreeSet<String> = BTreeSet::new();
        let mut quarantine_by_class: BTreeMap<String, u64> = BTreeMap::new();
        let mut quarantined_records = 0u64;

        for src_disc in &discovery {
            for file_path in &src_disc.files {
                files_scanned += 1;
                let content = match fs::read_to_string(file_path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                let location = SourceLocation::new(file_path.to_string_lossy(), 1);
                let parse_out = parser.parse(&content, &location);

                for event in parse_out.events() {
                    let class_name = match event.classification() {
                        crate::transcripts::EvidenceClassification::Reported => {
                            "reported".to_string()
                        }
                        crate::transcripts::EvidenceClassification::Derived => {
                            "derived".to_string()
                        }
                        crate::transcripts::EvidenceClassification::Reconstructed {
                            estimator,
                            version,
                        } => {
                            format!("reconstructed:{}:{}", estimator.as_str(), version.as_str())
                        }
                    };
                    live_evidence_classes.insert(class_name);
                }

                for q in parse_out.quarantined() {
                    quarantined_records += 1;
                    *quarantine_by_class
                        .entry(q.class().name().to_string())
                        .or_insert(0) += 1;
                }

                for line in content.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    records_scanned += 1;
                    if let Ok(val) = serde_json::from_str::<Value>(line) {
                        let shape = extract_record_shape(&val);
                        for fp in &shape.field_paths {
                            live_fields.insert(fp.clone());
                        }
                        if let Some(ref rk) = shape.record_kind {
                            live_record_kinds.insert(rk.clone());
                        }
                        live_shapes
                            .entry(shape.shape_hash.clone())
                            .and_modify(|s| s.occurrence_count += 1)
                            .or_insert(shape);
                    }
                }
            }
        }

        // Include any DB-tracked quarantine counts if provided
        if let Some(db_summary) = db_quarantine_summary {
            for grp in db_summary {
                if grp.parser == parser.parser_version().as_str() || grp.parser == format_name {
                    *quarantine_by_class
                        .entry(grp.failure_class.clone())
                        .or_insert(0) += grp.count;
                    quarantined_records += grp.count;
                }
            }
        }

        // Compute uncovered items
        let uncovered_fields: BTreeSet<String> = live_fields
            .difference(&fixture_corpus.fields)
            .cloned()
            .collect();

        let uncovered_record_kinds: BTreeSet<String> = live_record_kinds
            .difference(&fixture_corpus.record_kinds)
            .cloned()
            .collect();

        let uncovered_evidence_classes: BTreeSet<String> = live_evidence_classes
            .difference(&fixture_corpus.evidence_classes)
            .cloned()
            .collect();

        let mut uncovered_shapes = Vec::new();
        let mut shapes_seen = Vec::new();

        for (hash, shape) in &live_shapes {
            let summary = ShapeSummary::from(shape);
            shapes_seen.push(summary.clone());
            if !fixture_corpus.shapes.contains_key(hash) {
                uncovered_shapes.push(summary);
            }
        }

        let drift_detected = !uncovered_fields.is_empty()
            || !uncovered_record_kinds.is_empty()
            || !uncovered_evidence_classes.is_empty()
            || !uncovered_shapes.is_empty()
            || quarantined_records > 0;

        if drift_detected {
            overall_drift = true;
        }

        let remediation = if drift_detected {
            Some(format!(
                "Capture and sanitize a new fixture following the procedure in {FIXTURE_CAPTURE_PROCEDURE_DOC}"
            ))
        } else {
            None
        };

        source_reports.push(SourceDriftReport {
            source: source_cfg.name.clone(),
            format: format_name.to_string(),
            parser_version: parser.parser_version(),
            files_scanned,
            records_scanned,
            quarantined_records,
            quarantine_by_class,
            shapes_seen,
            uncovered_fields,
            uncovered_record_kinds,
            uncovered_evidence_classes,
            uncovered_shapes,
            drift_detected,
            remediation,
        });
    }

    let overall_remediation = if overall_drift {
        Some(format!(
            "Capture and sanitize new fixtures following the procedure in {FIXTURE_CAPTURE_PROCEDURE_DOC}"
        ))
    } else {
        None
    };

    Ok(TranscriptDriftReport {
        metadata: ReportMetadata::new(generated_at, generated_at, LedgerGeneration::new(0), None),
        has_configured_roots: true,
        sources: source_reports,
        overall_drift_detected: overall_drift,
        remediation: overall_remediation,
    })
}
