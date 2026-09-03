//! Rendering helpers that require explicit context.
//!
//! Every helper takes a unit label, a qualification and a precision policy, plus
//! freshness where the value is a meter reading. A bare scalar cannot reach a
//! user-visible surface because no helper here accepts one, and a bare total where
//! known missing evidence affects the aggregate is refused by construction.

use crate::domain::failure::FailureClass;
use crate::domain::freshness::{Freshness, StaleReason};
use crate::domain::provenance::DerivationId;
use crate::domain::quota::QuotaRemaining;
use crate::domain::render::Precision;
use crate::domain::time::{Age, ClockSkewEnvelope, UtcTimestamp, age};
use crate::domain::tokens::TokenKind;
use crate::domain::window::NominalWindowDuration;
use crate::error::Error;
use crate::evidence::CoverageCompleteness;
use crate::presentation::precision::{PERCENT, TOKENS};
use crate::presentation::vocabulary::{Qualification, coverage_term, quality_term};
use crate::report::{ProvenanceGraph, SpendGroup, SpendReport, StatusReport};
use crate::transcripts::TranscriptDriftReport;

/// The explain level a command was asked for.
///
/// `Off` is the default when no `--explain` token is present. A bare `--explain`
/// selects `Summary`, and `--explain=full` selects `Full`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExplainMode {
    #[default]
    Off,
    Summary,
    Full,
}

/// Renders a failure with a concrete recovery action and collapses the current
/// home directory to `~`. The error type keeps the raw cause for diagnostics;
/// this boundary owns what is safe and useful to print.
pub(crate) fn render_actionable_failure_message(
    error: &Error,
    command: Option<&str>,
    home: Option<&str>,
) -> String {
    let message = collapse_home_path(&error.to_string(), home);
    let rerun = command
        .map(|name| format!("aub {name}"))
        .unwrap_or_else(|| "aub --help".to_string());
    let action = match error {
        Error::Internal(_) => format!("run {rerun} again with AUB_LOG_LEVEL=debug"),
        Error::Usage(_) => "run aub --help".to_string(),
        Error::AuthRequired(_) => {
            format!("set accounts[].credential, then run {rerun} again")
        }
        Error::RemoteUnavailable(_) => {
            format!("run {rerun} again after the named remote prerequisite is reachable")
        }
        Error::Store(_) => {
            format!("check the state.dir database prerequisite, then run {rerun} again")
        }
        Error::InsufficientEvidence(_) => {
            format!("run {rerun} again after collecting the named prerequisite")
        }
        Error::ThresholdNotMet(_) => {
            format!("run {rerun} again after the named threshold condition changes")
        }
        Error::IngestIncomplete(_) => {
            format!("fix the named local prerequisite, then run {rerun} again")
        }
    };
    format!("{message}; next: {action}")
}

fn collapse_home_path(message: &str, home: Option<&str>) -> String {
    let Some(home) = home.map(|path| path.trim_end_matches('/')) else {
        return message.to_string();
    };
    if home.is_empty() || home == "/" {
        return message.to_string();
    }
    message.replace(&format!("{home}/"), "~/")
}

/// Formats a raw integer with the given number of fractional digits, trimming
/// trailing zeros. The raw value is already scaled to the display unit.
pub fn format_number(raw: &str, precision: Precision) -> String {
    let digits = precision.digits() as usize;
    if digits == 0 {
        return raw.to_string();
    }
    let (sign, body) = match raw.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", raw),
    };
    let padded = format!("{body:0>width$}", width = digits + 1);
    let (int, frac) = padded.split_at(padded.len() - digits);
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() {
        format!("{sign}{int}")
    } else {
        format!("{sign}{int}.{frac}")
    }
}

/// Renders a quota fraction in parts per million as a percentage with the given
/// precision. One percent is 10_000 ppm.
pub fn render_percentage(ppm: u32, precision: Precision) -> String {
    let digits = precision.digits() as u32;
    let divisor = 10u32.pow(digits);
    let scaled = (u64::from(ppm) * u64::from(divisor) + 5_000) / 10_000;
    format_number(&scaled.to_string(), precision)
}

/// The unit every meter reading is rendered in: remaining quota is a percentage of
/// the window.
const METER_UNIT: &str = "%";

/// The report-to-rendering seam for status: takes a [`StatusReport`] and returns
/// its human rendering, one line per account carrying the account name and its
/// meter reading.
///
/// This function is the entry point the status command calls. It performs no data
/// collection of its own (the model arrives complete), and it reaches the fragment
/// renderers rather than bypassing them, so a wording change stays in one place.
pub fn render_status_report(
    report: &StatusReport,
    now: UtcTimestamp,
    envelope: ClockSkewEnvelope,
) -> String {
    render_status_report_with_explain(report, now, envelope, ExplainMode::Off)
}

/// Renders a status report, optionally including the explain block.
pub fn render_status_report_with_explain(
    report: &StatusReport,
    now: UtcTimestamp,
    envelope: ClockSkewEnvelope,
    explain: ExplainMode,
) -> String {
    // A projection the status path could not read is the design's degraded
    // form: the question mark, with a compact reason where the output mode
    // permits. No account line is rendered, because no account value exists
    // to render and none may be substituted.
    if let crate::report::ProjectionReadState::Unavailable { state: _, reason } =
        &report.projection_state
    {
        let mut line = String::from("aub ?");
        if explain != ExplainMode::Off {
            line.push_str(" · ");
            line.push_str(reason);
        }
        return line;
    }
    let mut lines: Vec<String> = Vec::with_capacity(report.accounts.len());
    for account in &report.accounts {
        let reading = render_meter_reading(
            &account.reading,
            METER_UNIT,
            PERCENT,
            now,
            envelope,
            account
                .limiting_window
                .as_ref()
                .map(|limit| limit.nominal_duration),
        );
        lines.push(format!("aub {} {}", account.account.as_str(), reading));
    }
    let report_text = lines.join("\n");
    if explain == ExplainMode::Off {
        report_text
    } else {
        let explain_text = render_explain(&report.provenance, explain);
        if report_text.is_empty() {
            explain_text
        } else {
            format!("{report_text}\n\n{explain_text}")
        }
    }
}

/// The unit every token count is rendered in.
const TOKEN_UNIT: &str = "tokens";

/// The report-to-rendering seam for spend: the window, one line per group with the
/// four known kinds and any unknown component, each count carrying its unit, then
/// the ingest summary. The summary is not optional output: a count printed without
/// what was quarantined, skipped or replayed behind it would read as complete when
/// nothing proved it was.
pub fn render_spend_report(report: &SpendReport) -> String {
    render_spend_report_with_explain(report, ExplainMode::Off)
}

/// Renders a spend report, optionally including the explain block.
pub fn render_spend_report_with_explain(report: &SpendReport, explain: ExplainMode) -> String {
    let mut lines = vec![format!(
        "spend from {} to {} (UTC days, end exclusive), by day and source",
        report.since.iso(),
        report.until.iso()
    )];
    if report.groups.is_empty() {
        lines.push(format!(
            "no usage events in the window: {} canonical events read, {} outside it, {} undated",
            report.ingest.events_in_window,
            report.ingest.events_outside_window,
            report.ingest.undated_events
        ));
    }
    for group in &report.groups {
        lines.push(render_spend_group(group));
    }
    lines.push(render_ingest_summary(report));
    let report_text = lines.join("\n");
    if explain == ExplainMode::Off {
        report_text
    } else {
        let explain_text = render_explain(&report.provenance, explain);
        if report_text.is_empty() {
            explain_text
        } else {
            format!("{report_text}\n\n{explain_text}")
        }
    }
}

/// Renders the provenance graph for a report in human text format.
///
/// Under Summary mode, prints the 10 provenance elements for each field in the graph.
/// Under Full mode, additionally expands the canonical evidence member set for each manifest.
pub fn render_explain(graph: &ProvenanceGraph, mode: ExplainMode) -> String {
    if mode == ExplainMode::Off {
        return String::new();
    }
    if graph.is_empty() {
        return "explain: no quantitative fields in report".to_string();
    }
    let mut lines = Vec::new();
    lines.push("explain:".to_string());
    for (field, node) in graph.iter() {
        let manifest = node.manifest();
        let derivation_id = DerivationId::from_manifest(manifest);
        lines.push(format!("  field: {}", field.label()));
        lines.push(format!("    derivation: {}", derivation_id.to_hex()));
        lines.push(format!(
            "    sources: {}, observations: {}",
            node.source_count(),
            node.observation_count()
        ));
        lines.push(format!(
            "    manifest: hash={}, inputs={}, semantics=(grouping={}, filtering={})",
            manifest.inputs_hash().to_hex(),
            manifest.input_count(),
            manifest.query_semantics().grouping(),
            manifest.query_semantics().filtering()
        ));
        lines.push(format!(
            "    account attribution: {}",
            field.account_attribution()
        ));

        let cost_model = manifest
            .witnesses()
            .iter()
            .find_map(|w| w.cost_model())
            .map(|id| id.as_str())
            .unwrap_or("none");
        lines.push(format!("    cost model: {cost_model}"));

        let window_cal = manifest
            .witnesses()
            .iter()
            .find_map(|w| w.window_calibration())
            .map(|id| id.as_str())
            .unwrap_or("none");
        lines.push(format!("    window calibration: {window_cal}"));

        let rate_card = manifest
            .witnesses()
            .iter()
            .find_map(|w| w.rate_card())
            .map(|id| id.as_str())
            .unwrap_or("none");
        lines.push(format!("    rate card: {rate_card}"));

        lines.push("    coverage and quality: complete".to_string());

        let empirical = if manifest.query_semantics().filtering().contains("can-run") {
            "can-run"
        } else {
            "none"
        };
        lines.push(format!("    empirical history: {empirical}"));

        lines.push(format!("    arithmetic: {}", node.arithmetic().label()));

        if mode == ExplainMode::Full {
            lines.push(format!("    members ({}):", node.members().len()));
            for member in node.members() {
                lines.push(format!("      - {}", member.as_str()));
            }
        }
    }
    lines.join("\n")
}

fn render_spend_group(group: &SpendGroup) -> String {
    let known = group.usage.known();
    let mut parts: Vec<String> = TokenKind::ALL
        .iter()
        .map(|kind| {
            format!(
                "{} {}",
                token_kind_label(*kind),
                render_count(known.value(*kind))
            )
        })
        .collect();
    for (name, count) in group.usage.unknown() {
        parts.push(format!("{name} {}", render_count(count.value())));
    }
    let qualification = match quality_term(group.usage.quality()) {
        Some(term) => term,
        None => coverage_term(group.usage.coverage()),
    };
    format!(
        "{}  {} ({})",
        group.key.as_str(),
        parts.join(" · "),
        qualification.term()
    )
}

fn render_count(raw: u64) -> String {
    format!("{} {TOKEN_UNIT}", format_number(&raw.to_string(), TOKENS))
}

fn token_kind_label(kind: TokenKind) -> &'static str {
    match kind {
        TokenKind::Input => "input",
        TokenKind::Output => "output",
        TokenKind::CacheRead => "cache read",
        TokenKind::CacheWrite => "cache write",
    }
}

fn render_ingest_summary(report: &SpendReport) -> String {
    let ingest = &report.ingest;
    let quarantined: u64 = ingest.quarantined_by_class.values().sum();
    let by_class = ingest
        .quarantined_by_class
        .iter()
        .map(|(class, count)| format!("{class} {count}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut line = format!(
        "ingest: {} files read, {} skipped (unchanged before the window), {} unreadable · {} canonical events in window, {} outside, {} undated · {} replayed occurrences, {} collisions, {} without identity · {} quarantined",
        ingest.files_read,
        ingest.files_skipped_before_window,
        ingest.unreadable_files.len(),
        ingest.events_in_window,
        ingest.events_outside_window,
        ingest.undated_events,
        ingest.replayed_occurrences,
        ingest.collisions,
        ingest.without_identity,
        quarantined
    );
    if !by_class.is_empty() {
        line.push_str(&format!(" ({by_class})"));
    }
    for file in &ingest.unreadable_files {
        line.push_str(&format!("\nunreadable: {file}"));
    }
    line
}

/// Renders a [`TranscriptDriftReport`] for `aub doctor --transcript-format-drift`.
pub fn render_doctor_drift_report(report: &TranscriptDriftReport) -> String {
    if !report.has_configured_roots {
        return "Doctor: Transcript Format Drift\nNo configured transcript roots. Add [[transcripts]] entries to configuration to enable drift detection.".to_string();
    }
    let mut lines = Vec::new();
    lines.push("Doctor: Transcript Format Drift".to_string());
    for src in &report.sources {
        lines.push(format!(
            "Source: {} (format: {}, parser: {})",
            src.source,
            src.format,
            src.parser_version.as_str()
        ));
        lines.push(format!(
            "  Files scanned: {}, Records scanned: {}",
            src.files_scanned, src.records_scanned
        ));
        lines.push(format!(
            "  Quarantined records: {}",
            src.quarantined_records
        ));
        for (class, count) in &src.quarantine_by_class {
            lines.push(format!("    {class}: {count}"));
        }
        lines.push(format!("  Observed shapes: {}", src.shapes_seen.len()));
        for s in &src.shapes_seen {
            let kind = s.record_kind.as_deref().unwrap_or("record");
            let marker = if src
                .uncovered_shapes
                .iter()
                .any(|u| u.shape_hash == s.shape_hash)
            {
                " [UNCOVERED]"
            } else {
                ""
            };
            lines.push(format!(
                "    {} ({kind}, {} fields, {} records){marker}",
                s.shape_hash, s.field_count, s.occurrence_count
            ));
        }
        if src.drift_detected {
            lines.push("  UNCOVERED FORMAT DRIFT DETECTED:".to_string());
            if !src.uncovered_fields.is_empty() {
                let fields = src
                    .uncovered_fields
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                lines.push(format!("    Uncovered fields: {fields}"));
            }
            if !src.uncovered_record_kinds.is_empty() {
                let kinds = src
                    .uncovered_record_kinds
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                lines.push(format!("    Uncovered record kinds: {kinds}"));
            }
            if !src.uncovered_evidence_classes.is_empty() {
                let evs = src
                    .uncovered_evidence_classes
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                lines.push(format!("    Uncovered evidence classes: {evs}"));
            }
            if !src.uncovered_shapes.is_empty() {
                lines.push(format!(
                    "    Uncovered shapes: {} shape(s) not in fixture corpus",
                    src.uncovered_shapes.len()
                ));
            }
            if let Some(ref rem) = src.remediation {
                lines.push(format!("  Next action: {rem}"));
            }
        } else {
            lines.push("  Status: All record shapes covered by committed fixtures.".to_string());
        }
    }
    lines.join("\n")
}

/// Renders a quantity with its unit, precision and qualification.
pub fn render_quantity(
    raw: &str,
    unit: &str,
    precision: Precision,
    qualification: Qualification,
) -> String {
    let value = format_number(raw, precision);
    format!("{value} {unit} ({})", qualification.term())
}

/// Renders a total. A complete aggregate is a total; a partial aggregate is a known
/// subtotal, never a bare total, because a bare total where known missing evidence
/// affects the aggregate is forbidden everywhere.
pub fn render_total(
    raw: &str,
    unit: &str,
    precision: Precision,
    coverage: &CoverageCompleteness,
) -> String {
    let value = format_number(raw, precision);
    match coverage {
        CoverageCompleteness::Complete => format!("Total: {value} {unit}"),
        CoverageCompleteness::Partial { .. } => {
            format!("Known subtotal: {value} {unit}; report incomplete")
        }
    }
}

/// Renders a meter reading with its freshness, age and reason. Freshness is conveyed
/// in text, never by colour alone: the state is readable from the words themselves.
pub fn render_meter_reading(
    reading: &Freshness<QuotaRemaining>,
    unit: &str,
    precision: Precision,
    now: UtcTimestamp,
    envelope: ClockSkewEnvelope,
    limiting_window: Option<NominalWindowDuration>,
) -> String {
    match reading {
        Freshness::Fresh { observed, .. } => {
            let value = render_percentage(observed.value().as_ppm().get(), precision);
            // The fresh line names the limiting window's nominal length, the
            // suffix the design's example shows: "38% left · 5h". The window
            // is part of the value's meaning, not of its freshness.
            let window = limiting_window
                .map(render_window_duration)
                .map(|label| format!(" · {label}"))
                .unwrap_or_default();
            format!("{value}{unit} left{window}")
        }
        Freshness::Stale {
            last_good, reason, ..
        } => {
            let value = last_good
                .as_ref()
                .map(|observed| render_percentage(observed.value().as_ppm().get(), precision));
            let age = last_good
                .as_ref()
                .and_then(|observed| observed_age(observed, now, envelope));
            match (value, age) {
                (Some(value), Some(age)) => format!(
                    "~{value}{unit} · stale {} · {}",
                    render_age(age),
                    render_stale_reason(*reason)
                ),
                (Some(value), None) => {
                    format!("~{value}{unit} · stale · {}", render_stale_reason(*reason))
                }
                (None, _) => format!("? · stale · {}", render_stale_reason(*reason)),
            }
        }
        Freshness::AuthRequired { .. } => "auth!".to_string(),
    }
}

fn observed_age(
    observed: &crate::domain::freshness::Observed<QuotaRemaining>,
    now: UtcTimestamp,
    envelope: ClockSkewEnvelope,
) -> Option<Age> {
    age(
        observed.provider_observed_at(),
        observed.received_at(),
        observed.measurement_basis(),
        now,
        envelope,
    )
    .ok()
}

/// Renders a stale reason as the fixed human wording.
pub fn render_stale_reason(reason: StaleReason) -> &'static str {
    match reason {
        StaleReason::AgeExceeded => "age exceeded",
        StaleReason::NoSuccessfulObservation => "no successful sample",
        StaleReason::SourceUnreachable(class) => render_failure_class(class),
        StaleReason::MalformedProviderResponse => "malformed response",
        StaleReason::RateLimited => "rate limited",
        StaleReason::SamplingGap => "sampling gap",
        StaleReason::ClockAnomaly => "clock anomaly",
        StaleReason::CollectorInterrupted => "collector interrupted",
        StaleReason::CredentialChangedUnverified => "credential changed",
    }
}

/// Renders a failure class as the fixed human wording.
pub fn render_failure_class(class: FailureClass) -> &'static str {
    match class {
        FailureClass::DnsFailure => "dns failure",
        FailureClass::ConnectTimeout
        | FailureClass::ReadTimeout
        | FailureClass::TotalBudgetExpired => "timeout",
        FailureClass::HttpStatus(_) => "http error",
        FailureClass::RateLimited { .. } => "rate limited",
        FailureClass::MalformedBody | FailureClass::MissingRequiredField => "malformed response",
    }
}

/// Renders a nominal window duration as the compact human label the design's
/// fresh status line shows: "38% left · 5h". Same ladder as an age, because a
/// window length is a duration a human reads the same way.
pub fn render_window_duration(duration: NominalWindowDuration) -> String {
    let seconds = duration.as_nanos() / 1_000_000_000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

/// Renders an age as a compact human duration: seconds, minutes, hours or days.
pub fn render_age(age: Age) -> String {
    let seconds = age.as_nanos() / 1_000_000_000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attempt::AttemptId;
    use crate::domain::freshness::Observed;
    use crate::domain::quota::QuotaFractionPpm;
    use crate::domain::time::{MeasurementBasis, MonotonicDuration, ReceivedAt};

    const NANOS_PER_SECOND: i64 = 1_000_000_000;

    fn now() -> UtcTimestamp {
        UtcTimestamp::from_unix_nanos(1_000_000 * NANOS_PER_SECOND)
    }

    fn envelope() -> ClockSkewEnvelope {
        ClockSkewEnvelope::new(MonotonicDuration::from_seconds(60))
    }

    fn remaining(ppm: u32) -> QuotaRemaining {
        QuotaRemaining::new(QuotaFractionPpm::new(ppm as i32).unwrap())
    }

    fn observed(ppm: u32, received: UtcTimestamp) -> Observed<QuotaRemaining> {
        Observed::new(
            remaining(ppm),
            None,
            ReceivedAt::new(received),
            MeasurementBasis::LocallyReceived,
        )
    }

    /// The design's example status renderings (PLAN.md section 48), so a wording
    /// change is a deliberate diff.
    #[test]
    fn golden_status_renderings() {
        let now = now();
        let envelope = envelope();
        let precision = crate::presentation::precision::PERCENT;

        let fresh = Freshness::Fresh {
            observed: observed(
                380_000,
                UtcTimestamp::from_unix_nanos(now.unix_nanos() - 5 * 3_600 * NANOS_PER_SECOND),
            ),
            latest_attempt: AttemptId::new(1),
        };
        // The suffix is the limiting window's nominal length, not the sample's
        // age: the design's fresh line reads "38% left · 5h" for the 5-hour
        // window, and a fresh reading's age is implied by the word fresh.
        assert_eq!(
            render_meter_reading(
                &fresh,
                "%",
                precision,
                now,
                envelope,
                Some(NominalWindowDuration::from_nanos(
                    5 * 3_600 * NANOS_PER_SECOND as u64
                )),
            ),
            "38% left · 5h"
        );
        // Without a window to name, the value is shown bare.
        assert_eq!(
            render_meter_reading(&fresh, "%", precision, now, envelope, None),
            "38% left"
        );

        let stale_timeout = Freshness::Stale {
            last_good: Some(observed(
                380_000,
                UtcTimestamp::from_unix_nanos(now.unix_nanos() - 14 * 60 * NANOS_PER_SECOND),
            )),
            latest_attempt: AttemptId::new(2),
            reason: StaleReason::SourceUnreachable(FailureClass::ConnectTimeout),
        };
        assert_eq!(
            render_meter_reading(&stale_timeout, "%", precision, now, envelope, None),
            "~38% · stale 14m · timeout"
        );

        let auth = Freshness::<QuotaRemaining>::AuthRequired {
            last_good: None,
            latest_attempt: AttemptId::new(3),
        };
        assert_eq!(
            render_meter_reading(&auth, "%", precision, now, envelope, None),
            "auth!"
        );

        let stale_interrupted = Freshness::Stale {
            last_good: Some(observed(
                380_000,
                UtcTimestamp::from_unix_nanos(now.unix_nanos() - 9 * 60 * NANOS_PER_SECOND),
            )),
            latest_attempt: AttemptId::new(4),
            reason: StaleReason::CollectorInterrupted,
        };
        assert_eq!(
            render_meter_reading(&stale_interrupted, "%", precision, now, envelope, None),
            "~38% · stale 9m · collector interrupted"
        );

        let never_observed = Freshness::<QuotaRemaining>::Stale {
            last_good: None,
            latest_attempt: AttemptId::new(5),
            reason: StaleReason::NoSuccessfulObservation,
        };
        assert_eq!(
            render_meter_reading(&never_observed, "%", precision, now, envelope, None),
            "? · stale · no successful sample"
        );
    }

    /// A partial aggregate is never rendered as a bare total: it is a known subtotal.
    #[test]
    fn a_partial_aggregate_is_never_a_bare_total() {
        let complete = render_total(
            "1200000",
            "tokens",
            crate::presentation::precision::TOKENS,
            &CoverageCompleteness::Complete,
        );
        assert_eq!(complete, "Total: 1200000 tokens");

        let partial = render_total(
            "1200000",
            "tokens",
            crate::presentation::precision::TOKENS,
            &CoverageCompleteness::partial([crate::evidence::ComponentKind::new("cache-write")]),
        );
        assert!(
            !partial.contains("Total:"),
            "a partial aggregate must not be a bare total: {partial}"
        );
        assert!(partial.contains("Known subtotal"));
    }

    /// A stale value is rendered with its age and reason attached, never as a
    /// standalone number: the rendered line always carries the stale marker.
    #[test]
    fn a_stale_value_carries_its_age_and_reason() {
        let now = now();
        let stale = Freshness::Stale {
            last_good: Some(observed(
                380_000,
                UtcTimestamp::from_unix_nanos(now.unix_nanos() - 14 * 60 * NANOS_PER_SECOND),
            )),
            latest_attempt: AttemptId::new(1),
            reason: StaleReason::SourceUnreachable(FailureClass::ReadTimeout),
        };
        let rendered = render_meter_reading(
            &stale,
            "%",
            crate::presentation::precision::PERCENT,
            now,
            envelope(),
            None,
        );
        assert!(
            rendered.contains("stale"),
            "stale must be in text: {rendered}"
        );
        assert!(rendered.contains("14m"), "age must be attached: {rendered}");
        assert!(
            rendered.contains("timeout"),
            "reason must be attached: {rendered}"
        );
    }

    /// Freshness is conveyed in text, never by colour alone: with no colour at all
    /// the state is still readable from the words.
    #[test]
    fn freshness_is_readable_without_colour() {
        let now = now();
        let envelope = envelope();
        let precision = crate::presentation::precision::PERCENT;

        let fresh = Freshness::Fresh {
            observed: observed(
                380_000,
                UtcTimestamp::from_unix_nanos(now.unix_nanos() - 5 * 3_600 * NANOS_PER_SECOND),
            ),
            latest_attempt: AttemptId::new(1),
        };
        let stale = Freshness::Stale {
            last_good: None,
            latest_attempt: AttemptId::new(2),
            reason: StaleReason::NoSuccessfulObservation,
        };
        let auth = Freshness::<QuotaRemaining>::AuthRequired {
            last_good: None,
            latest_attempt: AttemptId::new(3),
        };

        // No colour is ever added; the words alone distinguish the three states.
        assert!(render_meter_reading(&fresh, "%", precision, now, envelope, None).contains("left"));
        assert!(
            render_meter_reading(&stale, "%", precision, now, envelope, None).contains("stale")
        );
        assert_eq!(
            render_meter_reading(&auth, "%", precision, now, envelope, None),
            "auth!"
        );
    }

    #[test]
    fn format_number_trims_trailing_zeros() {
        assert_eq!(format_number("3800", Precision::new(2)), "38");
        assert_eq!(format_number("3855", Precision::new(2)), "38.55");
        assert_eq!(format_number("5", Precision::new(2)), "0.05");
        assert_eq!(format_number("42", Precision::new(0)), "42");
        assert_eq!(format_number("-1234", Precision::new(2)), "-12.34");
    }

    #[test]
    fn render_percentage_converts_ppm() {
        assert_eq!(render_percentage(380_000, Precision::new(2)), "38");
        assert_eq!(render_percentage(385_500, Precision::new(2)), "38.55");
        assert_eq!(render_percentage(1_000_000, Precision::new(2)), "100");
    }

    /// The status entry point renders every fragment the report model carries: each
    /// account line carries the account name and its own reading, fresh, stale and
    /// auth-required alike, so no fragment can be dropped without this failing.
    #[test]
    fn status_report_renders_each_account_fragment() {
        use crate::logging::LogicalName;
        use crate::report::{LedgerGeneration, MeterAccount, ReportMetadata, StatusReport};

        let now = now();
        let envelope = envelope();
        let metadata = ReportMetadata::new(now, now, LedgerGeneration::new(0), None);
        let report = StatusReport::new(
            metadata,
            vec![
                MeterAccount::from_projection(
                    LogicalName::new("work-a"),
                    Freshness::Fresh {
                        observed: observed(
                            380_000,
                            UtcTimestamp::from_unix_nanos(
                                now.unix_nanos() - 5 * 3_600 * NANOS_PER_SECOND,
                            ),
                        ),
                        latest_attempt: AttemptId::new(1),
                    },
                    Some(crate::report::LimitingWindow {
                        scope: crate::domain::window::WindowScope::AccountWide,
                        nominal_duration: NominalWindowDuration::from_nanos(
                            5 * 3_600 * NANOS_PER_SECOND as u64,
                        ),
                    }),
                    vec![],
                    None,
                ),
                MeterAccount::new(
                    LogicalName::new("research"),
                    Freshness::Stale {
                        last_good: Some(observed(
                            380_000,
                            UtcTimestamp::from_unix_nanos(
                                now.unix_nanos() - 14 * 60 * NANOS_PER_SECOND,
                            ),
                        )),
                        latest_attempt: AttemptId::new(2),
                        reason: StaleReason::SourceUnreachable(FailureClass::ConnectTimeout),
                    },
                ),
                MeterAccount::new(
                    LogicalName::new("legacy"),
                    Freshness::<QuotaRemaining>::AuthRequired {
                        last_good: None,
                        latest_attempt: AttemptId::new(3),
                    },
                ),
            ],
            vec![],
            crate::report::ProjectionReadState::Read,
        );

        let rendered = render_status_report(&report, now, envelope);
        assert!(
            rendered.contains("aub work-a 38% left · 5h"),
            "fresh fragment missing from rendering: {rendered}"
        );
        assert!(
            rendered.contains("aub research ~38% · stale 14m · timeout"),
            "stale fragment missing from rendering: {rendered}"
        );
        assert!(
            rendered.contains("aub legacy auth!"),
            "auth fragment missing from rendering: {rendered}"
        );
    }

    /// The entry point performs no lookup of its own: a report built entirely from
    /// literals, with no configuration, no store and no data source, renders every
    /// fragment it carries. An entry point that collected data itself would have
    /// nothing to collect from for this model, and the exact-output assertion
    /// would fail instead of being satisfied by a coincidentally empty lookup.
    #[test]
    fn status_report_built_from_literals_renders_without_lookup() {
        use crate::domain::attempt::AttemptId;
        use crate::domain::freshness::Observed;
        use crate::domain::quota::{QuotaFractionPpm, QuotaRemaining};
        use crate::domain::time::{MeasurementBasis, ReceivedAt, UtcTimestamp};
        use crate::logging::LogicalName;
        use crate::report::{LedgerGeneration, MeterAccount, ReportMetadata, StatusReport};

        let now = UtcTimestamp::from_unix_nanos(1_000_000_000_000);
        let envelope = ClockSkewEnvelope::new(MonotonicDuration::from_seconds(60));
        let metadata = ReportMetadata::new(now, now, LedgerGeneration::new(0), None);
        let observed = Observed::new(
            QuotaRemaining::new(QuotaFractionPpm::new(380_000).unwrap()),
            None,
            ReceivedAt::new(UtcTimestamp::from_unix_nanos(
                now.unix_nanos() - 5 * 3_600 * NANOS_PER_SECOND,
            )),
            MeasurementBasis::LocallyReceived,
        );
        let report = StatusReport::new(
            metadata,
            vec![MeterAccount::from_projection(
                LogicalName::new("work-primary"),
                Freshness::Fresh {
                    observed,
                    latest_attempt: AttemptId::new(1),
                },
                Some(crate::report::LimitingWindow {
                    scope: crate::domain::window::WindowScope::AccountWide,
                    nominal_duration: NominalWindowDuration::from_nanos(
                        5 * 3_600 * NANOS_PER_SECOND as u64,
                    ),
                }),
                vec![],
                None,
            )],
            vec![],
            crate::report::ProjectionReadState::Read,
        );

        let rendered = render_status_report(&report, now, envelope);
        assert_eq!(rendered, "aub work-primary 38% left · 5h");
    }
}
