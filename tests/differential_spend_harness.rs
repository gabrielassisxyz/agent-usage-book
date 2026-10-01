//! Differential test harness against legacy spend tools (aub-lqe.16, PLAN.md 32, 33 Phase 5, 34.30).
//!
//! Validates:
//! 1. Automated comparison over a deterministic small corpus producing per-period comparison.
//! 2. Classification of every discrepancy into one of the five named categories:
//!    - newly discovered subagents
//!    - replay removal
//!    - parser correction
//!    - cache-write visibility
//!    - legacy bug
//!      or flagged as unclassified.
//! 3. Assertion that unclassified count must be zero before any legacy tool is retired.
//! 4. Per-category reporting stating exact share (tokens and ppm/percentage) of total difference.
//! 5. Content-identified, reproducible corpus digest comparing identical results across runs.
//! 6. Zero-tolerance invariant: a single unexplained unit remains unclassified and fails the harness.
//! 7. End-to-end execution against release aub binary and stub legacy executable across:
//!    - agreement
//!    - classified disagreement
//!    - unclassified disagreement
//!    - child nonzero exit
//!    - timeout
//!    - malformed output
//!      preserving exact argv, digests, stdout/stderr artifacts, exits, and unclassified count.
//! 8. Operational run over representative multi-week corpus recording content identity and classification.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use test_support::StateDir;

// --- Discrepancy Categories and Core Models ---------------------------------

/// The five expected categories of discrepancy between aub and legacy spend tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DiscrepancyCategory {
    NewlyDiscoveredSubagents,
    ReplayRemoval,
    ParserCorrection,
    CacheWriteVisibility,
    LegacyBug,
}

impl DiscrepancyCategory {
    pub const ALL: [DiscrepancyCategory; 5] = [
        DiscrepancyCategory::NewlyDiscoveredSubagents,
        DiscrepancyCategory::ReplayRemoval,
        DiscrepancyCategory::ParserCorrection,
        DiscrepancyCategory::CacheWriteVisibility,
        DiscrepancyCategory::LegacyBug,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            Self::NewlyDiscoveredSubagents => "newly discovered subagents",
            Self::ReplayRemoval => "replay removal",
            Self::ParserCorrection => "parser correction",
            Self::CacheWriteVisibility => "cache-write visibility",
            Self::LegacyBug => "legacy bug",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "newly discovered subagents" => Some(Self::NewlyDiscoveredSubagents),
            "replay removal" => Some(Self::ReplayRemoval),
            "parser correction" => Some(Self::ParserCorrection),
            "cache-write visibility" => Some(Self::CacheWriteVisibility),
            "legacy bug" => Some(Self::LegacyBug),
            _ => None,
        }
    }
}

/// Token usage counts for a period.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total: u64,
}

impl TokenUsage {
    pub fn new(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Self {
        let total = input + output + cache_read + cache_write;
        Self {
            input,
            output,
            cache_read,
            cache_write,
            total,
        }
    }
}

/// Signed difference between aub and legacy token usage (aub - legacy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenDelta {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub total: i64,
}

impl TokenDelta {
    pub fn between(aub: &TokenUsage, legacy: &TokenUsage) -> Self {
        Self {
            input: aub.input as i64 - legacy.input as i64,
            output: aub.output as i64 - legacy.output as i64,
            cache_read: aub.cache_read as i64 - legacy.cache_read as i64,
            cache_write: aub.cache_write as i64 - legacy.cache_write as i64,
            total: aub.total as i64 - legacy.total as i64,
        }
    }

    pub fn is_zero(&self) -> bool {
        self.input == 0
            && self.output == 0
            && self.cache_read == 0
            && self.cache_write == 0
            && self.total == 0
    }

    pub fn absolute_magnitude(&self) -> u64 {
        self.input.unsigned_abs()
            + self.output.unsigned_abs()
            + self.cache_read.unsigned_abs()
            + self.cache_write.unsigned_abs()
    }

    pub fn net_tokens(&self) -> i64 {
        self.total
    }
}

/// An explanation accounting for part or all of a discrepancy in a period.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedExplanation {
    pub category: DiscrepancyCategory,
    pub delta: TokenDelta,
    pub reason: String,
}

/// Comparison result for a single period (e.g. day or calendar interval).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodComparison {
    pub period: String,
    pub aub: TokenUsage,
    pub legacy: TokenUsage,
    pub total_delta: TokenDelta,
    pub classified_explanations: Vec<ClassifiedExplanation>,
    pub unclassified_delta: TokenDelta,
    pub is_agreement: bool,
}

/// Category accounting summary stating token delta and share of total difference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CategoryAccounting {
    pub category: DiscrepancyCategory,
    pub net_delta: i64,
    pub absolute_tokens: u64,
    pub share_ppm: u64,
    pub share_percentage: String,
}

/// Accounting summary for unclassified discrepancies.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UnclassifiedAccounting {
    pub count: u64,
    pub net_delta: i64,
    pub absolute_tokens: u64,
    pub share_ppm: u64,
    pub share_percentage: String,
}

/// Complete differential report covering all periods and discrepancy categories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DifferentialReport {
    pub corpus_content_digest: String,
    pub periods: Vec<PeriodComparison>,
    pub category_breakdown: BTreeMap<DiscrepancyCategory, CategoryAccounting>,
    pub unclassified: UnclassifiedAccounting,
    pub total_absolute_difference: u64,
    pub total_classified_difference: u64,
    pub retirement_ready: bool,
}

impl DifferentialReport {
    pub fn is_retirement_ready(&self) -> bool {
        self.unclassified.count == 0 && self.retirement_ready
    }

    pub fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("Corpus Digest: {}\n", self.corpus_content_digest));
        out.push_str(&format!(
            "Total Absolute Difference: {} tokens\n",
            self.total_absolute_difference
        ));
        out.push_str(&format!(
            "Unclassified Discrepancies: {} ({} tokens, {})\n",
            self.unclassified.count,
            self.unclassified.absolute_tokens,
            self.unclassified.share_percentage
        ));
        out.push_str(&format!(
            "Retirement Ready: {}\n\n",
            if self.is_retirement_ready() {
                "YES"
            } else {
                "NO (unclassified discrepancies remain)"
            }
        ));
        out.push_str("Category Breakdown:\n");
        for cat in DiscrepancyCategory::ALL {
            if let Some(acc) = self.category_breakdown.get(&cat) {
                out.push_str(&format!(
                    "  - {:<28} : net {:+8} tokens, {:8} abs tokens ({:>7}, {:6} ppm)\n",
                    cat.name(),
                    acc.net_delta,
                    acc.absolute_tokens,
                    acc.share_percentage,
                    acc.share_ppm
                ));
            }
        }
        out.push_str("\nPeriods:\n");
        for p in &self.periods {
            if p.is_agreement {
                out.push_str(&format!(
                    "  [AGREE] {} : total {} tokens\n",
                    p.period, p.aub.total
                ));
            } else if p.unclassified_delta.is_zero() {
                out.push_str(&format!(
                    "  [CLASSIFIED] {} : aub={} legacy={} (delta={:+})\n",
                    p.period, p.aub.total, p.legacy.total, p.total_delta.total
                ));
                for exp in &p.classified_explanations {
                    out.push_str(&format!(
                        "      category: {} (delta={:+}, reason: {})\n",
                        exp.category.name(),
                        exp.delta.total,
                        exp.reason
                    ));
                }
            } else {
                out.push_str(&format!(
                    "  [UNCLASSIFIED] {} : aub={} legacy={} (unexplained delta={:+})\n",
                    p.period, p.aub.total, p.legacy.total, p.unclassified_delta.total
                ));
            }
        }
        out
    }

    pub fn to_json_value(&self) -> Value {
        let periods_json: Vec<Value> = self
            .periods
            .iter()
            .map(|p| {
                let classified_json: Vec<Value> = p
                    .classified_explanations
                    .iter()
                    .map(|exp| {
                        json!({
                            "category": exp.category.name(),
                            "delta": {
                                "input": exp.delta.input,
                                "output": exp.delta.output,
                                "cache_read": exp.delta.cache_read,
                                "cache_write": exp.delta.cache_write,
                                "total": exp.delta.total,
                            },
                            "reason": exp.reason,
                        })
                    })
                    .collect();
                json!({
                    "period": p.period,
                    "is_agreement": p.is_agreement,
                    "aub": {
                        "input": p.aub.input,
                        "output": p.aub.output,
                        "cache_read": p.aub.cache_read,
                        "cache_write": p.aub.cache_write,
                        "total": p.aub.total,
                    },
                    "legacy": {
                        "input": p.legacy.input,
                        "output": p.legacy.output,
                        "cache_read": p.legacy.cache_read,
                        "cache_write": p.legacy.cache_write,
                        "total": p.legacy.total,
                    },
                    "total_delta": {
                        "input": p.total_delta.input,
                        "output": p.total_delta.output,
                        "cache_read": p.total_delta.cache_read,
                        "cache_write": p.total_delta.cache_write,
                        "total": p.total_delta.total,
                    },
                    "classified_explanations": classified_json,
                    "unclassified_delta": {
                        "input": p.unclassified_delta.input,
                        "output": p.unclassified_delta.output,
                        "cache_read": p.unclassified_delta.cache_read,
                        "cache_write": p.unclassified_delta.cache_write,
                        "total": p.unclassified_delta.total,
                    },
                })
            })
            .collect();

        let mut breakdown_json = json!({});
        for (cat, acc) in &self.category_breakdown {
            breakdown_json[cat.name()] = json!({
                "net_delta": acc.net_delta,
                "absolute_tokens": acc.absolute_tokens,
                "share_ppm": acc.share_ppm,
                "share_percentage": acc.share_percentage,
            });
        }

        json!({
            "corpus_content_digest": self.corpus_content_digest,
            "total_absolute_difference": self.total_absolute_difference,
            "total_classified_difference": self.total_classified_difference,
            "unclassified": {
                "count": self.unclassified.count,
                "net_delta": self.unclassified.net_delta,
                "absolute_tokens": self.unclassified.absolute_tokens,
                "share_ppm": self.unclassified.share_ppm,
                "share_percentage": self.unclassified.share_percentage,
            },
            "retirement_ready": self.retirement_ready,
            "category_breakdown": breakdown_json,
            "periods": periods_json,
        })
    }
}

/// Execution artifacts for one process in the differential harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessArtifacts {
    pub argv: Vec<String>,
    pub binary_digest: String,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// The run log capturing all execution details across child processes and verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DifferentialRunLog {
    pub scenario: String,
    pub corpus_digest: String,
    pub aub: ProcessArtifacts,
    pub legacy: ProcessArtifacts,
    pub per_category_deltas: BTreeMap<String, i64>,
    pub unclassified_count: u64,
    pub outcome: String,
}

impl DifferentialRunLog {
    pub fn to_json_value(&self) -> Value {
        json!({
            "scenario": self.scenario,
            "corpus_digest": self.corpus_digest,
            "aub": {
                "argv": self.aub.argv,
                "binary_digest": self.aub.binary_digest,
                "exit_code": self.aub.exit_code,
                "signal": self.aub.signal,
                "stdout_len": self.aub.stdout.len(),
                "stderr_len": self.aub.stderr.len(),
            },
            "legacy": {
                "argv": self.legacy.argv,
                "binary_digest": self.legacy.binary_digest,
                "exit_code": self.legacy.exit_code,
                "signal": self.legacy.signal,
                "stdout_len": self.legacy.stdout.len(),
                "stderr_len": self.legacy.stderr.len(),
            },
            "per_category_deltas": self.per_category_deltas,
            "unclassified_count": self.unclassified_count,
            "outcome": self.outcome,
        })
    }
}

// --- Corpus Digest and Content Identification ------------------------------

/// Computes the deterministic cryptographic content digest of all files in a corpus.
pub fn compute_corpus_content_digest(corpus_root: &Path) -> Result<String, io::Error> {
    let mut files = Vec::new();
    collect_files_recursive(corpus_root, &mut files)?;
    files.sort();

    let mut overall = Sha256::new();
    for file in &files {
        let rel = file
            .strip_prefix(corpus_root)
            .unwrap_or(file)
            .to_string_lossy();
        let content = fs::read(file)?;
        let file_hash = Sha256::digest(&content);

        overall.update(rel.as_bytes());
        overall.update(file_hash);
    }
    Ok(format!("{:x}", overall.finalize()))
}

fn collect_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), io::Error> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files_recursive(&path, out)?;
        } else if path.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

/// Computes the sha256 digest of a single binary file.
pub fn compute_file_digest(file: &Path) -> Result<String, io::Error> {
    let content = fs::read(file)?;
    Ok(format!("{:x}", Sha256::digest(&content)))
}

// --- Parsers for Tool Outputs ----------------------------------------------

/// Parses `aub spend --format json` output into per-period `TokenUsage` map.
pub fn parse_aub_json(raw: &str) -> Result<BTreeMap<String, TokenUsage>, String> {
    let root: Value = serde_json::from_str(raw).map_err(|e| format!("invalid aub json: {e}"))?;
    let groups = root
        .get("groups")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing groups array in aub spend output".to_string())?;

    let mut periods = BTreeMap::new();
    for g in groups {
        let raw_key = g
            .get("key")
            .or_else(|| g.get("name"))
            .and_then(Value::as_str)
            .ok_or_else(|| "missing group key or name".to_string())?;
        let period = if let Some(stripped) = raw_key.strip_prefix("day=") {
            stripped
                .split_whitespace()
                .next()
                .unwrap_or(stripped)
                .to_string()
        } else {
            raw_key
                .split_whitespace()
                .next()
                .unwrap_or(raw_key)
                .to_string()
        };

        let tokens = g
            .get("tokens")
            .ok_or_else(|| "missing tokens object".to_string())?;

        let parse_val = |field: &str| -> u64 {
            let v = tokens.get(field).and_then(|obj| obj.get("value"));
            if let Some(num) = v.and_then(Value::as_u64) {
                num
            } else if let Some(s) = v.and_then(Value::as_str) {
                s.parse().unwrap_or(0)
            } else {
                0
            }
        };

        let input = parse_val("input");
        let output = parse_val("output");
        let cache_read = parse_val("cache_read");
        let cache_write = parse_val("cache_write");

        let usage = TokenUsage::new(input, output, cache_read, cache_write);
        let entry = periods.entry(period).or_insert(TokenUsage::default());
        *entry = TokenUsage::new(
            entry.input + usage.input,
            entry.output + usage.output,
            entry.cache_read + usage.cache_read,
            entry.cache_write + usage.cache_write,
        );
    }
    Ok(periods)
}

/// Parses legacy spend JSON output into per-period `TokenUsage` map.
pub fn parse_legacy_json(raw: &str) -> Result<BTreeMap<String, TokenUsage>, String> {
    let root: Value = serde_json::from_str(raw).map_err(|e| format!("invalid legacy json: {e}"))?;
    let periods_array = root
        .get("periods")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing periods array in legacy spend output".to_string())?;

    let mut periods = BTreeMap::new();
    for p in periods_array {
        let period = p
            .get("period")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing period field".to_string())?
            .to_string();
        let input = p.get("input").and_then(Value::as_u64).unwrap_or(0);
        let output = p.get("output").and_then(Value::as_u64).unwrap_or(0);
        let cache_read = p.get("cache_read").and_then(Value::as_u64).unwrap_or(0);
        let cache_write = p.get("cache_write").and_then(Value::as_u64).unwrap_or(0);
        let total = p
            .get("total")
            .and_then(Value::as_u64)
            .unwrap_or(input + output + cache_read + cache_write);

        periods.insert(
            period,
            TokenUsage {
                input,
                output,
                cache_read,
                cache_write,
                total,
            },
        );
    }
    Ok(periods)
}

// --- Comparison and Classification Logic -----------------------------------

/// Discrepancy evidence rule providing expected classified explanations for known corpus patterns.
#[derive(Debug, Clone)]
pub struct DiscrepancyRule {
    pub period: String,
    pub category: DiscrepancyCategory,
    pub delta: TokenDelta,
    pub reason: String,
}

/// Classifies discrepancies for a single period given aub usage, legacy usage and registered evidence.
pub fn classify_period_discrepancy(
    period: &str,
    aub: &TokenUsage,
    legacy: &TokenUsage,
    rules: &[DiscrepancyRule],
) -> PeriodComparison {
    let total_delta = TokenDelta::between(aub, legacy);
    if total_delta.is_zero() {
        return PeriodComparison {
            period: period.to_string(),
            aub: *aub,
            legacy: *legacy,
            total_delta,
            classified_explanations: Vec::new(),
            unclassified_delta: TokenDelta::default(),
            is_agreement: true,
        };
    }

    let mut remaining = total_delta;
    let mut classified = Vec::new();

    // 1. Automatic structural classification: CacheWriteVisibility.
    // If legacy reported 0 cache_write while aub measured positive cache writes,
    // that exact difference is attributable to cache-write visibility.
    if legacy.cache_write == 0 && aub.cache_write > 0 {
        let cw_diff = aub.cache_write as i64;
        let cw_delta = TokenDelta {
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: cw_diff,
            total: cw_diff,
        };
        classified.push(ClassifiedExplanation {
            category: DiscrepancyCategory::CacheWriteVisibility,
            delta: cw_delta,
            reason: format!(
                "legacy spend tool omitted cache_write billing; aub measured {cw_diff} tokens"
            ),
        });
        remaining.cache_write -= cw_diff;
        remaining.total -= cw_diff;
    }

    // 2. Apply period-specific evidence rules for the other named categories.
    for rule in rules {
        if rule.period != period {
            continue;
        }
        classified.push(ClassifiedExplanation {
            category: rule.category,
            delta: rule.delta,
            reason: rule.reason.clone(),
        });
        remaining.input -= rule.delta.input;
        remaining.output -= rule.delta.output;
        remaining.cache_read -= rule.delta.cache_read;
        remaining.cache_write -= rule.delta.cache_write;
        remaining.total -= rule.delta.total;
    }

    // Zero-tolerance rule: any unexplained non-zero token delta remains unclassified.
    // No tolerance threshold may turn an unexplained residual into agreement.
    let is_agreement = total_delta.is_zero();

    PeriodComparison {
        period: period.to_string(),
        aub: *aub,
        legacy: *legacy,
        total_delta,
        classified_explanations: classified,
        unclassified_delta: remaining,
        is_agreement,
    }
}

/// Builds the comprehensive differential report from period comparisons and corpus digest.
pub fn generate_differential_report(
    corpus_digest: String,
    periods: Vec<PeriodComparison>,
) -> DifferentialReport {
    let mut total_absolute_difference = 0u64;
    let mut category_totals: BTreeMap<DiscrepancyCategory, (i64, u64)> = BTreeMap::new();
    let mut unclassified_count = 0u64;
    let mut unclassified_net = 0i64;
    let mut unclassified_abs = 0u64;

    for p in &periods {
        if p.is_agreement {
            continue;
        }
        total_absolute_difference += p.total_delta.absolute_magnitude();

        for exp in &p.classified_explanations {
            let entry = category_totals.entry(exp.category).or_insert((0i64, 0u64));
            entry.0 += exp.delta.net_tokens();
            entry.1 += exp.delta.absolute_magnitude();
        }

        if !p.unclassified_delta.is_zero() {
            unclassified_count += 1;
            unclassified_net += p.unclassified_delta.net_tokens();
            unclassified_abs += p.unclassified_delta.absolute_magnitude();
        }
    }

    let mut category_breakdown = BTreeMap::new();
    let mut total_classified_abs = 0u64;

    for cat in DiscrepancyCategory::ALL {
        let (net, abs) = category_totals.get(&cat).copied().unwrap_or((0, 0));
        total_classified_abs += abs;

        let (share_ppm, share_pct) = if total_absolute_difference > 0 {
            let ppm = (abs as u128 * 1_000_000 / total_absolute_difference as u128) as u64;
            let pct = format!("{:.2}%", (ppm as f64) / 10_000.0);
            (ppm, pct)
        } else {
            (0, "0.00%".to_string())
        };

        category_breakdown.insert(
            cat,
            CategoryAccounting {
                category: cat,
                net_delta: net,
                absolute_tokens: abs,
                share_ppm,
                share_percentage: share_pct,
            },
        );
    }

    let unclassified_accounting = if total_absolute_difference > 0 {
        let ppm = (unclassified_abs as u128 * 1_000_000 / total_absolute_difference as u128) as u64;
        let pct = format!("{:.2}%", (ppm as f64) / 10_000.0);
        UnclassifiedAccounting {
            count: unclassified_count,
            net_delta: unclassified_net,
            absolute_tokens: unclassified_abs,
            share_ppm: ppm,
            share_percentage: pct,
        }
    } else {
        UnclassifiedAccounting::default()
    };

    let retirement_ready = unclassified_count == 0;

    DifferentialReport {
        corpus_content_digest: corpus_digest,
        periods,
        category_breakdown,
        unclassified: unclassified_accounting,
        total_absolute_difference,
        total_classified_difference: total_classified_abs,
        retirement_ready,
    }
}

// --- End-to-End Execution Runner -------------------------------------------

/// Runs both aub spend and legacy tool, recording the exact run log and differential report.
///
/// The long argument list and the large `Err` variant are both deliberate: each process
/// contributes its binary, its argv and its environment separately so the run log can record
/// exactly what was invoked, and a failed run returns that same log because the log is the
/// evidence the failure produces. Bundling either one hides what this harness exists to show.
#[allow(clippy::too_many_arguments, clippy::result_large_err)]
pub fn run_differential_executables(
    aub_bin: &Path,
    aub_args: &[String],
    aub_envs: &[(&str, &str)],
    legacy_bin: &Path,
    legacy_args: &[String],
    legacy_envs: &[(&str, &str)],
    corpus_dir: &Path,
    rules: &[DiscrepancyRule],
    scenario: &str,
    timeout: Duration,
) -> Result<(DifferentialReport, DifferentialRunLog), DifferentialRunLog> {
    let corpus_digest = compute_corpus_content_digest(corpus_dir).unwrap_or_default();
    let aub_binary_digest = compute_file_digest(aub_bin).unwrap_or_default();
    let legacy_binary_digest = compute_file_digest(legacy_bin).unwrap_or_default();

    // 1. Invoke aub spend
    let mut aub_cmd = Command::new(aub_bin);
    aub_cmd.args(aub_args);
    for (k, v) in aub_envs {
        aub_cmd.env(k, v);
    }
    let aub_output = aub_cmd.output().map_err(|e| DifferentialRunLog {
        scenario: scenario.to_string(),
        corpus_digest: corpus_digest.clone(),
        aub: ProcessArtifacts {
            argv: std::iter::once(aub_bin.to_string_lossy().to_string())
                .chain(aub_args.iter().cloned())
                .collect(),
            binary_digest: aub_binary_digest.clone(),
            exit_code: None,
            signal: None,
            stdout: String::new(),
            stderr: format!("failed to spawn aub: {e}"),
        },
        legacy: ProcessArtifacts {
            argv: std::iter::once(legacy_bin.to_string_lossy().to_string())
                .chain(legacy_args.iter().cloned())
                .collect(),
            binary_digest: legacy_binary_digest.clone(),
            exit_code: None,
            signal: None,
            stdout: String::new(),
            stderr: String::new(),
        },
        per_category_deltas: BTreeMap::new(),
        unclassified_count: 1,
        outcome: format!("failed to spawn aub: {e}"),
    })?;

    let aub_stdout = String::from_utf8_lossy(&aub_output.stdout).to_string();
    let aub_stderr = String::from_utf8_lossy(&aub_output.stderr).to_string();
    let aub_artifacts = ProcessArtifacts {
        argv: std::iter::once(aub_bin.to_string_lossy().to_string())
            .chain(aub_args.iter().cloned())
            .collect(),
        binary_digest: aub_binary_digest.clone(),
        exit_code: aub_output.status.code(),
        signal: None,
        stdout: aub_stdout.clone(),
        stderr: aub_stderr,
    };

    // 2. Invoke legacy executable with timeout control
    let mut legacy_cmd = Command::new(legacy_bin);
    legacy_cmd.args(legacy_args);
    for (k, v) in legacy_envs {
        legacy_cmd.env(k, v);
    }
    legacy_cmd.stdout(Stdio::piped());
    legacy_cmd.stderr(Stdio::piped());

    let start = Instant::now();
    let mut child = legacy_cmd.spawn().map_err(|e| DifferentialRunLog {
        scenario: scenario.to_string(),
        corpus_digest: corpus_digest.clone(),
        aub: aub_artifacts.clone(),
        legacy: ProcessArtifacts {
            argv: std::iter::once(legacy_bin.to_string_lossy().to_string())
                .chain(legacy_args.iter().cloned())
                .collect(),
            binary_digest: legacy_binary_digest.clone(),
            exit_code: None,
            signal: None,
            stdout: String::new(),
            stderr: format!("failed to spawn legacy: {e}"),
        },
        per_category_deltas: BTreeMap::new(),
        unclassified_count: 1,
        outcome: format!("failed to spawn legacy tool: {e}"),
    })?;

    // Poll child for exit or timeout
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    timed_out = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                return Err(DifferentialRunLog {
                    scenario: scenario.to_string(),
                    corpus_digest: corpus_digest.clone(),
                    aub: aub_artifacts,
                    legacy: ProcessArtifacts {
                        argv: std::iter::once(legacy_bin.to_string_lossy().to_string())
                            .chain(legacy_args.iter().cloned())
                            .collect(),
                        binary_digest: legacy_binary_digest,
                        exit_code: None,
                        signal: None,
                        stdout: String::new(),
                        stderr: format!("error polling child: {e}"),
                    },
                    per_category_deltas: BTreeMap::new(),
                    unclassified_count: 1,
                    outcome: format!("legacy execution error: {e}"),
                });
            }
        }
    }

    if timed_out {
        return Err(DifferentialRunLog {
            scenario: scenario.to_string(),
            corpus_digest: corpus_digest.clone(),
            aub: aub_artifacts,
            legacy: ProcessArtifacts {
                argv: std::iter::once(legacy_bin.to_string_lossy().to_string())
                    .chain(legacy_args.iter().cloned())
                    .collect(),
                binary_digest: legacy_binary_digest,
                exit_code: None,
                signal: Some(9),
                stdout: String::new(),
                stderr: "process timed out".to_string(),
            },
            per_category_deltas: BTreeMap::new(),
            unclassified_count: 1,
            outcome: "legacy execution timed out".to_string(),
        });
    }

    let mut legacy_stdout = String::new();
    let mut legacy_stderr = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut legacy_stdout);
    }
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut legacy_stderr);
    }

    let legacy_status = child.wait().ok();
    let legacy_exit = legacy_status.and_then(|s| s.code());

    let legacy_artifacts = ProcessArtifacts {
        argv: std::iter::once(legacy_bin.to_string_lossy().to_string())
            .chain(legacy_args.iter().cloned())
            .collect(),
        binary_digest: legacy_binary_digest,
        exit_code: legacy_exit,
        signal: None,
        stdout: legacy_stdout.clone(),
        stderr: legacy_stderr.clone(),
    };

    if legacy_exit != Some(0) {
        return Err(DifferentialRunLog {
            scenario: scenario.to_string(),
            corpus_digest,
            aub: aub_artifacts,
            legacy: legacy_artifacts,
            per_category_deltas: BTreeMap::new(),
            unclassified_count: 1,
            outcome: format!("legacy tool failed with exit code {:?}", legacy_exit),
        });
    }

    // 3. Parse outputs and run differential comparison
    let aub_periods = match parse_aub_json(&aub_stdout) {
        Ok(m) => m,
        Err(e) => {
            return Err(DifferentialRunLog {
                scenario: scenario.to_string(),
                corpus_digest,
                aub: aub_artifacts,
                legacy: legacy_artifacts,
                per_category_deltas: BTreeMap::new(),
                unclassified_count: 1,
                outcome: format!("malformed aub output: {e}"),
            });
        }
    };

    let legacy_periods = match parse_legacy_json(&legacy_stdout) {
        Ok(m) => m,
        Err(e) => {
            return Err(DifferentialRunLog {
                scenario: scenario.to_string(),
                corpus_digest,
                aub: aub_artifacts,
                legacy: legacy_artifacts,
                per_category_deltas: BTreeMap::new(),
                unclassified_count: 1,
                outcome: format!("malformed legacy output: {e}"),
            });
        }
    };

    let mut all_period_keys: BTreeSet<String> = BTreeSet::new();
    all_period_keys.extend(aub_periods.keys().cloned());
    all_period_keys.extend(legacy_periods.keys().cloned());

    let mut period_comparisons = Vec::new();
    for p in all_period_keys {
        let aub_usage = aub_periods.get(&p).copied().unwrap_or_default();
        let legacy_usage = legacy_periods.get(&p).copied().unwrap_or_default();
        let cmp = classify_period_discrepancy(&p, &aub_usage, &legacy_usage, rules);
        period_comparisons.push(cmp);
    }

    let report = generate_differential_report(corpus_digest.clone(), period_comparisons);

    let mut per_category_deltas = BTreeMap::new();
    for (cat, acc) in &report.category_breakdown {
        per_category_deltas.insert(cat.name().to_string(), acc.net_delta);
    }

    let run_log = DifferentialRunLog {
        scenario: scenario.to_string(),
        corpus_digest,
        aub: aub_artifacts,
        legacy: legacy_artifacts,
        per_category_deltas,
        unclassified_count: report.unclassified.count,
        outcome: if report.is_retirement_ready() {
            "passed".to_string()
        } else {
            format!(
                "failed: {} unclassified discrepancies remain",
                report.unclassified.count
            )
        },
    };

    if report.is_retirement_ready() {
        Ok((report, run_log))
    } else {
        Err(run_log)
    }
}

// --- Test Suites -----------------------------------------------------------

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn legacy_stub_bin() -> PathBuf {
    repo_root().join("tests/fixtures/differential/legacy_spend_stub.sh")
}

fn small_corpus_path() -> PathBuf {
    repo_root().join("tests/fixtures/differential/small_corpus")
}

fn multi_week_corpus_path() -> PathBuf {
    repo_root().join("tests/fixtures/differential/multi_week_corpus")
}

fn create_test_config(state: &StateDir, corpus: &Path) -> PathBuf {
    let cfg_path = state.path().join("aub.toml");
    let content = format!(
        r#"
[[transcripts]]
name = "claude-code"
root = "{}"
pattern = "**/*.jsonl"
format = "claude-code"
"#,
        corpus.join("claude-code").display()
    );
    fs::write(&cfg_path, content).expect("write aub.toml");
    cfg_path
}

// ---------------------------------------------------------------------------
// Acceptance Criterion 1 & Integration Test 1:
// The automated harness runs both tool sets over a deterministic small corpus
// and produces a per-period comparison.
// ---------------------------------------------------------------------------
#[test]
fn integration_harness_runs_over_deterministic_small_corpus_and_produces_per_period_comparison() {
    let aub_bin = PathBuf::from(env!("CARGO_BIN_EXE_aub"));
    let legacy_bin = legacy_stub_bin();
    let corpus = small_corpus_path();
    let state = StateDir::new();
    let config = create_test_config(&state, &corpus);

    let aub_args = vec![
        "spend".to_string(),
        "--since".to_string(),
        "2026-08-25".to_string(),
        "--days".to_string(),
        "2".to_string(),
        "--group-by".to_string(),
        "day".to_string(),
        "--refresh".to_string(),
        "force".to_string(),
        "--format".to_string(),
        "json".to_string(),
    ];
    let aub_envs = [
        ("AUB_CONFIG_FILE", config.to_str().unwrap()),
        ("AUB_STATE_DIR", state.path().to_str().unwrap()),
    ];

    let legacy_args = vec![
        "--since".to_string(),
        "2026-08-25".to_string(),
        "--days".to_string(),
        "2".to_string(),
        "--scenario".to_string(),
        "agreement".to_string(),
    ];

    let (report, run_log) = run_differential_executables(
        &aub_bin,
        &aub_args,
        &aub_envs,
        &legacy_bin,
        &legacy_args,
        &[],
        &corpus,
        &[],
        "agreement",
        Duration::from_secs(5),
    )
    .expect("harness run on small corpus must pass");

    assert_eq!(
        report.periods.len(),
        2,
        "must produce comparison for both periods"
    );
    assert_eq!(report.periods[0].period, "2026-08-25");
    assert_eq!(report.periods[1].period, "2026-08-26");

    assert!(
        report.periods[0].is_agreement,
        "period 1 must agree in agreement mode"
    );
    assert!(
        report.periods[1].is_agreement,
        "period 2 must agree in agreement mode"
    );
    assert_eq!(run_log.outcome, "passed");
}

// ---------------------------------------------------------------------------
// Acceptance Criterion 2 & Integration Test 2:
// Every discrepancy is classified into one of the five named categories, or is
// flagged as unclassified.
// ---------------------------------------------------------------------------
#[test]
fn integration_every_discrepancy_classified_or_flagged_unclassified() {
    let period = "2026-08-25";
    let aub = TokenUsage::new(1200, 600, 2000, 3000);
    let legacy = TokenUsage::new(1000, 500, 2000, 0);

    // Rule for subagent delta (-200 input, -100 output)
    let rules = vec![DiscrepancyRule {
        period: period.to_string(),
        category: DiscrepancyCategory::NewlyDiscoveredSubagents,
        delta: TokenDelta {
            input: 200,
            output: 100,
            cache_read: 0,
            cache_write: 0,
            total: 300,
        },
        reason: "legacy missed nested subagent transcript".to_string(),
    }];

    let cmp = classify_period_discrepancy(period, &aub, &legacy, &rules);

    assert!(!cmp.is_agreement);
    assert_eq!(cmp.classified_explanations.len(), 2);

    let categories: BTreeSet<DiscrepancyCategory> = cmp
        .classified_explanations
        .iter()
        .map(|e| e.category)
        .collect();
    assert!(categories.contains(&DiscrepancyCategory::CacheWriteVisibility));
    assert!(categories.contains(&DiscrepancyCategory::NewlyDiscoveredSubagents));
    assert!(
        cmp.unclassified_delta.is_zero(),
        "all deltas must be fully accounted for"
    );
}

// ---------------------------------------------------------------------------
// Acceptance Criterion 3 & Integration Test 3:
// The count of unclassified discrepancies is zero before any legacy tool is retired.
// ---------------------------------------------------------------------------
#[test]
fn integration_unclassified_count_must_be_zero_before_retirement() {
    let report_zero = DifferentialReport {
        corpus_content_digest: "abcd".to_string(),
        periods: vec![],
        category_breakdown: BTreeMap::new(),
        unclassified: UnclassifiedAccounting {
            count: 0,
            net_delta: 0,
            absolute_tokens: 0,
            share_ppm: 0,
            share_percentage: "0.00%".to_string(),
        },
        total_absolute_difference: 100,
        total_classified_difference: 100,
        retirement_ready: true,
    };
    assert!(
        report_zero.is_retirement_ready(),
        "retirement is permitted when unclassified count is zero"
    );

    let report_unclassified = DifferentialReport {
        corpus_content_digest: "abcd".to_string(),
        periods: vec![],
        category_breakdown: BTreeMap::new(),
        unclassified: UnclassifiedAccounting {
            count: 1,
            net_delta: 1,
            absolute_tokens: 1,
            share_ppm: 10000,
            share_percentage: "1.00%".to_string(),
        },
        total_absolute_difference: 100,
        total_classified_difference: 99,
        retirement_ready: false,
    };
    assert!(
        !report_unclassified.is_retirement_ready(),
        "retirement must be blocked when unclassified count > 0"
    );
}

// ---------------------------------------------------------------------------
// Acceptance Criterion 4 & Unit Test 4:
// The report states, per category, how much of the total difference it accounts for.
// ---------------------------------------------------------------------------
#[test]
fn unit_report_states_per_category_share_of_total_difference() {
    let period1 = PeriodComparison {
        period: "2026-08-25".to_string(),
        aub: TokenUsage::new(1000, 500, 0, 3000),
        legacy: TokenUsage::new(1000, 500, 0, 0),
        total_delta: TokenDelta {
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 3000,
            total: 3000,
        },
        classified_explanations: vec![ClassifiedExplanation {
            category: DiscrepancyCategory::CacheWriteVisibility,
            delta: TokenDelta {
                input: 0,
                output: 0,
                cache_read: 0,
                cache_write: 3000,
                total: 3000,
            },
            reason: "cache write visibility".to_string(),
        }],
        unclassified_delta: TokenDelta::default(),
        is_agreement: false,
    };

    let period2 = PeriodComparison {
        period: "2026-08-26".to_string(),
        aub: TokenUsage::new(1000, 500, 0, 0),
        legacy: TokenUsage::new(1000, 1500, 0, 0),
        total_delta: TokenDelta {
            input: 0,
            output: -1000,
            cache_read: 0,
            cache_write: 0,
            total: -1000,
        },
        classified_explanations: vec![ClassifiedExplanation {
            category: DiscrepancyCategory::ReplayRemoval,
            delta: TokenDelta {
                input: 0,
                output: -1000,
                cache_read: 0,
                cache_write: 0,
                total: -1000,
            },
            reason: "replay removal".to_string(),
        }],
        unclassified_delta: TokenDelta::default(),
        is_agreement: false,
    };

    let report = generate_differential_report("test-digest".to_string(), vec![period1, period2]);

    assert_eq!(report.total_absolute_difference, 4000);

    let cw = report
        .category_breakdown
        .get(&DiscrepancyCategory::CacheWriteVisibility)
        .expect("cache-write entry must exist");
    assert_eq!(cw.absolute_tokens, 3000);
    assert_eq!(cw.share_ppm, 750000, "3000/4000 = 750,000 ppm");
    assert_eq!(cw.share_percentage, "75.00%");

    let rep = report
        .category_breakdown
        .get(&DiscrepancyCategory::ReplayRemoval)
        .expect("replay entry must exist");
    assert_eq!(rep.absolute_tokens, 1000);
    assert_eq!(rep.share_ppm, 250000, "1000/4000 = 250,000 ppm");
    assert_eq!(rep.share_percentage, "25.00%");

    assert_eq!(
        cw.share_ppm + rep.share_ppm,
        1_000_000,
        "total share must sum to 100%"
    );
}

// ---------------------------------------------------------------------------
// Acceptance Criterion 5 & Unit Test 5:
// The harness is re-runnable and its corpus is identified by content so results
// are comparable across runs.
// ---------------------------------------------------------------------------
#[test]
fn unit_corpus_identified_by_content_and_comparable_across_runs() {
    let corpus = small_corpus_path();
    let digest1 = compute_corpus_content_digest(&corpus).expect("digest run 1");
    let digest2 = compute_corpus_content_digest(&corpus).expect("digest run 2");

    assert_eq!(
        digest1, digest2,
        "corpus content digest must be deterministic across runs"
    );
    assert!(!digest1.is_empty());
    assert_eq!(digest1.len(), 64, "must be a 64-char sha256 hex digest");

    // Proving mutation: if any content changes, digest changes
    let state = StateDir::new();
    let mutated_corpus = state.path().join("mutated_corpus");
    fs::create_dir_all(&mutated_corpus).unwrap();
    fs::write(mutated_corpus.join("session.jsonl"), "content A").unwrap();
    let dig_a = compute_corpus_content_digest(&mutated_corpus).unwrap();

    fs::write(mutated_corpus.join("session.jsonl"), "content B").unwrap();
    let dig_b = compute_corpus_content_digest(&mutated_corpus).unwrap();

    assert_ne!(
        dig_a, dig_b,
        "different content must produce different digests"
    );
}

// ---------------------------------------------------------------------------
// Acceptance Criterion 6 & Integration Test 6:
// A one-unit unexplained discrepancy remains unclassified and makes the harness
// fail; no tolerance threshold can turn it into agreement.
// ---------------------------------------------------------------------------
#[test]
fn integration_one_unit_unexplained_discrepancy_fails_harness() {
    let period = "2026-08-25";
    let aub = TokenUsage::new(1000, 500, 0, 0);
    // Legacy differs by exactly 1 unit (499 output tokens instead of 500)
    let legacy = TokenUsage::new(1000, 499, 0, 0);

    let cmp = classify_period_discrepancy(period, &aub, &legacy, &[]);

    assert!(
        !cmp.is_agreement,
        "1-unit discrepancy cannot be treated as agreement"
    );
    assert_eq!(
        cmp.unclassified_delta.output, 1,
        "1-unit difference must remain unclassified"
    );
    assert_eq!(cmp.unclassified_delta.total, 1);

    let report = generate_differential_report("digest".to_string(), vec![cmp]);
    assert_eq!(report.unclassified.count, 1);
    assert_eq!(report.unclassified.absolute_tokens, 1);
    assert!(
        !report.is_retirement_ready(),
        "1-unit unexplained discrepancy must fail harness"
    );
}

// ---------------------------------------------------------------------------
// Multi-week differential with corpus-derived expectations (aub-wsjq).
//
// The default-suite test below runs the real `aub spend --format json` binary
// over tests/fixtures/differential/multi_week_corpus/ and derives the legacy
// side from the same corpus with an independent parse written here. Neither
// side is a literal: a change in the corpus, in aub's spend, or in the
// harness classifier fails the test instead of passing against stale numbers.
//
// Legacy model (what the legacy tools count, per record class):
// - Records under a `subagents/` directory are invisible to legacy (legacy
//   never discovered nested transcripts) and count fully toward
//   NewlyDiscoveredSubagents.
// - Legacy sums every line including replayed ones while aub keeps the last
//   line per message id, so ReplayRemoval equals the superseded lines'
//   tokens.
// - Legacy never reports cache_write, so CacheWriteVisibility equals aub's
//   measured cache writes. This is also the classifier's automatic arm, so no
//   explicit rule below carries a cache_write component.
// - Legacy's codex parser counts reasoning_output_tokens as output while aub
//   reports model output only, so ParserCorrection equals the negated
//   reasoning tokens.
// - Legacy's cumulative importer swaps input and output of a record ingested
//   after a cache-write record in the same file (inversion bug), so LegacyBug
//   equals the swapped-minus-original delta of that record.
// The derivation reads the JSONL files directly and never calls the harness
// classifier or aub's transcript parser. Replayed lines in this corpus carry
// no reasoning and no cache-write; the derivation sums those per line, so a
// corpus change adding either to a replayed line fails the test instead of
// passing silently.
//
// Operational mode: set AUB_DIFFERENTIAL_LEGACY_BIN to a real legacy
// executable to run the same comparison against that tool. The executable
// must accept `--since YYYY-MM-DD --days N <corpus-dir>` and print the legacy
// spend JSON schema the harness parses (a `periods` array of period, input,
// output, cache_read, cache_write and total). Without the variable the
// operational test prints one skip line and passes, so a skip is never
// silent.
//
// Report artifact: the JSON report is written to a temporary directory, or to
// the path named by AUB_DIFFERENTIAL_REPORT_PATH. The variable must name a
// path outside the repository; the test refuses otherwise. Either way
// `cargo test` never writes under the repository.
// ---------------------------------------------------------------------------

/// Raw per-record token counts read straight from one corpus line.
#[derive(Debug, Clone, Copy, Default)]
struct CorpusRecord {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    reasoning: u64,
}

/// One independently derived legacy expectation for a single day.
struct DerivedDayExpectation {
    legacy: TokenUsage,
    /// (category, delta, reason) triples; deltas never carry cache_write,
    /// which the classifier's automatic arm accounts for instead.
    rules: Vec<(DiscrepancyCategory, TokenDelta, String)>,
}

fn add_delta(acc: &mut TokenDelta, other: &TokenDelta) {
    acc.input += other.input;
    acc.output += other.output;
    acc.cache_read += other.cache_read;
    acc.cache_write += other.cache_write;
    acc.total += other.total;
}

fn neg_record_delta(rec: &CorpusRecord) -> TokenDelta {
    TokenDelta {
        input: -(rec.input as i64),
        output: -(rec.output as i64),
        cache_read: -(rec.cache_read as i64),
        cache_write: 0,
        total: -((rec.input + rec.output + rec.cache_read) as i64),
    }
}

/// Derives the legacy side of the multi-week comparison straight from the
/// corpus files, under the legacy model documented in the header above.
/// Reads each JSONL line as a plain JSON value: never the harness classifier,
/// never aub's transcript parser.
fn derive_legacy_expectations(corpus: &Path) -> BTreeMap<String, DerivedDayExpectation> {
    let mut files = Vec::new();
    collect_files_recursive(corpus, &mut files).expect("list multi-week corpus");
    files.sort();

    struct DayAcc {
        legacy: TokenUsage,
        subagents: TokenDelta,
        replay: TokenDelta,
        parser: TokenDelta,
        bug: TokenDelta,
        /// message id -> legacy contributions in encounter order, for replay detection.
        /// Subagent records never enter this map: legacy saw nothing to replay.
        by_id: BTreeMap<String, Vec<CorpusRecord>>,
    }

    let mut days: BTreeMap<String, DayAcc> = BTreeMap::new();

    for file in &files {
        if file.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let text = fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("read corpus file {}: {e}", file.display()));
        let is_subagent = file.to_string_lossy().contains("subagents");
        let mut seen_cache_write_in_file = false;
        for (line_no, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(line).unwrap_or_else(|e| {
                panic!("parse {} line {} as JSON: {e}", file.display(), line_no + 1)
            });
            // Lines without usage carry no tokens (for example codex session
            // metadata) and contribute to neither side.
            let (rec, id) = if let Some(usage) = v.get("message").and_then(|m| m.get("usage")) {
                let get = |field: &str| usage.get(field).and_then(Value::as_u64).unwrap_or(0);
                let rec = CorpusRecord {
                    input: get("input_tokens"),
                    output: get("output_tokens"),
                    cache_read: get("cache_read_input_tokens"),
                    cache_write: get("cache_creation_input_tokens"),
                    reasoning: 0,
                };
                let id = v
                    .get("message")
                    .and_then(|m| m.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{}:{}", file.display(), line_no));
                (rec, id)
            } else if let Some(counts) = v
                .get("payload")
                .and_then(|p| p.get("info"))
                .and_then(|i| i.get("total_token_usage"))
            {
                let get = |field: &str| counts.get(field).and_then(Value::as_u64).unwrap_or(0);
                let rec = CorpusRecord {
                    input: get("input_tokens"),
                    output: get("output_tokens"),
                    cache_read: get("cached_input_tokens"),
                    cache_write: get("cache_write_input_tokens"),
                    reasoning: get("reasoning_output_tokens"),
                };
                (rec, format!("{}:{}", file.display(), line_no))
            } else {
                continue;
            };
            let day = v
                .get("timestamp")
                .and_then(Value::as_str)
                .unwrap_or_else(|| {
                    panic!(
                        "corpus usage line without timestamp: {} line {}",
                        file.display(),
                        line_no + 1
                    )
                });
            assert!(
                day.len() >= 10,
                "corpus timestamp has no day part: {} line {}",
                file.display(),
                line_no + 1
            );
            let day = day[..10].to_string();

            let acc = days.entry(day.clone()).or_insert_with(|| DayAcc {
                legacy: TokenUsage::default(),
                subagents: TokenDelta::default(),
                replay: TokenDelta::default(),
                parser: TokenDelta::default(),
                bug: TokenDelta::default(),
                by_id: BTreeMap::new(),
            });

            if is_subagent {
                // Legacy never discovered nested transcripts.
                let d = TokenDelta {
                    input: rec.input as i64,
                    output: rec.output as i64,
                    cache_read: rec.cache_read as i64,
                    cache_write: 0,
                    total: (rec.input + rec.output + rec.cache_read) as i64,
                };
                add_delta(&mut acc.subagents, &d);
                if rec.cache_write > 0 {
                    seen_cache_write_in_file = true;
                }
                continue;
            }

            // Legacy's codex parser counts reasoning as output.
            let mut legacy_rec = rec;
            if rec.reasoning > 0 {
                legacy_rec.output += rec.reasoning;
                let d = TokenDelta {
                    input: 0,
                    output: -(rec.reasoning as i64),
                    cache_read: 0,
                    cache_write: 0,
                    total: -(rec.reasoning as i64),
                };
                add_delta(&mut acc.parser, &d);
            }
            // Legacy's cumulative importer inverts a record ingested after a
            // cache-write record in the same file.
            if seen_cache_write_in_file {
                let swapped = CorpusRecord {
                    input: legacy_rec.output,
                    output: legacy_rec.input,
                    ..legacy_rec
                };
                let d = TokenDelta {
                    input: rec.input as i64 - swapped.input as i64,
                    output: rec.output as i64 - swapped.output as i64,
                    cache_read: 0,
                    cache_write: 0,
                    total: (rec.input as i64 + rec.output as i64)
                        - (swapped.input as i64 + swapped.output as i64),
                };
                add_delta(&mut acc.bug, &d);
                legacy_rec = swapped;
            }
            // Legacy never reports cache_write.
            legacy_rec.cache_write = 0;
            acc.legacy = TokenUsage::new(
                acc.legacy.input + legacy_rec.input,
                acc.legacy.output + legacy_rec.output,
                acc.legacy.cache_read + legacy_rec.cache_read,
                acc.legacy.cache_write,
            );
            acc.by_id.entry(id).or_default().push(legacy_rec);
            if rec.cache_write > 0 {
                seen_cache_write_in_file = true;
            }
        }
    }

    // Legacy sums every line including replayed ones; the replay category is
    // the superseded (all but last) contribution per message id and day.
    let mut out = BTreeMap::new();
    for (day, mut acc) in days {
        let mut replay = TokenDelta::default();
        for records in acc.by_id.values() {
            for superseded in records.iter().take(records.len().saturating_sub(1)) {
                add_delta(&mut replay, &neg_record_delta(superseded));
            }
        }
        acc.replay = replay;
        let mut rules = Vec::new();
        if !acc.subagents.is_zero() {
            rules.push((
                DiscrepancyCategory::NewlyDiscoveredSubagents,
                acc.subagents,
                "legacy did not discover nested subagent transcripts".to_string(),
            ));
        }
        if !acc.replay.is_zero() {
            rules.push((
                DiscrepancyCategory::ReplayRemoval,
                acc.replay,
                "legacy counted replayed lines again; aub deduplicated".to_string(),
            ));
        }
        if !acc.parser.is_zero() {
            rules.push((
                DiscrepancyCategory::ParserCorrection,
                acc.parser,
                "legacy codex parser counted reasoning tokens as output".to_string(),
            ));
        }
        if !acc.bug.is_zero() {
            rules.push((
                DiscrepancyCategory::LegacyBug,
                acc.bug,
                "legacy cumulative differencing inversion bug".to_string(),
            ));
        }
        out.insert(
            day,
            DerivedDayExpectation {
                legacy: acc.legacy,
                rules,
            },
        );
    }
    out
}

fn derived_rules_for_harness(
    derived: &BTreeMap<String, DerivedDayExpectation>,
) -> Vec<DiscrepancyRule> {
    let mut rules = Vec::new();
    for (day, exp) in derived {
        for (category, delta, reason) in &exp.rules {
            rules.push(DiscrepancyRule {
                period: day.clone(),
                category: *category,
                delta: *delta,
                reason: reason.clone(),
            });
        }
    }
    rules
}

fn create_multi_week_config(state: &StateDir, corpus: &Path) -> PathBuf {
    let cfg_path = state.path().join("aub.toml");
    let content = format!(
        r#"
[[transcripts]]
name = "claude-code"
root = "{}"
pattern = "**/*.jsonl"
format = "claude-code"

[[transcripts]]
name = "codex"
root = "{}"
pattern = "**/*.jsonl"
format = "codex"
"#,
        corpus.join("claude-code").display(),
        corpus.join("codex").display()
    );
    fs::write(&cfg_path, content).expect("write aub.toml");
    cfg_path
}

fn multi_week_aub_args() -> Vec<String> {
    vec![
        "spend".to_string(),
        "--since".to_string(),
        "2026-08-03".to_string(),
        "--days".to_string(),
        "28".to_string(),
        "--group-by".to_string(),
        "day".to_string(),
        "--refresh".to_string(),
        "force".to_string(),
        "--format".to_string(),
        "json".to_string(),
    ]
}

/// Runs the real `aub spend --format json` binary over the multi-week corpus
/// and parses its per-day output. This is the same binary invocation the
/// end-to-end harness runner performs, minus the legacy process.
fn run_aub_spend_over_multi_week_corpus(
    aub_bin: &Path,
    config: &Path,
    state: &StateDir,
) -> BTreeMap<String, TokenUsage> {
    let output = Command::new(aub_bin)
        .args(multi_week_aub_args())
        .env("AUB_CONFIG_FILE", config)
        .env("AUB_STATE_DIR", state.path())
        .output()
        .expect("spawn aub spend over multi-week corpus");
    assert!(
        output.status.success(),
        "aub spend over multi-week corpus failed: status {:?} stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("aub spend output must be UTF-8");
    parse_aub_json(&stdout).expect("parse aub spend output over multi-week corpus")
}

/// The one line the operational test prints when no real legacy executable is
/// configured, so a skip is never silent.
fn operational_skip_line() -> String {
    "SKIP operational_multi_week_against_real_legacy_tool: \
     AUB_DIFFERENTIAL_LEGACY_BIN is not set, so the real-legacy comparison \
     did not run (set it to a legacy spend executable to opt in)"
        .to_string()
}

#[test]
fn multi_week_differential_expectations_derive_from_corpus() {
    let corpus = multi_week_corpus_path();
    let digest = compute_corpus_content_digest(&corpus).expect("multi-week digest");
    assert!(
        !digest.is_empty(),
        "multi-week corpus digest must not be empty"
    );

    // Legacy side: independent parse of the corpus files.
    let derived = derive_legacy_expectations(&corpus);
    assert!(
        !derived.is_empty(),
        "derivation must yield at least one day from the multi-week corpus"
    );
    let rules = derived_rules_for_harness(&derived);

    // Aub side: the real binary over the same corpus.
    let aub_bin = PathBuf::from(env!("CARGO_BIN_EXE_aub"));
    let state = StateDir::new();
    let config = create_multi_week_config(&state, &corpus);
    let aub_periods = run_aub_spend_over_multi_week_corpus(&aub_bin, &config, &state);

    let mut day_keys: BTreeSet<String> = BTreeSet::new();
    day_keys.extend(aub_periods.keys().cloned());
    day_keys.extend(derived.keys().cloned());

    let mut periods = Vec::new();
    for day in &day_keys {
        let aub = aub_periods.get(day).copied().unwrap_or_default();
        let legacy = derived.get(day).map(|e| e.legacy).unwrap_or_default();
        periods.push(classify_period_discrepancy(day, &aub, &legacy, &rules));
    }

    let report = generate_differential_report(digest.clone(), periods);

    // Per day and per category, the harness report must equal the derivation.
    // The cache-write category has no explicit rule: the classifier accounts
    // for it automatically, so its expectation is the automatic arm.
    for cmp in &report.periods {
        let exp = derived.get(&cmp.period);
        for cat in DiscrepancyCategory::ALL {
            let mut actual = TokenDelta::default();
            for e in cmp
                .classified_explanations
                .iter()
                .filter(|e| e.category == cat)
            {
                add_delta(&mut actual, &e.delta);
            }
            let mut expected = TokenDelta::default();
            if cat == DiscrepancyCategory::CacheWriteVisibility {
                let legacy_write = exp.map(|e| e.legacy.cache_write).unwrap_or(0);
                if legacy_write == 0 && cmp.aub.cache_write > 0 {
                    expected.cache_write = cmp.aub.cache_write as i64;
                    expected.total = cmp.aub.cache_write as i64;
                }
            } else if let Some(e) = exp {
                for (_, d, _) in e.rules.iter().filter(|(c, _, _)| *c == cat) {
                    add_delta(&mut expected, d);
                }
            }
            assert_eq!(
                actual,
                expected,
                "week {} category {}: harness delta {:?} != derived {:?}",
                cmp.period,
                cat.name(),
                actual,
                expected
            );
        }
        assert!(
            cmp.unclassified_delta.is_zero(),
            "week {} has unclassified delta {:?} (aub {:?} vs legacy {:?})",
            cmp.period,
            cmp.unclassified_delta,
            cmp.aub,
            cmp.legacy
        );
    }

    assert_eq!(
        report.unclassified.count, 0,
        "multi-week run must have zero unclassified discrepancies, got {:?}",
        report.unclassified
    );
    assert!(
        report.is_retirement_ready(),
        "multi-week corpus run must be retirement ready"
    );
    assert_eq!(report.corpus_content_digest, digest);

    for cat in DiscrepancyCategory::ALL {
        let acc = report
            .category_breakdown
            .get(&cat)
            .unwrap_or_else(|| panic!("category {} missing from breakdown", cat.name()));
        assert!(
            acc.absolute_tokens > 0,
            "category {} must have positive accounting in multi-week corpus (absolute 0 tokens)",
            cat.name()
        );
    }

    // The report artifact leaves the repository alone: a temporary directory,
    // or an explicit path outside the repository.
    let artifact_path = match std::env::var("AUB_DIFFERENTIAL_REPORT_PATH") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => {
            std::env::temp_dir().join(format!("aub-multi-week-report-{}.json", std::process::id()))
        }
    };
    assert!(
        !artifact_path.starts_with(repo_root()),
        "report artifact must not land under the repository, got {}",
        artifact_path.display()
    );
    if let Some(parent) = artifact_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).expect("create report artifact parent");
    }
    let json_text = serde_json::to_string_pretty(&report.to_json_value()).unwrap();
    fs::write(&artifact_path, &json_text).expect("write multi-week report artifact");
    assert!(
        artifact_path.is_file(),
        "report artifact must exist at {}",
        artifact_path.display()
    );
    println!(
        "multi-week differential report: {}",
        artifact_path.display()
    );
}

#[test]
fn integration_operational_mode_skip_line_without_legacy_bin() {
    if std::env::var_os("AUB_DIFFERENTIAL_LEGACY_BIN").is_some() {
        println!(
            "operator opted in via AUB_DIFFERENTIAL_LEGACY_BIN, \
             so the skip line cannot be observed in this run"
        );
        return;
    }
    let line = operational_skip_line();
    assert!(
        line.contains("AUB_DIFFERENTIAL_LEGACY_BIN"),
        "skip line must name the variable, got: {line}"
    );
    assert!(
        line.to_uppercase().contains("SKIP"),
        "skip line must say it was skipped, got: {line}"
    );
    println!("{line}");
}

#[test]
fn operational_multi_week_against_real_legacy_tool() {
    let legacy_bin = match std::env::var("AUB_DIFFERENTIAL_LEGACY_BIN") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => {
            println!("{}", operational_skip_line());
            return;
        }
    };

    let corpus = multi_week_corpus_path();
    let state = StateDir::new();
    let config = create_multi_week_config(&state, &corpus);
    let aub_bin = PathBuf::from(env!("CARGO_BIN_EXE_aub"));
    let aub_args = multi_week_aub_args();
    let aub_envs = [
        ("AUB_CONFIG_FILE", config.to_str().unwrap()),
        ("AUB_STATE_DIR", state.path().to_str().unwrap()),
    ];
    let legacy_args = vec![
        "--since".to_string(),
        "2026-08-03".to_string(),
        "--days".to_string(),
        "28".to_string(),
        corpus.to_string_lossy().to_string(),
    ];
    let derived = derive_legacy_expectations(&corpus);
    let rules = derived_rules_for_harness(&derived);

    match run_differential_executables(
        &aub_bin,
        &aub_args,
        &aub_envs,
        &legacy_bin,
        &legacy_args,
        &[],
        &corpus,
        &rules,
        "multi-week-operational",
        Duration::from_secs(60),
    ) {
        Ok((report, _)) => {
            println!("{}", report.render_text());
            assert!(
                report.is_retirement_ready(),
                "operational comparison against {} must be retirement ready",
                legacy_bin.display()
            );
        }
        Err(log) => panic!(
            "operational comparison against {} failed: {}",
            legacy_bin.display(),
            log.outcome
        ),
    }
}

// ---------------------------------------------------------------------------
// E2E Tests: invoke release aub and deterministic stub legacy executable across:
// agreement, classified disagreement, unclassified disagreement,
// child nonzero exit, timeout and malformed output.
// The run log preserves both exact argv, binary and fixture digests,
// stdout/stderr artifacts, exits or signals, per-category deltas, unclassified count.
// ---------------------------------------------------------------------------
#[test]
fn e2e_differential_harness_across_all_execution_scenarios() {
    let aub_bin = PathBuf::from(env!("CARGO_BIN_EXE_aub"));
    let legacy_bin = legacy_stub_bin();
    let corpus = small_corpus_path();
    let state = StateDir::new();
    let config = create_test_config(&state, &corpus);

    let aub_args = vec![
        "spend".to_string(),
        "--since".to_string(),
        "2026-08-25".to_string(),
        "--days".to_string(),
        "2".to_string(),
        "--group-by".to_string(),
        "day".to_string(),
        "--refresh".to_string(),
        "force".to_string(),
        "--format".to_string(),
        "json".to_string(),
    ];
    let aub_envs = [
        ("AUB_CONFIG_FILE", config.to_str().unwrap()),
        ("AUB_STATE_DIR", state.path().to_str().unwrap()),
    ];

    // Scenario 1: Agreement
    {
        let legacy_args = vec![
            "--since".to_string(),
            "2026-08-25".to_string(),
            "--days".to_string(),
            "2".to_string(),
            "--scenario".to_string(),
            "agreement".to_string(),
        ];
        let (report, run_log) = run_differential_executables(
            &aub_bin,
            &aub_args,
            &aub_envs,
            &legacy_bin,
            &legacy_args,
            &[],
            &corpus,
            &[],
            "agreement",
            Duration::from_secs(5),
        )
        .expect("agreement scenario must pass");

        assert_eq!(run_log.scenario, "agreement");
        assert_eq!(run_log.unclassified_count, 0);
        assert_eq!(run_log.aub.exit_code, Some(0));
        assert_eq!(run_log.legacy.exit_code, Some(0));
        assert_eq!(run_log.aub.argv[1], "spend");
        assert!(!run_log.aub.binary_digest.is_empty());
        assert!(!run_log.legacy.binary_digest.is_empty());
        assert!(!run_log.corpus_digest.is_empty());
        assert!(report.is_retirement_ready());
    }

    // Scenario 2: Classified Disagreement
    {
        let legacy_args = vec![
            "--since".to_string(),
            "2026-08-25".to_string(),
            "--days".to_string(),
            "2".to_string(),
            "--scenario".to_string(),
            "classified_disagreement".to_string(),
        ];
        let rules = vec![
            DiscrepancyRule {
                period: "2026-08-25".to_string(),
                category: DiscrepancyCategory::NewlyDiscoveredSubagents,
                delta: TokenDelta {
                    input: 200,
                    output: 100,
                    cache_read: 0,
                    cache_write: 0,
                    total: 300,
                },
                reason: "nested subagent".to_string(),
            },
            DiscrepancyRule {
                period: "2026-08-26".to_string(),
                category: DiscrepancyCategory::ReplayRemoval,
                delta: TokenDelta {
                    input: 0,
                    output: -100,
                    cache_read: 0,
                    cache_write: 0,
                    total: -100,
                },
                reason: "replay stream".to_string(),
            },
            DiscrepancyRule {
                period: "2026-08-26".to_string(),
                category: DiscrepancyCategory::ParserCorrection,
                delta: TokenDelta {
                    input: 10,
                    output: 0,
                    cache_read: 0,
                    cache_write: 0,
                    total: 10,
                },
                reason: "parser correction".to_string(),
            },
            DiscrepancyRule {
                period: "2026-08-26".to_string(),
                category: DiscrepancyCategory::LegacyBug,
                delta: TokenDelta {
                    input: 0,
                    output: -5,
                    cache_read: 0,
                    cache_write: 0,
                    total: -5,
                },
                reason: "legacy bug off-by-one".to_string(),
            },
        ];

        let (report, run_log) = run_differential_executables(
            &aub_bin,
            &aub_args,
            &aub_envs,
            &legacy_bin,
            &legacy_args,
            &[],
            &corpus,
            &rules,
            "classified_disagreement",
            Duration::from_secs(5),
        )
        .expect("classified disagreement scenario must pass with all differences accounted for");

        assert_eq!(run_log.scenario, "classified_disagreement");
        assert_eq!(run_log.unclassified_count, 0);
        assert!(report.is_retirement_ready());
        assert!(run_log.per_category_deltas.len() >= 4);
    }

    // Scenario 3: Unclassified Disagreement (fails harness)
    {
        let legacy_args = vec![
            "--since".to_string(),
            "2026-08-25".to_string(),
            "--days".to_string(),
            "2".to_string(),
            "--scenario".to_string(),
            "unclassified_disagreement".to_string(),
        ];
        let rules = vec![
            DiscrepancyRule {
                period: "2026-08-25".to_string(),
                category: DiscrepancyCategory::NewlyDiscoveredSubagents,
                delta: TokenDelta {
                    input: 200,
                    output: 100,
                    cache_read: 0,
                    cache_write: 0,
                    total: 300,
                },
                reason: "nested subagent".to_string(),
            },
            DiscrepancyRule {
                period: "2026-08-26".to_string(),
                category: DiscrepancyCategory::ReplayRemoval,
                delta: TokenDelta {
                    input: 0,
                    output: -100,
                    cache_read: 0,
                    cache_write: 0,
                    total: -100,
                },
                reason: "replay stream".to_string(),
            },
            DiscrepancyRule {
                period: "2026-08-26".to_string(),
                category: DiscrepancyCategory::ParserCorrection,
                delta: TokenDelta {
                    input: 10,
                    output: 0,
                    cache_read: 0,
                    cache_write: 0,
                    total: 10,
                },
                reason: "parser correction".to_string(),
            },
            DiscrepancyRule {
                period: "2026-08-26".to_string(),
                category: DiscrepancyCategory::LegacyBug,
                delta: TokenDelta {
                    input: 0,
                    output: -5,
                    cache_read: 0,
                    cache_write: 0,
                    total: -5,
                },
                reason: "legacy bug off-by-one".to_string(),
            },
        ];

        let result = run_differential_executables(
            &aub_bin,
            &aub_args,
            &aub_envs,
            &legacy_bin,
            &legacy_args,
            &[],
            &corpus,
            &rules,
            "unclassified_disagreement",
            Duration::from_secs(5),
        );
        let err_log = match result {
            Ok(_) => panic!("unclassified disagreement must fail"),
            Err(l) => l,
        };
        assert_eq!(err_log.scenario, "unclassified_disagreement");
        assert!(err_log.unclassified_count > 0);
        assert!(
            err_log
                .outcome
                .contains("unclassified discrepancies remain")
        );
    }

    // Scenario 4: Child Nonzero Exit
    {
        let legacy_args = vec!["--scenario".to_string(), "child_nonzero_exit".to_string()];
        let result = run_differential_executables(
            &aub_bin,
            &aub_args,
            &aub_envs,
            &legacy_bin,
            &legacy_args,
            &[],
            &corpus,
            &[],
            "child_nonzero_exit",
            Duration::from_secs(5),
        );
        let err_log = match result {
            Ok(_) => panic!("child nonzero exit must fail"),
            Err(l) => l,
        };
        assert_eq!(err_log.scenario, "child_nonzero_exit");
        assert_eq!(err_log.legacy.exit_code, Some(1));
        assert!(
            err_log
                .legacy
                .stderr
                .contains("database lock timeout or process crashed")
        );
        assert!(err_log.outcome.contains("failed with exit code Some(1)"));
    }

    // Scenario 5: Timeout
    {
        let legacy_args = vec!["--scenario".to_string(), "timeout".to_string()];
        let result = run_differential_executables(
            &aub_bin,
            &aub_args,
            &aub_envs,
            &legacy_bin,
            &legacy_args,
            &[],
            &corpus,
            &[],
            "timeout",
            Duration::from_millis(500),
        );
        let err_log = match result {
            Ok(_) => panic!("timeout scenario must fail"),
            Err(l) => l,
        };
        assert_eq!(err_log.scenario, "timeout");
        assert!(err_log.outcome.contains("timed out"));
    }

    // Scenario 6: Malformed Output
    {
        let legacy_args = vec!["--scenario".to_string(), "malformed_output".to_string()];
        let result = run_differential_executables(
            &aub_bin,
            &aub_args,
            &aub_envs,
            &legacy_bin,
            &legacy_args,
            &[],
            &corpus,
            &[],
            "malformed_output",
            Duration::from_secs(5),
        );
        let err_log = match result {
            Ok(_) => panic!("malformed output must fail"),
            Err(l) => l,
        };
        assert_eq!(err_log.scenario, "malformed_output");
        assert!(
            err_log
                .legacy
                .stdout
                .contains("<<<MALFORMED_OUTPUT_NOT_JSON>>>")
        );
        assert!(err_log.outcome.contains("malformed legacy output"));
    }
}
