//! Parser and types for the pre-implementation seed archive (aub-fon.2, FORMAT.md).
//!
//! Preserves quota readings captured by the external timer before the real system
//! existed. Handles both the modern YYYY-MM-DD.jsonl per-day layout and the older
//! capture.jsonl format.

use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
use crate::domain::time::{MeasurementBasis, UtcTimestamp};
use crate::error::Error;
use crate::legacy_meter::LegacyWindow;

/// A successful seed capture record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedArchiveSuccessRecord {
    pub source_file: String,
    pub source_line: u64,
    pub received_at: UtcTimestamp,
    pub generated_at: UtcTimestamp,
    pub generated_at_original: String,
    pub account: String,
    pub plan: Option<String>,
    pub tool: Option<String>,
    pub tool_version: Option<String>,
    pub windows: Vec<LegacyWindow>,
    pub raw_reading: String,
    pub measurement_basis: MeasurementBasis,
}

/// A failed seed capture record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedArchiveFailureRecord {
    pub source_file: String,
    pub source_line: u64,
    pub received_at: UtcTimestamp,
    pub account: String,
    pub tool: Option<String>,
    pub tool_version: Option<String>,
    pub failure_classification: String,
    pub exit_code: Option<i64>,
}

/// One parsed seed record: either a successful observation or a recorded failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedArchiveRecord {
    Success(SeedArchiveSuccessRecord),
    Failure(SeedArchiveFailureRecord),
}

impl SeedArchiveRecord {
    pub fn source_file(&self) -> &str {
        match self {
            Self::Success(r) => &r.source_file,
            Self::Failure(r) => &r.source_file,
        }
    }

    pub fn source_line(&self) -> u64 {
        match self {
            Self::Success(r) => r.source_line,
            Self::Failure(r) => r.source_line,
        }
    }

    pub fn received_at(&self) -> UtcTimestamp {
        match self {
            Self::Success(r) => r.received_at,
            Self::Failure(r) => r.received_at,
        }
    }

    pub fn account(&self) -> &str {
        match self {
            Self::Success(r) => &r.account,
            Self::Failure(r) => &r.account,
        }
    }
}

/// A record quarantined due to parsing errors or partial write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedArchiveQuarantine {
    pub source_file: String,
    pub source_line: u64,
    pub reason: String,
}

/// Parsed seed archive source contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSeedArchiveSource {
    pub content_digest: String,
    pub declared_measurement_basis: MeasurementBasis,
    pub records: Vec<SeedArchiveRecord>,
    pub records_read: u64,
    pub records_quarantined: u64,
    pub quarantined: Vec<SeedArchiveQuarantine>,
}

/// Reads and parses a seed archive from a file or directory.
pub fn read_source(path: &Path) -> Result<ParsedSeedArchiveSource, Error> {
    if !path.exists() {
        return Err(Error::IngestIncomplete(format!(
            "source path does not exist: {}",
            path.display()
        )));
    }

    if path.is_file() {
        read_single_file(path)
    } else if path.is_dir() {
        read_directory(path)
    } else {
        Err(Error::IngestIncomplete(format!(
            "source path is neither file nor directory: {}",
            path.display()
        )))
    }
}

fn read_single_file(path: &Path) -> Result<ParsedSeedArchiveSource, Error> {
    let bytes = fs::read(path).map_err(|e| {
        Error::IngestIncomplete(format!("cannot read file {}: {e}", path.display()))
    })?;
    let content_digest = sha256_hex(&bytes);
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "source.jsonl".to_string());

    let (records, records_read, records_quarantined, quarantined) =
        parse_file_content(&file_name, &bytes);

    Ok(ParsedSeedArchiveSource {
        content_digest,
        declared_measurement_basis: MeasurementBasis::ProviderObserved,
        records,
        records_read,
        records_quarantined,
        quarantined,
    })
}

fn read_directory(dir: &Path) -> Result<ParsedSeedArchiveSource, Error> {
    let mut entries = Vec::new();
    let read_dir = fs::read_dir(dir).map_err(|e| {
        Error::IngestIncomplete(format!("cannot read directory {}: {e}", dir.display()))
    })?;

    for entry in read_dir {
        let entry = entry
            .map_err(|e| Error::IngestIncomplete(format!("cannot read directory entry: {e}")))?;
        let entry_path = entry.path();
        if entry_path.is_file()
            && entry_path
                .extension()
                .map(|ext| ext == "jsonl")
                .unwrap_or(false)
        {
            entries.push(entry_path);
        }
    }

    if entries.is_empty() {
        return Err(Error::IngestIncomplete(format!(
            "no .jsonl files found in directory {}",
            dir.display()
        )));
    }

    // Sort order: capture.jsonl comes first (pre-landing archive), then dated files sorted by name
    entries.sort_by(|a, b| {
        let a_name = a.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let b_name = b.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if a_name == "capture.jsonl" && b_name != "capture.jsonl" {
            std::cmp::Ordering::Less
        } else if b_name == "capture.jsonl" && a_name != "capture.jsonl" {
            std::cmp::Ordering::Greater
        } else {
            a_name.cmp(b_name)
        }
    });

    let mut hasher = Sha256::new();
    let mut all_records = Vec::new();
    let mut total_read = 0u64;
    let mut total_quarantined = 0u64;
    let mut all_quarantined = Vec::new();

    for file_path in entries {
        let bytes = fs::read(&file_path).map_err(|e| {
            Error::IngestIncomplete(format!("cannot read file {}: {e}", file_path.display()))
        })?;
        hasher.update(&bytes);
        let file_name = file_path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown.jsonl".to_string());

        let (records, read_count, quarantined_count, quarantined) =
            parse_file_content(&file_name, &bytes);
        all_records.extend(records);
        total_read += read_count;
        total_quarantined += quarantined_count;
        all_quarantined.extend(quarantined);
    }

    let content_digest = format!("{:x}", hasher.finalize());

    Ok(ParsedSeedArchiveSource {
        content_digest,
        declared_measurement_basis: MeasurementBasis::ProviderObserved,
        records: all_records,
        records_read: total_read,
        records_quarantined: total_quarantined,
        quarantined: all_quarantined,
    })
}

fn parse_file_content(
    file_name: &str,
    bytes: &[u8],
) -> (Vec<SeedArchiveRecord>, u64, u64, Vec<SeedArchiveQuarantine>) {
    let content = String::from_utf8_lossy(bytes);
    let raw_lines: Vec<&str> = content.lines().collect();
    let non_empty_indices: Vec<usize> = raw_lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(idx, _)| idx)
        .collect();

    let mut records = Vec::new();
    let mut records_read = 0u64;
    let mut records_quarantined = 0u64;
    let mut quarantined = Vec::new();

    let last_non_empty_index = non_empty_indices.last().copied();

    for (line_idx, line) in raw_lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        records_read += 1;
        let source_line = (line_idx + 1) as u64;
        let is_last = Some(line_idx) == last_non_empty_index;

        match parse_record_line(file_name, source_line, line, is_last) {
            Ok(record) => records.push(record),
            Err(reason) => {
                records_quarantined += 1;
                quarantined.push(SeedArchiveQuarantine {
                    source_file: file_name.to_string(),
                    source_line,
                    reason,
                });
            }
        }
    }

    (records, records_read, records_quarantined, quarantined)
}

fn parse_record_line(
    file_name: &str,
    source_line: u64,
    line: &str,
    is_last: bool,
) -> Result<SeedArchiveRecord, String> {
    let value: serde_json::Value = serde_json::from_str(line).map_err(|_| {
        if is_last {
            "partial_trailing_line".to_string()
        } else {
            "invalid_json".to_string()
        }
    })?;

    let object = value
        .as_object()
        .ok_or_else(|| "json_not_an_object".to_string())?;

    let received_at_str = object
        .get("received_at")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing_received_at".to_string())?;

    let received_at = UtcTimestamp::parse_rfc3339(received_at_str)
        .ok_or_else(|| "invalid_received_at".to_string())?;

    let account = object
        .get("account")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "invalid_account".to_string())?
        .to_string();

    let tool = object
        .get("tool")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let tool_version = object
        .get("tool_version")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let plan_fallback = object
        .get("plan")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    // If reading is present, this is a success record
    if let Some(reading_val) = object.get("reading") {
        return parse_success_record(
            file_name,
            source_line,
            received_at,
            account,
            tool,
            tool_version,
            plan_fallback,
            reading_val,
        );
    }

    // Otherwise check for failure record
    let failure_opt = object
        .get("failure")
        .or_else(|| object.get("failure_classification"))
        .and_then(|v| v.as_str());
    let exit_code_opt = object.get("exit_code").and_then(|v| v.as_i64());

    if let Some(failure) = failure_opt {
        match failure {
            "spawn_failed" | "non_zero_exit" | "empty_output" => {
                Ok(SeedArchiveRecord::Failure(SeedArchiveFailureRecord {
                    source_file: file_name.to_string(),
                    source_line,
                    received_at,
                    account,
                    tool,
                    tool_version,
                    failure_classification: failure.to_string(),
                    exit_code: exit_code_opt,
                }))
            }
            _ => Err("unknown_failure_class".to_string()),
        }
    } else if let Some(code) = exit_code_opt
        && code != 0
    {
        // Legacy capture.jsonl format with exit_code != 0
        let classification = if code == 127 {
            "spawn_failed"
        } else {
            "non_zero_exit"
        };
        Ok(SeedArchiveRecord::Failure(SeedArchiveFailureRecord {
            source_file: file_name.to_string(),
            source_line,
            received_at,
            account,
            tool,
            tool_version,
            failure_classification: classification.to_string(),
            exit_code: Some(code),
        }))
    } else {
        Err("unrecognized_seed_record_shape".to_string())
    }
}

#[allow(clippy::too_many_arguments)]
fn parse_success_record(
    file_name: &str,
    source_line: u64,
    received_at: UtcTimestamp,
    account: String,
    tool: Option<String>,
    tool_version: Option<String>,
    plan_fallback: Option<String>,
    reading_val: &serde_json::Value,
) -> Result<SeedArchiveRecord, String> {
    let (reading_obj, raw_reading): (serde_json::Map<String, serde_json::Value>, String) =
        match reading_val {
            serde_json::Value::String(s) => {
                let parsed: serde_json::Value =
                    serde_json::from_str(s).map_err(|_| "malformed_reading_json".to_string())?;
                let map = parsed
                    .as_object()
                    .ok_or_else(|| "reading_not_an_object".to_string())?
                    .clone();
                (map, s.clone())
            }
            serde_json::Value::Object(map) => (map.clone(), reading_val.to_string()),
            serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::Array(_) => return Err("malformed_reading".to_string()),
        };

    let generated_at_str = reading_obj
        .get("generatedAt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing_generated_at".to_string())?;

    let generated_at = UtcTimestamp::parse_rfc3339(generated_at_str)
        .ok_or_else(|| "invalid_generated_at".to_string())?;

    let providers = reading_obj
        .get("providers")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "missing_providers".to_string())?;

    // Match provider by account name or provider field
    let provider_entry = providers
        .iter()
        .find(|p| {
            p.get("provider")
                .and_then(|v| v.as_str())
                .map(|p_name| {
                    p_name.eq_ignore_ascii_case(&account)
                        || (account.eq_ignore_ascii_case("primary")
                            && p_name.eq_ignore_ascii_case("claude"))
                        || (account.eq_ignore_ascii_case("claude")
                            && p_name.eq_ignore_ascii_case("claude"))
                })
                .unwrap_or(false)
        })
        .or_else(|| {
            if providers.len() == 1 {
                providers.first()
            } else {
                None
            }
        })
        .ok_or_else(|| "provider_not_found".to_string())?;

    let provider_obj = provider_entry
        .as_object()
        .ok_or_else(|| "provider_not_an_object".to_string())?;

    let plan = provider_obj
        .get("plan")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or(plan_fallback);

    let windows_arr = provider_obj
        .get("windows")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "missing_windows".to_string())?;

    let mut windows = Vec::new();
    for w_val in windows_arr {
        let w_obj = w_val
            .as_object()
            .ok_or_else(|| "window_not_an_object".to_string())?;

        let id = w_obj
            .get("id")
            .or_else(|| w_obj.get("name"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing_window_id".to_string())?;

        let percent_val = w_obj
            .get("percentUsed")
            .ok_or_else(|| "missing_percent_used".to_string())?;
        let quota_used =
            percentage_to_used(percent_val).ok_or_else(|| "invalid_percent_used".to_string())?;

        let default_window_seconds = match id {
            "five_hour" => 18_000,
            "seven_day" => 604_800,
            _ => 18_000,
        };
        let window_seconds = w_obj
            .get("windowSeconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(default_window_seconds);

        let nominal_duration_nanos = window_seconds.saturating_mul(1_000_000_000);
        let resets_at = match w_obj.get("resetsAt") {
            Some(val) => {
                parse_reset_timestamp(val).ok_or_else(|| "invalid_resets_at".to_string())?
            }
            None => UtcTimestamp::from_unix_nanos(
                generated_at
                    .unix_nanos()
                    .saturating_add(nominal_duration_nanos as i64),
            ),
        };

        let semantic_key = match id {
            "five_hour" => "five_hour",
            "seven_day" => "seven_day",
            other => Box::leak(other.to_string().into_boxed_str()),
        };

        windows.push(LegacyWindow {
            semantic_key,
            quota_used,
            resets_at,
            nominal_duration_nanos,
        });
    }

    Ok(SeedArchiveRecord::Success(SeedArchiveSuccessRecord {
        source_file: file_name.to_string(),
        source_line,
        received_at,
        generated_at,
        generated_at_original: generated_at_str.to_string(),
        account,
        plan,
        tool,
        tool_version,
        windows,
        raw_reading,
        measurement_basis: MeasurementBasis::ProviderObserved,
    }))
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
    let percentage = percentage.round();
    (0.0..=100.0).contains(&percentage).then_some(())?;
    let ppm = (percentage as i32).checked_mul(10_000)?;
    QuotaFractionPpm::new(ppm).map(QuotaUsed::new)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
