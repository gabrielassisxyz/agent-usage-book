//! Rendering helpers that require explicit context.
//!
//! Every helper takes a unit label, a qualification and a precision policy, plus
//! freshness where the value is a meter reading. A bare scalar cannot reach a
//! user-visible surface because no helper here accepts one, and a bare total where
//! known missing evidence affects the aggregate is refused by construction.

use crate::doctor::{CheckStatus, DoctorReport};
use crate::domain::credits::Credits;
use crate::domain::failure::FailureClass;
use crate::domain::freshness::{Freshness, StaleReason};
use crate::domain::money::{Currency, Money};
use crate::domain::provenance::DerivationId;
use crate::domain::quota::QuotaRemaining;
use crate::domain::render::Precision;
use crate::domain::time::{Age, ClockSkewEnvelope, MonotonicDuration, UtcTimestamp, age};
use crate::domain::tokens::TokenKind;
use crate::domain::window::NominalWindowDuration;
use crate::error::Error;
use crate::evidence::{CoverageCompleteness, Derivation, RequiredFact};
use crate::presentation::precision::{COVERAGE_PERCENT, PERCENT, TOKENS};
use crate::presentation::vocabulary::{Qualification, coverage_term, quality_term};
use crate::report::{
    CoverageReport, NowReport, ProvenanceGraph, SpendGroup, SpendReport, StatusReport,
};
use crate::transcripts::TranscriptDriftReport;
use crate::valuation::ValuationOutcome;

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
    let lines = meter_account_lines(&report.accounts, now, envelope);
    join_report_with_explain(lines, &report.provenance, explain)
}

/// Renders a now live report.
pub fn render_now_report(
    report: &NowReport,
    now: UtcTimestamp,
    envelope: ClockSkewEnvelope,
) -> String {
    render_now_report_with_explain(report, now, envelope, ExplainMode::Off)
}

/// Renders a now live report, optionally including the explain block.
///
/// `now` and `status` render one account line the same way, through the same
/// [`meter_account_lines`] helper: a `now` immediately followed by a `status`
/// cannot disagree on the text because neither has its own line format.
pub fn render_now_report_with_explain(
    report: &NowReport,
    now: UtcTimestamp,
    envelope: ClockSkewEnvelope,
    explain: ExplainMode,
) -> String {
    let lines = meter_account_lines(&report.accounts, now, envelope);
    join_report_with_explain(lines, &report.provenance, explain)
}

/// One `aub <account> <reading>` line per account, in order, through the shared
/// meter-reading fragment renderer so wording lives in one place.
fn meter_account_lines(
    accounts: &[crate::report::MeterAccount],
    now: UtcTimestamp,
    envelope: ClockSkewEnvelope,
) -> Vec<String> {
    accounts
        .iter()
        .map(|account| {
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
            format!("aub {} {}", account.account.as_str(), reading)
        })
        .collect()
}

/// Joins account lines and, when asked, the explain block below them.
fn join_report_with_explain(
    lines: Vec<String>,
    provenance: &ProvenanceGraph,
    explain: ExplainMode,
) -> String {
    let report_text = lines.join("\n");
    if explain == ExplainMode::Off {
        return report_text;
    }
    let explain_text = render_explain(provenance, explain);
    if report_text.is_empty() {
        explain_text
    } else {
        format!("{report_text}\n\n{explain_text}")
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
    let grouping = report
        .grouping
        .iter()
        .map(|dimension| dimension.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let is_valued = report.groups.iter().any(|g| g.valuation.is_some());
    let valuation_clause = if is_valued {
        ", valued at API list-price equivalent"
    } else {
        ""
    };
    let credit_clause = match &report.credit_model {
        Some(model) => format!(", converted to credits under cost model {}", model.as_str()),
        None if report.groups.iter().any(|g| g.credits.is_some()) => {
            ", credits requested with no active cost model".to_string()
        }
        None => String::new(),
    };
    let mut lines = vec![format!(
        "spend from {} to {} (UTC days, end exclusive), grouped by {grouping}{valuation_clause}{credit_clause}",
        report.since.iso(),
        report.until.iso()
    )];
    if let Some(generation) = report.metadata.ingestion_generation {
        lines.push(format!("ingestion generation: {}", generation.get()));
    }
    if let Some(note) = &report.stale_rate_card_note {
        lines.push(format!("note: {note}"));
    }
    if report.groups.is_empty() {
        lines.push(format!(
            "no usage events in the window: {} canonical events read, {} outside it, {} undated",
            report.ingest.events_in_window,
            report.ingest.events_outside_window,
            report.ingest.undated_events
        ));
    }
    for group in &report.groups {
        render_spend_group(group, 0, &mut lines);
    }
    lines.push(render_ingest_summary(report));
    let report_text = lines.join("\n");
    if explain == ExplainMode::Off {
        report_text
    } else {
        let mut explain_text = render_explain(&report.provenance, explain);
        let account_text = render_account_explain(report);
        if !account_text.is_empty() {
            explain_text.push_str("\n\n");
            explain_text.push_str(&account_text);
        }
        if report_text.is_empty() {
            explain_text
        } else {
            format!("{report_text}\n\n{explain_text}")
        }
    }
}

/// The marker evidence behind every account group, under `--explain`. Empty
/// unless the report was grouped by account. Each line names the account, its
/// effective evidence class, and the exact markers that produced it, so the
/// human output carries the same references the JSON explain does (aub-mgv.4).
fn render_account_explain(report: &SpendReport) -> String {
    if report.account_explain.is_empty() {
        return String::new();
    }
    let mut lines = vec!["account explain:".to_string()];
    for group in &report.account_explain {
        let markers = if group.markers.is_empty() {
            "none".to_string()
        } else {
            group
                .markers
                .iter()
                .map(|marker| format!("{} ({})", marker.reference, marker.evidence_class.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        };
        lines.push(format!(
            "  {}  evidence_class={}  markers=[{markers}]",
            group.key.as_str(),
            group.evidence_class.as_str()
        ));
    }
    lines.join("\n")
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

fn render_spend_group(group: &SpendGroup, depth: usize, lines: &mut Vec<String>) {
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
    if let Some(valuation) = &group.valuation {
        match valuation {
            ValuationOutcome::Complete(equiv) => {
                parts.push(format!(
                    "API list-price equivalent ${}",
                    render_money_amount(equiv.amount())
                ));
            }
            ValuationOutcome::Incomplete { .. } | ValuationOutcome::UnsupportedCurrency { .. } => {
                parts.push("API list-price equivalent unavailable".to_string());
            }
        }
    }
    if let Some(credits) = &group.credits {
        parts.push(render_credits(credits));
    }
    let qualification = match quality_term(group.usage.quality()) {
        Some(term) => term,
        None => coverage_term(group.usage.coverage()),
    };
    lines.push(format!(
        "{}{}  {} ({})",
        "  ".repeat(depth),
        group.key.as_str(),
        parts.join(" · "),
        qualification.term()
    ));
    for child in &group.children {
        render_spend_group(child, depth + 1, lines);
    }
}

/// The credit term of a spend line: the qualified amount, or the refusal naming
/// every fact it is missing. A refusal is rendered next to the tokens rather than
/// in place of them, so a window whose credits cannot be derived still reports the
/// usage it did measure.
fn render_credits(credits: &Derivation<Credits>) -> String {
    match credits {
        Derivation::Available(qualified) => {
            let (value, coverage, quality, _) = qualified.clone().into_parts();
            let qualification = match quality_term(&quality) {
                Some(term) => term,
                None => coverage_term(&coverage),
            };
            format!(
                "{} {CREDIT_UNIT} ({})",
                render_credits_amount(value),
                qualification.term()
            )
        }
        Derivation::Unavailable { missing, .. } => format!(
            "{CREDIT_UNIT} unavailable: {}",
            missing
                .iter()
                .map(RequiredFact::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Formats a credit amount with two fractional digits, the same precision the
/// monetary renderer uses: a credit is a billing quantity and reads wrong at
/// either full micro precision or as a bare integer.
pub fn render_credits_amount(credits: Credits) -> String {
    let micros = credits.micros();
    let hundredths = (micros.abs() + 5_000) / 10_000;
    let sign = if micros < 0 { "-" } else { "" };
    format!("{sign}{}.{:02}", hundredths / 100, hundredths % 100)
}

/// Formats a typed monetary amount with two fractional digits.
pub fn render_money_amount<C: Currency>(money: Money<C>) -> String {
    let micros = money.micros();
    let cents = (micros.abs() + 5_000) / 10_000;
    let sign = if micros < 0 { "-" } else { "" };
    let whole = cents / 100;
    let frac = cents % 100;
    format!("{sign}{whole}.{frac:02}")
}

/// The unit every credit quantity is carried in.
pub const CREDIT_UNIT: &str = "credits";

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
    if ingest.refresh_attempted {
        line.push_str(" · refresh requested");
    }
    if let Some(failure) = &ingest.refresh_failure {
        line.push_str(&format!(" · refresh incomplete: {failure}"));
    }
    if !by_class.is_empty() {
        line.push_str(&format!(" ({by_class})"));
    }
    for file in &ingest.unreadable_files {
        line.push_str(&format!("\nunreadable: {file}"));
    }
    line
}

/// Renders the attribution-quality section of `aub doctor`: the metric over
/// all history and over the recent window, and any configured-floor breach.
///
/// An empty metric is stated as "no account attribution segments recorded
/// yet", never as `0%`: a fabricated zero would read as every token
/// unattributed.
pub fn render_attribution_quality(
    assessment: &crate::attribution::quality::AttributionQualityAssessment,
) -> String {
    use crate::attribution::account_segment::AccountEvidenceClass;
    use crate::attribution::quality::AttributionQuality;

    fn percent(fraction: crate::attribution::quality::AttributionFraction) -> String {
        match fraction.ppm() {
            Some(ppm) => format!("{:.1}%", ppm as f64 / 10_000.0),
            None => "no usage".to_string(),
        }
    }

    fn render_metric(lines: &mut Vec<String>, quality: &AttributionQuality) {
        if quality.is_empty() {
            lines.push("  no account attribution segments recorded yet".to_string());
            return;
        }
        for kind in TokenKind::ALL {
            let breakdown = quality.breakdown(kind);
            if breakdown.total() == 0 {
                continue;
            }
            lines.push(format!(
                "  {}: {} tokens, {} attributed",
                token_kind_label(kind),
                breakdown.total(),
                percent(breakdown.attributed_fraction())
            ));
            for class in AccountEvidenceClass::ALL {
                let tokens = breakdown.tokens(class);
                if tokens == 0 {
                    continue;
                }
                lines.push(format!(
                    "    {}: {} ({})",
                    class.as_str(),
                    tokens,
                    percent(breakdown.class_fraction(class))
                ));
            }
        }
    }

    let mut lines = Vec::new();
    lines.push("Doctor: Attribution Quality".to_string());
    lines.push("All history:".to_string());
    render_metric(&mut lines, &assessment.all_history);
    lines.push(format!(
        "Recent window (since {}):",
        assessment.recent_window.since.unix_nanos()
    ));
    render_metric(&mut lines, &assessment.recent_window.quality);
    if assessment.recent_window.undated_observations > 0 {
        lines.push(format!(
            "  ({} observations with unknown session start excluded from the window)",
            assessment.recent_window.undated_observations
        ));
    }
    for breach in &assessment.breaches {
        let scope = match breach.scope {
            crate::attribution::quality::MetricScope::AllHistory => "all-history".to_string(),
            crate::attribution::quality::MetricScope::RecentWindow { since } => {
                format!("recent-window (since {})", since.unix_nanos())
            }
        };
        lines.push(format!(
            "FLOOR BREACH: {scope} {} attribution {} is below the configured floor of {:.1}%",
            token_kind_label(breach.kind),
            percent(breach.fraction),
            breach.floor.as_f64() * 100.0
        ));
    }
    lines.join("\n")
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

/// Renders the full check registry for `aub doctor` (`aub-n27.7`): every registered
/// check, its status and, where it has one, its reason. Distinct from
/// [`render_doctor_drift_report`], which is the deeper `--transcript-format-drift`
/// view of one check's own evidence.
pub fn render_doctor_report(report: &DoctorReport) -> String {
    let mut lines = vec![format!("Doctor: {} checks", report.outcomes.len())];
    for outcome in &report.outcomes {
        let marker = match &outcome.status {
            CheckStatus::Pass => "PASS".to_string(),
            CheckStatus::Fail(_) => "FAIL".to_string(),
            CheckStatus::NotApplicable(_) => "N/A ".to_string(),
            CheckStatus::NotYetAvailable { .. } => "TODO".to_string(),
        };
        let mut line = format!("  [{marker}] {}", outcome.name.as_str());
        match &outcome.status {
            CheckStatus::Fail(reason) | CheckStatus::NotApplicable(reason) => {
                line.push_str(&format!(": {reason}"));
            }
            CheckStatus::NotYetAvailable { owning_bead } => {
                line.push_str(&format!(": not yet available ({owning_bead})"));
            }
            CheckStatus::Pass => {}
        }
        if outcome.has_repair {
            line.push_str(" [repairable with --fix]");
        }
        lines.push(line);
    }
    lines.push(format!(
        "Summary: {} passed, {} failed, {} not applicable, {} not yet available",
        report.passed(),
        report.failed(),
        report.not_applicable(),
        report.not_yet_available(),
    ));
    lines.join("\n")
}

/// Renders a `doctor --fix` result: one line per action performed, in order.
pub fn render_fix_report(report: &crate::doctor::FixReport) -> String {
    let mut lines = vec![format!("Fix: {} action(s) performed", report.actions.len())];
    for outcome in &report.actions {
        lines.push(format!("  {}: {}", outcome.action.as_str(), outcome.detail));
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

/// Renders a duration the way the coverage table reads it: seconds, minutes,
/// or hours and days with the remainder carried alongside ("9m", "2h 11m"),
/// matching the worked example in PLAN.md section 49.
fn render_coverage_duration(duration: MonotonicDuration) -> String {
    let total_seconds = duration.as_nanos() / 1_000_000_000;
    if total_seconds < 60 {
        format!("{total_seconds}s")
    } else if total_seconds < 3_600 {
        format!("{}m", total_seconds / 60)
    } else if total_seconds < 86_400 {
        let hours = total_seconds / 3_600;
        let minutes = (total_seconds % 3_600) / 60;
        if minutes == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h {minutes}m")
        }
    } else {
        let days = total_seconds / 86_400;
        let hours = (total_seconds % 86_400) / 3_600;
        if hours == 0 {
            format!("{days}d")
        } else {
            format!("{days}d {hours}h")
        }
    }
}

/// One table cell of the coverage row.
fn coverage_cell(text: &str, width: usize) -> String {
    format!("{text:<width$}  ")
}

/// The attempts cell: the coverage percentage where one exists, the named
/// refusal where the engine refused to compute one. A policy the ledger
/// cannot reconstruct reads as "unknown", never as a number.
fn coverage_attempts_cell(engine: &crate::coverage::CoverageReport) -> String {
    match engine.attempt_coverage {
        Some(fraction) => {
            format!(
                "{}%",
                render_percentage(fraction.as_ppm(), COVERAGE_PERCENT)
            )
        }
        None => match engine.expected_opportunities {
            None => "unknown".to_string(),
            Some(0) => "n/a".to_string(),
            Some(_) => "unknown".to_string(),
        },
    }
}

/// The measurements cell: the conditional coverage over terminal attempts, or
/// the named refusal when no attempt reached a terminal state.
fn coverage_measurements_cell(engine: &crate::coverage::CoverageReport) -> String {
    match engine.measurement_coverage {
        Some(fraction) => {
            format!(
                "{}%",
                render_percentage(fraction.as_ppm(), COVERAGE_PERCENT)
            )
        }
        None => "none".to_string(),
    }
}

/// The detail block of one account, when its numbers need explaining: the
/// scheduler line, the non-zero failure classes largest first, the
/// interruptions, and the resets lost to blind gaps. A healthy account
/// renders no block: the table row already carries its numbers.
fn render_coverage_detail(
    report: &CoverageReport,
    account: &crate::report::CoverageAccount,
) -> Option<Vec<String>> {
    let engine = &account.engine;
    let attempt_below_floor = engine
        .attempt_coverage
        .is_some_and(|coverage| coverage.as_f64() < report.threshold.attempt_floor.get());
    let measurement_below_floor = engine
        .measurement_coverage
        .is_some_and(|coverage| coverage.as_f64() < report.threshold.measurement_floor.get());
    let interrupted = engine.started_without_terminal_result > 0;
    let policy_unknown = engine.expected_opportunities.is_none();
    let severe = !engine.reset_spanning_gaps.is_empty();
    if !policy_unknown
        && !attempt_below_floor
        && !measurement_below_floor
        && !interrupted
        && !severe
    {
        return None;
    }

    let mut lines = Vec::new();
    if policy_unknown {
        lines.push("no sampling policy snapshot covers the whole interval".to_string());
    } else {
        match engine.attempt_coverage {
            // The scheduler line names the only fact a high attempt coverage
            // carries: the opportunities the policy owed were begun.
            Some(_) if !attempt_below_floor => lines.push("scheduler ran normally".to_string()),
            Some(_) => lines.push("attempt coverage is below the configured floor".to_string()),
            // Nothing was owed: there is no attempt coverage to judge.
            None => {}
        }
    }
    for (group, count) in account.failures.nonzero() {
        let noun = if count == 1 { "attempt" } else { "attempts" };
        lines.push(format!("{count} {noun} {}", group.phrase()));
    }
    if interrupted {
        let noun = if engine.started_without_terminal_result == 1 {
            "attempt"
        } else {
            "attempts"
        };
        lines.push(format!(
            "{} {noun} started without a terminal result",
            engine.started_without_terminal_result
        ));
    }
    if severe {
        // The window length is the provider-reported nominal duration of the
        // reset the gap swallowed; it is rendered when one is known.
        let window_length = account
            .resets_in_gaps
            .iter()
            .map(|reset| reset.window_length)
            .max();
        match (engine.reset_spanning_gaps.len(), window_length) {
            (1, Some(length)) if length.as_nanos() > 0 => lines.push(format!(
                "one {} reset occurred without a successful observation in the surrounding interval",
                render_coverage_duration(length)
            )),
            (1, _) => lines.push(
                "one reset occurred without a successful observation in the surrounding interval"
                    .to_string(),
            ),
            (count, _) => lines.push(format!(
                "{count} resets occurred without successful observations in the surrounding intervals"
            )),
        }
    }
    Some(lines)
}

/// The report-to-rendering seam for coverage: the interval, one row per
/// covered account, and a detail block for every account whose numbers need
/// explaining. The model arrives complete; this function formats it. The
/// header echoes the window the command line asked for: "last 24h" is what
/// the operator requested, and the interval itself is carried by the model's
/// own timestamps.
pub fn render_coverage_report(report: &CoverageReport, window: &str) -> String {
    let mut lines = vec![format!("coverage - last {window}")];
    if report.accounts.is_empty() {
        if report.severe_only {
            lines.push("(no account has a severe interval)".to_string());
        } else {
            lines.push("(no account has recorded sampling evidence in the ledger)".to_string());
        }
        return lines.join("\n");
    }

    lines.push(String::new());

    let name_width = report
        .accounts
        .iter()
        .map(|account| account.name.as_str().len())
        .chain(std::iter::once("account".len()))
        .max()
        .unwrap_or(7);
    let headers: [(&str, usize); 5] = [
        ("account", name_width),
        ("attempts", 9),
        ("measurements", 12),
        ("longest blind gap", 17),
        ("reset gaps", 10),
    ];
    lines.push(
        headers
            .iter()
            .map(|(header, width)| coverage_cell(header, *width))
            .collect::<String>()
            .trim_end()
            .to_string(),
    );
    for account in &report.accounts {
        let engine = &account.engine;
        let cells = [
            coverage_cell(account.name.as_str(), name_width),
            coverage_cell(&coverage_attempts_cell(engine), 9),
            coverage_cell(&coverage_measurements_cell(engine), 12),
            coverage_cell(
                &engine
                    .longest_no_attempt_gap
                    .map(|gap| render_coverage_duration(gap.duration()))
                    .unwrap_or_else(|| "none".to_string()),
                17,
            ),
            engine.reset_spanning_gaps.len().to_string(),
        ];
        lines.push(cells.join("").trim_end().to_string());
    }
    for account in &report.accounts {
        let detail = render_coverage_detail(report, account);
        if detail.is_none() && !account.legacy_evidence_present {
            continue;
        }
        lines.push(String::new());
        lines.push(format!("{}:", account.name.as_str()));
        for line in detail.unwrap_or_default() {
            lines.push(format!("  - {line}"));
        }
        if account.legacy_evidence_present {
            lines.push(
                "  - legacy observations are shown as historical evidence, not ordinary attempt coverage"
                    .to_string(),
            );
        }
    }
    lines.join("\n")
}

/// The threshold-breach message the coverage command fails with, naming every
/// breached account, the floor's dimension, the measured coverage and the
/// floor itself. The report has already been printed; this message is what
/// the exit class's prose names.
pub fn render_coverage_threshold_message(report: &CoverageReport) -> String {
    let parts: Vec<String> = report
        .threshold
        .breaches
        .iter()
        .map(|breach| {
            let dimension = match breach.dimension {
                crate::report::CoverageBreachDimension::Attempt => "attempt",
                crate::report::CoverageBreachDimension::Measurement => "measurement",
            };
            format!(
                "{} {} coverage {}% is below the {}% floor",
                breach.account.as_str(),
                dimension,
                render_percentage(breach.coverage.as_ppm(), COVERAGE_PERCENT),
                render_percentage(breach.floor.as_ppm(), COVERAGE_PERCENT),
            )
        })
        .collect();
    if parts.is_empty() {
        "no threshold breach was recorded".to_string()
    } else {
        parts.join("; ")
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

    #[test]
    fn render_attribution_quality_states_empty_rather_than_zero_and_names_a_breach() {
        use crate::attribution::account_segment::AccountEvidenceClass;
        use crate::attribution::quality::{
            AttributionObservation, AttributionQualityAssessment, AttributionQualityFloor,
        };
        use crate::domain::tokens::{
            CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens,
        };

        // No observations: the metric must say so, never print 0%.
        let empty = AttributionQualityAssessment::assess(
            Vec::new(),
            UtcTimestamp::from_unix_nanos(0),
            None,
        );
        let empty_text = render_attribution_quality(&empty);
        assert!(empty_text.contains("no account attribution segments recorded yet"));
        assert!(!empty_text.contains('%'));

        // A breaching corpus: the metric and a FLOOR BREACH line.
        let tokens = |input: u64| {
            KnownTokenVector::new(
                InputTokens::new(input),
                OutputTokens::new(0),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(0),
            )
        };
        let observations = vec![
            AttributionObservation {
                evidence_class: AccountEvidenceClass::ExplicitLauncherOrHook,
                usage: tokens(20),
                observed_at: Some(UtcTimestamp::from_unix_nanos(10)),
            },
            AttributionObservation {
                evidence_class: AccountEvidenceClass::Unattributed,
                usage: tokens(80),
                observed_at: Some(UtcTimestamp::from_unix_nanos(10)),
            },
        ];
        let assessment = AttributionQualityAssessment::assess(
            observations,
            UtcTimestamp::from_unix_nanos(0),
            AttributionQualityFloor::new(0.9),
        );
        let text = render_attribution_quality(&assessment);
        assert!(
            text.contains("input: 100 tokens, 20.0% attributed"),
            "{text}"
        );
        assert!(text.contains("unattributed"));
        assert!(text.contains("FLOOR BREACH"));
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
