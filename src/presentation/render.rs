//! Rendering helpers that require explicit context.
//!
//! Every helper takes a unit label, a qualification and a precision policy, plus
//! freshness where the value is a meter reading. A bare scalar cannot reach a
//! user-visible surface because no helper here accepts one, and a bare total where
//! known missing evidence affects the aggregate is refused by construction.

use crate::attribution::TaskIdentityState;
use crate::doctor::{CheckStatus, DoctorReport};
use crate::domain::credits::Credits;
use crate::domain::failure::FailureClass;
use crate::domain::freshness::{Freshness, StaleReason};
use crate::domain::money::{Currency, Money};
use crate::domain::provenance::DerivationId;
use crate::domain::quota::{PercentagePoints, QuotaRemaining};
use crate::domain::render::Precision;
use crate::domain::time::{Age, ClockSkewEnvelope, MonotonicDuration, UtcTimestamp, age};
use crate::domain::tokens::{TokenKind, UsageVector};
use crate::domain::window::NominalWindowDuration;
use crate::error::Error;
use crate::evidence::{CoverageCompleteness, Derivation, RequiredFact};
use crate::presentation::boxed::{
    boxed_blank, boxed_body, boxed_bottom, boxed_content_area, boxed_rule, boxed_top, boxed_width,
};
use crate::presentation::precision::{COVERAGE_PERCENT, PERCENT, TOKENS};
use crate::presentation::style::Style;
use crate::presentation::vocabulary::{Qualification, coverage_term, quality_term};
use crate::report::{
    ActiveActivityState, CoverageReport, LivenessGap, NowReport, ProvenanceGraph, SpendGroup,
    SpendReport, StatusReport, TaskOverheadReport, TaskReport, WindowEquivalentDerivation,
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
    style: Style,
) -> String {
    render_status_report_with_explain(report, now, envelope, ExplainMode::Off, style)
}

/// Renders a status report, optionally including the explain block.
pub fn render_status_report_with_explain(
    report: &StatusReport,
    now: UtcTimestamp,
    envelope: ClockSkewEnvelope,
    explain: ExplainMode,
    style: Style,
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
    let grid = render_status_grid(&report.accounts, now, envelope, style);
    let mut rendered = if explain == ExplainMode::Off {
        grid
    } else {
        let explain_text = render_explain(&report.provenance, explain);
        if grid.is_empty() {
            explain_text
        } else {
            format!("{grid}\n\n{explain_text}")
        }
    };
    if explain != ExplainMode::Off {
        let meter_explain = render_meter_explain(&report.accounts, explain);
        if !meter_explain.is_empty() {
            if !rendered.is_empty() {
                rendered.push_str("\n\n");
            }
            rendered.push_str(&meter_explain);
        }
    }
    rendered
}

/// The fixed column widths of one grouped-grid window row, in characters. The
/// row is `<indent><label><sp><bar><sp><percent><sp><rate><sp><reset+note>`;
/// only the trailing reset column is elastic.
const STATUS_ROW_INDENT: usize = 4;
const STATUS_LABEL_WIDTH: usize = 8;
const STATUS_BAR_CELLS: usize = 30;
const STATUS_PERCENT_WIDTH: usize = 5;
const STATUS_RATE_WIDTH: usize = 7;

/// The used-fraction cut points the grid tones a row by, in whole percent used.
/// The same boundaries `bin/quota-bars` applies, so the two tools agree while
/// both exist: nothing used reads idle, at or above 85 reads red, at or above
/// 60 reads yellow, everything between reads green.
const STATUS_TONE_RED_AT_PERCENT: u32 = 85;
const STATUS_TONE_YELLOW_AT_PERCENT: u32 = 60;

/// Renders the grouped grid: a `QUOTA` header with the local timestamp
/// right-aligned to the terminal width, then one block per provider (accounts
/// in configured order, grouped), each account a name-and-plan line over one
/// row per quota window. Two blank lines separate providers, one separates
/// accounts. Empty only when there are no accounts to show.
fn render_status_grid(
    accounts: &[crate::report::MeterAccount],
    now: UtcTimestamp,
    envelope: ClockSkewEnvelope,
    style: Style,
) -> String {
    let width = style.width() as usize;

    // One percent column for the whole grid, sized to the widest window text
    // it holds (aub-v8wt): every window renders at its own reported
    // resolution, so the column is the maximum over the rows, never a global
    // decimal count imposed on windows that did not report it.
    let percent_width = status_percent_width(accounts);

    // Group accounts by provider, keeping first-seen (configured) order.
    let mut groups: Vec<(&str, Vec<String>)> = Vec::new();
    for account in accounts {
        let provider = account.provider.as_deref().unwrap_or("");
        let body = status_account_body(account, now, envelope, style, percent_width);
        match groups.last_mut() {
            Some((seen, bodies)) if *seen == provider => bodies.push(body),
            _ => groups.push((provider, vec![body])),
        }
    }

    let provider_blocks = groups.into_iter().map(|(provider, bodies)| {
        // One account line over its rows; a blank line between accounts.
        format!(
            "{}\n{}",
            status_provider_rule(provider, style, width),
            bodies.join("\n\n"),
        )
    });

    // The header joins the first provider with one blank line; providers join
    // each other with two.
    let mut out = status_header_line(now, style, width);
    for (index, block) in provider_blocks.enumerate() {
        out.push_str(if index == 0 { "\n\n" } else { "\n\n\n" });
        out.push_str(&block);
    }
    out
}

/// `QUOTA` at the left, the local wall-clock stamp flush against the right edge
/// so its last character lands on the terminal's last column.
fn status_header_line(now: UtcTimestamp, style: Style, width: usize) -> String {
    let stamp = crate::presentation::local_time::LocalWallClock::of(now)
        .map(|local| local.header_stamp())
        .unwrap_or_default();
    let gap = width.saturating_sub("QUOTA".len() + stamp.len()).max(1);
    format!(
        "{}{}{}",
        style.paint(style.text(), "QUOTA"),
        " ".repeat(gap),
        style.paint(style.dim(), &stamp),
    )
}

/// The provider name in the accent colour, then a rule to the right edge.
fn status_provider_rule(provider: &str, style: Style, width: usize) -> String {
    let rule = "\u{2500}".repeat(width.saturating_sub(provider.len() + 1).max(1));
    format!(
        "{} {}",
        style.paint(style.accent(), provider),
        style.paint(style.track(), &rule),
    )
}

/// One account: the name-and-plan line, then its window rows (or the freshness
/// answer when the account has no window to show).
fn status_account_body(
    account: &crate::report::MeterAccount,
    now: UtcTimestamp,
    envelope: ClockSkewEnvelope,
    style: Style,
    percent_width: usize,
) -> String {
    use crate::domain::freshness::FreshnessKind;
    let kind = account.reading.kind();
    let dim_block = kind == FreshnessKind::Stale;
    let plan = account.provider.as_deref().unwrap_or("").to_string();
    let header = format!(
        "  {}  {}",
        style.paint(style.bold(), account.account.as_str()),
        style.paint(style.dim(), &plan),
    );

    // An auth-required account cannot have its windows trusted: the design
    // shows `auth!` in place of the grid, never a row per window.
    let rows = if kind == FreshnessKind::AuthRequired {
        Vec::new()
    } else {
        status_window_rows(account, now, envelope, style, dim_block, percent_width)
    };
    if rows.is_empty() {
        let answer = if kind == FreshnessKind::AuthRequired {
            style.paint(style.red(), "auth!")
        } else {
            style.paint(
                style.dim(),
                &render_meter_reading(&account.reading, METER_UNIT, PERCENT, now, envelope, None),
            )
        };
        return format!("{header}\n{}{answer}", " ".repeat(STATUS_ROW_INDENT));
    }
    let mut out = header;
    for row in rows {
        out.push('\n');
        out.push_str(&row);
    }
    out
}

/// The window rows of one account, ordered 5-hour window, then the weekly
/// account-wide window, then model-scoped windows by model display name.
/// Provider quota groups (`aub-n8yx`) are not rows of that flat list: each
/// group renders as its own sub-block, one dim group-name line over the
/// group's windows in nominal-length order, beneath the account's plain
/// rows.
fn status_window_rows(
    account: &crate::report::MeterAccount,
    now: UtcTimestamp,
    envelope: ClockSkewEnvelope,
    style: Style,
    dim_block: bool,
    percent_width: usize,
) -> Vec<String> {
    // A `--model` selector narrows the grid to the account-wide windows and
    // the chosen model's own, the same set the reading was computed over;
    // a quota group constrains a set of models the window identity cannot
    // name, so its sub-block joins the grid only without a selector.
    let selected = account.selected_model.as_ref().map(|model| model.as_str());
    let mut plain: Vec<&crate::report::StatusWindow> = Vec::new();
    let mut groups: Vec<(
        &crate::domain::window::GroupName,
        Vec<&crate::report::StatusWindow>,
    )> = Vec::new();
    for window in &account.windows {
        if let Some(group) = window.scope.group_name() {
            if selected.is_none() {
                match groups.last_mut() {
                    Some((seen, windows)) if *seen == group => windows.push(window),
                    _ => groups.push((group, vec![window])),
                }
            }
        } else if match (selected, window.scope.scoped_model()) {
            (None, _) | (_, None) => true,
            (Some(selected), Some(model)) => model.as_str() == selected,
        } {
            plain.push(window);
        }
    }
    plain.sort_by_key(|window| status_window_order(window));

    let cached_note = if dim_block {
        account
            .windows
            .first()
            .and_then(|window| status_cached_note(window, now, envelope))
    } else {
        None
    };

    let mut rows: Vec<String> = plain
        .into_iter()
        .map(|window| {
            status_window_row(
                window,
                style,
                dim_block,
                cached_note.as_deref(),
                percent_width,
            )
        })
        .collect();
    for (group, windows) in &groups {
        rows.push(status_group_line(group, style, dim_block));
        let mut ordered = windows.clone();
        ordered.sort_by_key(|window| window.nominal_duration.as_nanos());
        for window in ordered {
            rows.push(status_window_row(
                window,
                style,
                dim_block,
                cached_note.as_deref(),
                percent_width,
            ));
        }
    }
    rows
}

/// The dim group-name line a quota-group sub-block starts with, at the same
/// indent a window row carries so the block stays aligned.
fn status_group_line(
    group: &crate::domain::window::GroupName,
    style: Style,
    dim_block: bool,
) -> String {
    let name_style = if dim_block {
        style.dimmer()
    } else {
        style.dim()
    };
    format!(
        "{indent}{name}",
        indent = " ".repeat(STATUS_ROW_INDENT),
        name = style.paint(name_style, group.as_str())
    )
}

/// The sort key placing account-wide windows first (shortest nominal length
/// first, so the 5-hour window precedes the weekly one) and model-scoped
/// windows after, ordered by model display name.
fn status_window_order(window: &crate::report::StatusWindow) -> (u8, u64, String) {
    match window.scope.scoped_model() {
        None => (0, window.nominal_duration.as_nanos(), String::new()),
        Some(model) => (1, 0, model.as_str().to_string()),
    }
}

/// One grid row: label, bar, percent used, burn rate, reset and any note.
fn status_window_row(
    window: &crate::report::StatusWindow,
    style: Style,
    dim_block: bool,
    cached_note: Option<&str>,
    percent_width: usize,
) -> String {
    let used_ppm = window.quota_used.as_ppm().get();
    let tone = status_used_tone(style, used_ppm, window);
    let label = status_window_label(window);

    let bar = status_bar(window, style, tone);
    let percent_text = status_used_percent_text(window);
    let percent = style.paint(
        &format!("{}{}", tone, style.bold()),
        &format!("{percent_text:>width$}", width = percent_width),
    );
    let rate_text = window
        .rate
        .map_or_else(|| "\u{2014}".to_string(), |rate| rate.to_string());
    let rate = style.paint(
        style.body(),
        &format!("{rate_text:>width$}", width = STATUS_RATE_WIDTH),
    );

    let mut trailing = status_reset_label(window);
    if let Some(instant) = window.capped_at
        && let Some(local) = crate::presentation::local_time::LocalWallClock::of(instant)
    {
        trailing.push_str(&format!("  capped {}", local.clock_label()));
    }
    if let Some(note) = cached_note {
        trailing.push_str("  ");
        trailing.push_str(note);
    }
    let trailing = if dim_block {
        style.paint(style.dimmer(), &trailing)
    } else {
        style.paint(style.dim(), &trailing)
    };

    format!(
        "{indent}{label} {bar} {percent} {rate} {trailing}",
        indent = " ".repeat(STATUS_ROW_INDENT),
        label = style.paint(
            style.muted(),
            &format!("{label:<width$}", width = STATUS_LABEL_WIDTH)
        ),
    )
}

/// The window label, clamped to the 8-cell label column so the grid stays
/// rigid: a model display name for a model-scoped window (truncated with an
/// ellipsis when it does not fit), `week` for the 7-day account-wide window,
/// `month` for one of 28 to 31 days, the nominal length otherwise.
fn status_window_label(window: &crate::report::StatusWindow) -> String {
    if let Some(model) = window.scope.scoped_model() {
        return clamp_label(model.as_str());
    }
    let whole_days = window.nominal_duration.as_nanos() / (86_400 * 1_000_000_000);
    match whole_days {
        7 => "week".to_string(),
        28..=31 => "month".to_string(),
        _ => render_window_duration(window.nominal_duration),
    }
}

/// A label clamped to the label column: unchanged when it fits, its first seven
/// characters and a trailing ellipsis when it does not.
fn clamp_label(label: &str) -> String {
    if label.chars().count() <= STATUS_LABEL_WIDTH {
        return label.to_string();
    }
    let head: String = label.chars().take(STATUS_LABEL_WIDTH - 1).collect();
    format!("{head}\u{2026}")
}

/// The percent text of one grid row, at the resolution its provider reported
/// (aub-v8wt): the decimals are the window's own, from
/// [`status_percent_decimals`], never a grid-wide policy, and trailing zeros
/// are trimmed by [`render_percentage`], so a whole-percent provider never
/// prints `53.0%` because another provider reports tenths. A quantization
/// other than `Exact` prefixes the approximate marker: the provider asserted
/// an interval, not a scalar, and the bare number would claim a precision it
/// did not report.
fn status_used_percent_text(window: &crate::report::StatusWindow) -> String {
    let decimals = status_percent_decimals(window.reported_resolution_ppm.as_ppm().get());
    let digits = render_percentage(window.quota_used.as_ppm().get(), Precision::new(decimals));
    let text = format!("{digits}%");
    if window.quantization == crate::domain::window::QuantizationSemantics::Exact {
        text
    } else {
        format!("~{text}")
    }
}

/// How many fractional percent digits a reported resolution earns: the fewest
/// that keep two readings the provider distinguishes distinct. 10_000 ppm is
/// whole percent (0), 1_000 ppm is tenths (1), 100 ppm is hundredths (2), down
/// to 1 ppm (4). Pure integer arithmetic: no float crosses this boundary.
fn status_percent_decimals(resolution_ppm: u32) -> u8 {
    for decimals in 0..=4u8 {
        let scale = 10u32.pow(u32::from(decimals));
        if resolution_ppm.saturating_mul(scale) % 10_000 == 0 {
            return decimals;
        }
    }
    4
}

/// The percent-column width of one grid: the widest window text it holds,
/// floored at [`STATUS_PERCENT_WIDTH`] so a whole-percent grid keeps its
/// historic five.
fn status_percent_width(accounts: &[crate::report::MeterAccount]) -> usize {
    accounts
        .iter()
        .flat_map(|account| account.windows.iter())
        .map(|window| status_used_percent_text(window).chars().count())
        .max()
        .unwrap_or(0)
        .max(STATUS_PERCENT_WIDTH)
}

/// The tone of a row by quota used: idle when nothing is used, red at or
/// above 85, yellow at or above 60, green otherwise. An auth-required or
/// not-started window has no fraction to tone and reads idle. Compared in
/// parts per million, not in rounded percent, so an 84.96% window does not
/// read red because its text rounds up.
fn status_used_tone(
    style: Style,
    used_ppm: u32,
    window: &crate::report::StatusWindow,
) -> &'static str {
    if window.reset_state.is_not_started() || used_ppm == 0 {
        style.idle()
    } else if used_ppm >= STATUS_TONE_RED_AT_PERCENT * 10_000 {
        style.red()
    } else if used_ppm >= STATUS_TONE_YELLOW_AT_PERCENT * 10_000 {
        style.yellow()
    } else {
        style.green()
    }
}

/// The 30-cell bar: `\u{2500}` track under `\u{2501}` fill, the fill count
/// `round(used_ppm * 30 / 1_000_000)`, the fill in the row's tone and the
/// track in the track colour.
fn status_bar(window: &crate::report::StatusWindow, style: Style, tone: &str) -> String {
    let ppm = u64::from(window.quota_used.as_ppm().get());
    let filled = ((ppm * STATUS_BAR_CELLS as u64 + 500_000) / 1_000_000)
        .min(STATUS_BAR_CELLS as u64) as usize;
    let track = STATUS_BAR_CELLS - filled;
    format!(
        "{}{}",
        style.paint(tone, &"\u{2501}".repeat(filled)),
        style.paint(style.track(), &"\u{2500}".repeat(track)),
    )
}

/// The reset instant as a local weekday and `HH:MM`, or `not started` for a
/// window that has not begun its cycle.
fn status_reset_label(window: &crate::report::StatusWindow) -> String {
    match window.reset_state.instant() {
        None => "not started".to_string(),
        Some(instant) => crate::presentation::local_time::LocalWallClock::of(instant)
            .map(|local| local.clock_label())
            .unwrap_or_else(|| format!("resets {}", format_time_hh_mm(instant))),
    }
}

/// `cached <age> ago · <reason>` for a stale block: the age of the observation
/// the window was read from, then why the reader would not call it fresh. The
/// reason is dropped only when the observation is not itself stale.
fn status_cached_note(
    window: &crate::report::StatusWindow,
    now: UtcTimestamp,
    envelope: ClockSkewEnvelope,
) -> Option<String> {
    let (observed, reason) = match &window.observation {
        Freshness::Stale {
            last_good, reason, ..
        } => (last_good.as_ref(), Some(render_stale_reason(*reason))),
        Freshness::Fresh { observed, .. } => (Some(observed), None),
        Freshness::AuthRequired { last_good, .. } => (last_good.as_ref(), None),
    };
    let age = observed_age(observed?, now, envelope)?;
    let mut note = format!("cached {} ago", render_status_compound_age(age));
    if let Some(reason) = reason {
        note.push_str(" · ");
        note.push_str(reason);
    }
    Some(note)
}

/// An age as the grid reads it: the two most significant units where the value
/// spans days or hours (`2d 22h`, `3h 4m`), one unit otherwise (`40m`, `12s`).
fn render_status_compound_age(age: Age) -> String {
    let total_seconds = age.as_nanos() / 1_000_000_000;
    if total_seconds < 60 {
        format!("{total_seconds}s")
    } else if total_seconds < 3_600 {
        format!("{}m", total_seconds / 60)
    } else if total_seconds < 86_400 {
        let (hours, minutes) = (total_seconds / 3_600, (total_seconds % 3_600) / 60);
        if minutes == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h {minutes}m")
        }
    } else {
        let (days, hours) = (total_seconds / 86_400, (total_seconds % 86_400) / 3_600);
        if hours == 0 {
            format!("{days}d")
        } else {
            format!("{days}d {hours}h")
        }
    }
}

/// Renders provider contract and raw window facts retained by the meter
/// projection. The ordinary status value remains derived from all applicable
/// windows; these lines make the provider's inputs auditable without changing
/// that selection rule.
fn render_meter_explain(accounts: &[crate::report::MeterAccount], explain: ExplainMode) -> String {
    let mut lines = Vec::new();
    for account in accounts {
        let Some(explanation) = &account.meter_explanation else {
            continue;
        };
        lines.push(format!("meter account: {}", account.account.as_str()));
        lines.push(format!(
            "  provider contract: {}",
            explanation.provider_contract_id.as_str()
        ));
        for window in &explanation.windows {
            let mut line = format!(
                "  window {}: is_active={}, severity={}",
                window.semantic_key,
                window.is_active,
                window.severity.as_str()
            );
            // Every window carries its own live rate under `--explain=full`;
            // the summary mode keeps the provider facts alone.
            if explain == ExplainMode::Full {
                let rate = window
                    .rate
                    .map_or_else(|| "none".to_string(), |rate| rate.to_string());
                line.push_str(&format!(", burn rate={rate}"));
            }
            lines.push(line);
        }
        // `--explain=full` names the observation instants the limiting
        // window's burn rate was derived from: the current observation
        // always, and the freeze observation when the window has capped.
        if explain == ExplainMode::Full
            && let Some(burn) = &account.burn_rate
        {
            let rate = burn
                .rate
                .map_or_else(|| "none".to_string(), |rate| rate.to_string());
            lines.push(format!(
                "  burn rate: {rate}, from observation received_at={}",
                burn.derived_from.unix_nanos()
            ));
            if let Some(capped_at) = burn.capped_at {
                lines.push(format!(
                    "  burn rate frozen at cap, observed received_at={}",
                    capped_at.unix_nanos()
                ));
            }
        }
    }
    if lines.is_empty() {
        String::new()
    } else {
        let mut rendered = vec!["meter explain:".to_string()];
        rendered.extend(lines.into_iter().map(|line| format!("  {line}")));
        rendered.join("\n")
    }
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
    // The now command keeps today's rendering byte for byte in every mode: its
    // dispatch is another change's edit surface, so it passes the plain style
    // until its own change threads a measured one through.
    let mut lines = meter_account_lines(&report.accounts, now, envelope, Style::plain());
    if let Some(activity_line) = render_activity_line(&report.activity) {
        lines.push(activity_line);
    }
    join_report_with_explain(lines, &report.provenance, explain)
}

/// The one line naming `aub-mgv.5`'s composed activity state, or `None` when the
/// report evaluated no session at all (no `--session-id` was given). A bare `aub
/// now` therefore reads identically to `aub status`, a contract this bead does
/// not touch; the line appears only once something was actually evaluated.
fn render_activity_line(activity: &ActiveActivityState) -> Option<String> {
    match activity {
        ActiveActivityState::NoEvidence => None,
        ActiveActivityState::ExplicitMarkerEvidence(claim) => Some(format!(
            "aub session: spending account={} marker={} heartbeat={}",
            claim.logical_account, claim.marker_reference, claim.heartbeat_reference
        )),
        ActiveActivityState::ConflictingEvidence(logical_accounts) => Some(format!(
            "aub session: conflicting_evidence accounts=[{}]",
            logical_accounts.join(", ")
        )),
        ActiveActivityState::Inactive(claim) => {
            let liveness = match &claim.liveness_gap {
                LivenessGap::NeverObserved => "never_observed".to_string(),
                LivenessGap::Aged {
                    last_heartbeat_at, ..
                } => format!("aged last_heartbeat={}", last_heartbeat_at.unix_nanos()),
            };
            Some(format!(
                "aub session: inactive account={} marker={} liveness={liveness}",
                claim.logical_account, claim.marker_reference
            ))
        }
    }
}

/// One `aub <account> <reading>` line per account, in order, through the shared
/// meter-reading fragment renderer so wording lives in one place.
fn meter_account_lines(
    accounts: &[crate::report::MeterAccount],
    now: UtcTimestamp,
    envelope: ClockSkewEnvelope,
    style: Style,
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
                    .map(LimitingWindowDisplay::from),
            );
            // A fresh reading carries a remaining fraction, so its tone is the
            // account's state at a glance. Stale and auth-required readings
            // have no fraction to tone and keep their text as the whole
            // answer; the words never change either way, because freshness is
            // conveyed in text and never by colour alone.
            let reading = match &account.reading {
                Freshness::Fresh { observed, .. } => {
                    style.paint(style.tone(observed.value().as_ppm()), &reading)
                }
                // Two arms rather than one alternation: boundary rule 10 reads
                // a `Freshness::X { .. }` that is not directly followed by
                // `=>` as a construction, and the first half of an
                // alternation is followed by `|`.
                Freshness::Stale { .. } => reading,
                Freshness::AuthRequired { .. } => reading,
            };
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

/// Renders an interval reconciliation outcome for human display (aub-dpn.1).
///
/// The output term is "unexplained residual" on every human surface.
pub fn render_reconciliation(outcome: &crate::reconciliation::ReconciliationOutcome) -> String {
    crate::reconciliation::render_reconciliation_human(outcome)
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
    let window_equivalent_clause = report
        .window_equivalent_window
        .as_deref()
        .map(|window| format!(", converted to window-equivalent percentage points for {window}"))
        .unwrap_or_default();
    let mut lines = vec![format!(
        "spend from {} to {} (UTC days, end exclusive), grouped by {grouping}{valuation_clause}{credit_clause}{window_equivalent_clause}",
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
    if let Some(window_equivalent) = &group.window_equivalent {
        parts.push(render_window_equivalent(window_equivalent));
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

fn render_window_equivalent(result: &WindowEquivalentDerivation) -> String {
    match result {
        WindowEquivalentDerivation::Available(value) => {
            let qualification = match quality_term(&value.quality) {
                Some(term) => term,
                None => coverage_term(&value.coverage),
            };
            format!(
                "window equivalent [{}, {}] percentage points ({}; calibration {})",
                render_percentage_points(value.interval.lower()),
                render_percentage_points(value.interval.upper()),
                qualification.term(),
                value.calibration_id.as_str(),
            )
        }
        WindowEquivalentDerivation::Unavailable { missing, .. } => format!(
            "window equivalent unavailable: {}",
            missing
                .iter()
                .map(RequiredFact::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn render_percentage_points(points: PercentagePoints) -> String {
    let raw = i64::from(points.get());
    let absolute = raw.unsigned_abs();
    let sign = if raw < 0 { "-" } else { "" };
    format!("{sign}{}.{:04}", absolute / 10_000, absolute % 10_000)
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

/// Renders one usage vector's token line: the four known kinds, any unknown
/// component, and the coverage-or-quality qualification term, exactly the
/// fragment `render_spend_group` builds for one spend group.
fn render_usage_line(usage: &UsageVector) -> String {
    let known = usage.known();
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
    for (name, count) in usage.unknown() {
        parts.push(format!("{name} {}", render_count(count.value())));
    }
    let qualification = match quality_term(usage.quality()) {
        Some(term) => term,
        None => coverage_term(usage.coverage()),
    };
    format!("{} ({})", parts.join(" · "), qualification.term())
}

/// Renders a task report for `aub task report TASK-ID`.
pub fn render_task_report(report: &TaskReport) -> String {
    render_task_report_with_explain(report, ExplainMode::Off)
}

/// Renders a task report, optionally including the explain block.
pub fn render_task_report_with_explain(report: &TaskReport, explain: ExplainMode) -> String {
    let mut lines = vec![format!("task {}", report.task_id.as_str())];
    let kind_line = match &report.task_kind {
        None => "task kind: no tracker evidence".to_string(),
        Some(row) => {
            let state = match row.state {
                TaskIdentityState::Resolved => "resolved",
                TaskIdentityState::Unknown => "unknown",
                TaskIdentityState::Conflict => "conflict",
            };
            let kind = row.kind.map(|kind| kind.as_str()).unwrap_or("none");
            format!(
                "task kind: {state} ({kind}) · normalization v{} · evidence: {}",
                row.normalization_version, row.evidence
            )
        }
    };
    lines.push(kind_line);
    lines.push(format!("usage: {}", render_usage_line(&report.usage)));
    lines.push(format!("credits: {}", render_credits(&report.credits)));
    if report.sessions.is_empty() {
        lines.push("sessions: none".to_string());
    } else {
        lines.push("sessions:".to_string());
        for session in &report.sessions {
            let run_clause = match &session.run {
                Some(run) => format!(" run={}", run.as_str()),
                None => String::new(),
            };
            lines.push(format!(
                "  {}{run_clause}  {}",
                session.session.as_str(),
                render_usage_line(&session.usage)
            ));
        }
    }
    let report_text = lines.join("\n");
    if explain == ExplainMode::Off {
        report_text
    } else {
        format!(
            "{report_text}\n\n{}",
            render_explain(&report.provenance, explain)
        )
    }
}

/// Renders a task overhead report for `aub task overhead --since`.
pub fn render_task_overhead_report(report: &TaskOverheadReport) -> String {
    render_task_overhead_report_with_explain(report, ExplainMode::Off)
}

/// Renders a task overhead report, optionally including the explain block.
/// Task-attributed consumption renders alongside the overhead buckets rather
/// than behind a flag (`aub-eu7.3`'s restored criterion).
pub fn render_task_overhead_report_with_explain(
    report: &TaskOverheadReport,
    explain: ExplainMode,
) -> String {
    let mut lines = vec![format!(
        "task overhead from {} to {} (UTC days, end exclusive)",
        report.since.iso(),
        report.until.iso()
    )];
    lines.push(format!(
        "task-attributed usage: {}",
        render_usage_line(&report.task_usage)
    ));
    if report.buckets.is_empty() {
        lines.push("overhead: none".to_string());
    } else {
        lines.push("overhead buckets:".to_string());
        for bucket in &report.buckets {
            lines.push(format!(
                "  {} ({}% share)  {}",
                bucket.reason.as_str(),
                render_percentage(bucket.share.get(), PERCENT),
                render_usage_line(&bucket.usage)
            ));
        }
    }
    let report_text = lines.join("\n");
    if explain == ExplainMode::Off {
        report_text
    } else {
        format!(
            "{report_text}\n\n{}",
            render_explain(&report.provenance, explain)
        )
    }
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
            CheckStatus::Pass | CheckStatus::PassWithDetail(_) => "PASS".to_string(),
            CheckStatus::Fail(_) => "FAIL".to_string(),
            CheckStatus::NotApplicable(_) => "N/A ".to_string(),
            CheckStatus::NotYetAvailable { .. } => "TODO".to_string(),
        };
        let mut line = format!("  [{marker}] {}", outcome.name.as_str());
        match &outcome.status {
            CheckStatus::Fail(reason)
            | CheckStatus::NotApplicable(reason)
            | CheckStatus::PassWithDetail(reason) => {
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
    if let Some(residual) = &report.residual {
        lines.push(String::new());
        lines.push("Doctor: Rolling Residual Health".to_string());
        lines.push(format!(
            "  window: {} ({} eligible intervals, minimum: {})",
            render_coverage_duration(residual.window),
            residual.eligible_count,
            residual.min_eligible
        ));
        lines.push(format!(
            "  residual interval: [{} .. {}] credits",
            residual.rolling_residual_interval.lower().micros(),
            residual.rolling_residual_interval.upper().micros()
        ));
        if let Some(fraction) = residual.rolling_residual_fraction {
            lines.push(format!("  residual fraction: {:+.2}%", fraction * 100.0));
        } else {
            lines.push("  residual fraction: n/a".to_string());
        }
        match &residual.verdict {
            crate::reconciliation::RollingResidualVerdict::Suppressed {
                eligible_count,
                min_eligible,
            } => {
                lines.push(format!(
                    "  verdict: suppressed ({eligible_count} eligible intervals below minimum {min_eligible})"
                ));
            }
            crate::reconciliation::RollingResidualVerdict::ReconcilesWithinUncertainty => {
                lines.push("  verdict: reconciles within uncertainty".to_string());
            }
            crate::reconciliation::RollingResidualVerdict::Discrepancy { .. } => {
                lines.push("  verdict: discrepancy".to_string());
            }
        }
        for pattern in &residual.patterns {
            lines.push(format!("  {}", pattern.explanation()));
        }
        if let Some(pointer) = residual.pointer {
            lines.push(format!("  {pointer}"));
        }
    }
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

/// The presentation shape of a limiting window: its duration when known, or the
/// fact that no window is in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitingWindowDisplay {
    Duration(NominalWindowDuration),
    NotStarted,
}

impl From<NominalWindowDuration> for LimitingWindowDisplay {
    fn from(duration: NominalWindowDuration) -> Self {
        LimitingWindowDisplay::Duration(duration)
    }
}

impl From<&crate::report::LimitingWindow> for LimitingWindowDisplay {
    fn from(limit: &crate::report::LimitingWindow) -> Self {
        if limit.reset_state.is_not_started() {
            LimitingWindowDisplay::NotStarted
        } else {
            LimitingWindowDisplay::Duration(limit.nominal_duration)
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
    limiting_window: Option<LimitingWindowDisplay>,
) -> String {
    match reading {
        Freshness::Fresh { observed, .. } => {
            let value = render_percentage(observed.value().as_ppm().get(), precision);
            // The fresh line names the limiting window's nominal length, the
            // suffix the design's example shows: "38% left · 5h", or " · no window in progress"
            // for a window that has not yet started. The window
            // is part of the value's meaning, not of its freshness.
            let window = match limiting_window {
                Some(LimitingWindowDisplay::NotStarted) => " · no window in progress".to_string(),
                Some(LimitingWindowDisplay::Duration(duration)) => {
                    format!(" · {}", render_window_duration(duration))
                }
                None => String::new(),
            };
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

/// A coverage fraction as a percentage that always carries its one decimal:
/// the table reads down a column, and "100" beside "88.9" reads as a
/// different unit. The rounding is the same half-up step
/// [`render_percentage`] applies, so a table cell and the threshold message
/// can never disagree about the number they were both handed.
fn coverage_percent_cell(fraction: crate::coverage::CoverageFraction) -> String {
    let tenths = (u64::from(fraction.as_ppm()) * 10 + 5_000) / 10_000;
    format!("{}.{:01}%", tenths / 10, tenths % 10)
}

/// The attempts cell: the coverage percentage where one exists, the named
/// refusal where the engine refused to compute one. A policy the ledger
/// cannot reconstruct reads as "unknown", never as a number; a policy that
/// owed nothing reads as "none", because there were no attempts to cover.
/// When the covered span is shorter than the window the percentage names it
/// (`98.7% of 12h 46m`); a fully covered window shows the percentage alone.
fn coverage_attempts_cell(
    engine: &crate::coverage::CoverageReport,
    window: MonotonicDuration,
) -> String {
    match engine.attempt_coverage {
        Some(fraction) => {
            let percent = coverage_percent_cell(fraction);
            match engine.policy_covered_span {
                Some(covered) if covered.as_nanos() < window.as_nanos() => {
                    format!("{percent} of {}", render_coverage_duration(covered))
                }
                _ => percent,
            }
        }
        None => match engine.expected_opportunities {
            None => "unknown".to_string(),
            // Nothing was owed: there were no attempts to cover.
            Some(0) => "none".to_string(),
            Some(_) => "unknown".to_string(),
        },
    }
}

/// The measurements cell: the conditional coverage over terminal attempts, or
/// the named refusal when no attempt reached a terminal state.
fn coverage_measurements_cell(engine: &crate::coverage::CoverageReport) -> String {
    match engine.measurement_coverage {
        Some(fraction) => coverage_percent_cell(fraction),
        None => "none".to_string(),
    }
}

/// The window the coverage report covers, from its own timestamps: the
/// duration `render_coverage_detail` compares the covered span against to
/// decide a partial span.
fn coverage_report_window_duration(report: &CoverageReport) -> MonotonicDuration {
    MonotonicDuration::from_nanos(
        (report.until.unix_nanos() - report.since.unix_nanos()).max(0) as u64,
    )
}

/// The detail block of one account, when its numbers need explaining: the
/// floor breaches, the non-zero failure classes largest first, the
/// interruptions, and the resets lost to blind gaps. A healthy account
/// renders no block: the table row already carries its numbers. An
/// unconfigured account renders no block either: it is not a table row, and
/// its history reaches the operator on the one "not in config" line.
/// A partially covered window renders its span (`policy known for 12h 46m
/// of 24h`); a window with no applicable snapshot renders no policy line,
/// the `unknown` cell already saying it.
fn render_coverage_detail(
    report: &CoverageReport,
    account: &crate::report::CoverageAccount,
    window_label: &str,
) -> Option<Vec<String>> {
    let engine = &account.engine;
    if !account.configured {
        return None;
    }
    let attempt_below_floor = account.configured
        && engine
            .attempt_coverage
            .is_some_and(|coverage| coverage.as_f64() < report.threshold.attempt_floor.get());
    let measurement_below_floor = account.configured
        && engine
            .measurement_coverage
            .is_some_and(|coverage| coverage.as_f64() < report.threshold.measurement_floor.get());
    let interrupted = engine.started_without_terminal_result > 0;
    let window = coverage_report_window_duration(report);
    let partial_span = engine
        .policy_covered_span
        .is_some_and(|covered| covered.as_nanos() < window.as_nanos());
    let severe = !engine.reset_spanning_gaps.is_empty();
    if !partial_span && !attempt_below_floor && !measurement_below_floor && !interrupted && !severe
    {
        return None;
    }

    let mut lines = Vec::new();
    if partial_span {
        let covered = engine.policy_covered_span.unwrap_or(window);
        lines.push(format!(
            "policy known for {} of {window_label}",
            render_coverage_duration(covered)
        ));
    } else if attempt_below_floor {
        lines.push(format!(
            "attempt coverage below the {}% floor",
            render_percentage(report.threshold.attempt_floor.as_ppm(), COVERAGE_PERCENT)
        ));
    }
    if measurement_below_floor {
        lines.push(format!(
            "measurement coverage below the {}% floor",
            render_percentage(
                report.threshold.measurement_floor.as_ppm(),
                COVERAGE_PERCENT
            )
        ));
    }
    for failure in &account.error_classifications {
        let noun = if failure.count == 1 {
            "attempt"
        } else {
            "attempts"
        };
        lines.push(format!(
            "{count} {noun} refused with {classification}",
            count = failure.count,
            classification = failure.classification
        ));
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
                "one {} reset inside a sampling hole",
                render_coverage_duration(length)
            )),
            (1, _) => lines.push("one reset inside a sampling hole".to_string()),
            (count, _) => lines.push(format!("{count} resets inside a sampling hole")),
        }
    }
    Some(lines)
}

/// The report-to-rendering seam for coverage: one box carrying the title
/// with the interval, one table row per configured account with that
/// account's findings indented under its own row, the ledger's unconfigured
/// accounts on one dim line, and the threshold verdict's next action as the
/// footer. The model arrives complete; this function formats it. The title
/// echoes the window the command line asked for: "last 24h" is what the
/// operator requested, and the interval itself is carried by the model's own
/// timestamps.
pub fn render_coverage_report(report: &CoverageReport, window: &str, style: Style) -> String {
    if report.accounts.is_empty() {
        let mut lines = vec![format!("coverage - last {window}")];
        if report.severe_only {
            lines.push("(no account has a severe interval)".to_string());
        } else {
            lines.push("(no account has recorded sampling evidence in the ledger)".to_string());
        }
        return lines.join("\n");
    }

    let width = boxed_width(&style);
    let area = boxed_content_area(width);
    let table: Vec<&crate::report::CoverageAccount> =
        report.accounts.iter().filter(|a| a.configured).collect();
    let retired: Vec<&crate::report::CoverageAccount> =
        report.accounts.iter().filter(|a| !a.configured).collect();

    let name_width = table
        .iter()
        .map(|account| account.name.as_str().len())
        .chain(std::iter::once("account".len()))
        .max()
        .unwrap_or(7);
    let rows: Vec<Vec<String>> = table
        .iter()
        .map(|account| {
            let engine = &account.engine;
            vec![
                account.name.as_str().to_string(),
                coverage_attempts_cell(engine, coverage_report_window_duration(report)),
                coverage_measurements_cell(engine),
                engine
                    .longest_no_attempt_gap
                    .map(|gap| render_coverage_duration(gap.duration()))
                    .unwrap_or_else(|| "none".to_string()),
                engine.reset_spanning_gaps.len().to_string(),
            ]
        })
        .collect();
    let headers = [
        ("account", name_width),
        ("attempts", 8),
        ("measurements", 12),
        ("longest gap", 11),
        ("resets unobserved", 17),
    ];
    let column_widths: [usize; 5] = std::array::from_fn(|column| {
        headers[column]
            .1
            .max(rows.iter().map(|row| row[column].len()).max().unwrap_or(0))
    });
    let mut header = String::new();
    for (column, (text, _)) in headers.iter().enumerate() {
        header.push_str(&coverage_cell(text, column_widths[column]));
    }
    let table_width = header.trim_end().len();

    let mut lines = vec![boxed_top(
        &style.paint(style.bold(), &format!("coverage \u{b7} last {window}")),
        width,
    )];
    lines.push(boxed_blank(width));
    lines.push(boxed_body(header.trim_end(), width));
    lines.push(boxed_rule(table_width, width));
    for (row_index, account) in table.iter().enumerate() {
        let painted_name = style.paint(style.bold(), &rows[row_index][0]);
        let mut row = format!(
            "{painted_name}{}  ",
            " ".repeat(name_width - rows[row_index][0].len())
        );
        for column in 1..rows[row_index].len() {
            row.push_str(&coverage_cell(
                &rows[row_index][column],
                column_widths[column],
            ));
        }
        lines.push(boxed_body(row.trim_end(), width));
        let mut findings = render_coverage_detail(report, account, window).unwrap_or_default();
        if account.legacy_evidence_present {
            findings.push(
                "legacy observations are shown as historical evidence, not ordinary attempt coverage"
                    .to_string(),
            );
        }
        for line in coverage_finding_lines(findings, name_width, area) {
            lines.push(boxed_body(&style.paint(style.body(), &line), width));
        }
    }
    let footer = coverage_footer_lines(report);
    if !retired.is_empty() || !footer.is_empty() {
        lines.push(boxed_blank(width));
    }
    for line in coverage_not_in_config_lines(&retired, area) {
        lines.push(boxed_body(&style.paint(style.dim(), &line), width));
    }
    for line in footer {
        lines.push(boxed_body(&line, width));
    }
    lines.push(boxed_bottom(width));
    lines.join("\n")
}

/// The findings of one account laid under its row: consecutive findings
/// share one line, joined by " \u{b7} ", for as long as the joined line fits the
/// columns the content area allows past the account indent; the rest keep
/// one line each. The indent is the account column's own width, so the
/// findings read as the row's explanation rather than a second table.
fn coverage_finding_lines(findings: Vec<String>, indent: usize, area: usize) -> Vec<String> {
    let pad = " ".repeat(indent);
    let budget = area.saturating_sub(indent);
    let mut lines = Vec::new();
    let mut current: Option<String> = None;
    for finding in findings {
        match &mut current {
            Some(line) if line.len() + 3 + finding.len() <= budget => {
                line.push_str(" \u{b7} ");
                line.push_str(&finding);
            }
            Some(line) => {
                lines.push(format!("{pad}{line}"));
                current = Some(finding);
            }
            None => current = Some(finding),
        }
    }
    if let Some(line) = current {
        lines.push(format!("{pad}{line}"));
    }
    lines
}

/// The ledger's unconfigured accounts on one line: `not in config:` with
/// each account's name and the UTC date of its last observation, wrapped to
/// further lines at name boundaries when the width is exceeded. An account
/// with no observation in the interval names itself alone.
fn coverage_not_in_config_lines(
    accounts: &[&crate::report::CoverageAccount],
    area: usize,
) -> Vec<String> {
    if accounts.is_empty() {
        return Vec::new();
    }
    let prefix = "not in config: ";
    let segments: Vec<String> = accounts
        .iter()
        .map(
            |account| match account.engine.most_recent_successful_observation {
                Some(observed) => format!(
                    "{} (last observed {})",
                    account.name.as_str(),
                    observed.utc_date().iso()
                ),
                None => account.name.as_str().to_string(),
            },
        )
        .collect();
    let mut rows: Vec<String> = Vec::new();
    let mut line = String::new();
    for segment in segments {
        if !line.is_empty() && prefix.len() + line.len() + 2 + segment.len() > area {
            rows.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push_str(", ");
        }
        line.push_str(&segment);
    }
    rows.push(line);
    let indent = " ".repeat(prefix.len());
    let mut rendered = vec![format!("{prefix}{}", rows[0])];
    for row in &rows[1..] {
        rendered.push(format!("{indent}{row}"));
    }
    rendered
}

/// The footer the box closes with: the `next:` sentence the threshold
/// message travels with when the verdict reports a breach, derived from that
/// message so the advice inside the box and the exit decision behind it come
/// from one verdict. A report with no breach has no footer: the table is the
/// whole answer, and nothing is advised.
fn coverage_footer_lines(report: &CoverageReport) -> Vec<String> {
    let message = render_coverage_threshold_message(report);
    if message == "no threshold breach was recorded" {
        Vec::new()
    } else {
        vec!["next: run coverage again once the floor condition changes".to_string()]
    }
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
        FailureClass::SchemaDrift => "schema drift",
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

/// Formats a summary of cleared diagnostic capture bodies for operator display.
pub fn render_clear_diagnostics(report: &crate::report::ClearDiagnosticsReport) -> String {
    let unit = if report.entries_removed == 1 {
        "body"
    } else {
        "bodies"
    };
    match &report.provider_filter {
        Some(provider) => format!(
            "Cleared {} retained {unit} ({} bytes) for provider '{provider}'",
            report.entries_removed, report.bytes_removed
        ),
        None => format!(
            "Cleared {} retained {unit} ({} bytes) in total",
            report.entries_removed, report.bytes_removed
        ),
    }
}

use crate::domain::interval::Interval;

fn format_credit_int_commas(credits: Credits) -> String {
    let whole = credits.micros() / 1_000_000;
    format_int_with_commas(whole)
}

fn format_int_with_commas(n: i64) -> String {
    let sign = if n < 0 { "-" } else { "" };
    let s = n.abs().to_string();
    let mut out = String::new();
    for (count, c) in s.chars().rev().enumerate() {
        if count > 0 && count % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    format!("{}{}", sign, out.chars().rev().collect::<String>())
}

fn format_credit_interval_commas(interval: Interval<Credits>) -> String {
    format!(
        "{}–{}",
        format_credit_int_commas(interval.lower()),
        format_credit_int_commas(interval.upper())
    )
}

pub(crate) fn format_duration_age(duration: MonotonicDuration) -> String {
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

pub(crate) fn format_time_hh_mm(timestamp: UtcTimestamp) -> String {
    let total_seconds = timestamp.unix_nanos().div_euclid(1_000_000_000);
    let day_seconds = total_seconds.rem_euclid(86_400);
    let hour = day_seconds / 3600;
    let minute = (day_seconds % 3600) / 60;
    format!("{:02}:{:02}", hour, minute)
}

/// Renders the can-run report according to PLAN.md §51.
pub fn render_can_run_report(report: &crate::report::CanRunReport) -> String {
    match &report.outcome {
        crate::report::CanRunOutcome::Ready(ready) => {
            let mut out = String::new();
            out.push_str(&format!("can-run: {}\n", report.task_kind));
            out.push_str(&format!("account: {}\n", report.account));
            out.push_str(&format!("model: {}\n\n", report.model));

            let age_str = match ready.observed_age {
                Some(age) => format!("{} ago", format_duration_age(age)),
                None => "just now".to_string(),
            };
            out.push_str(&format!(
                "constraining windows, fresh, observed {age_str}:\n"
            ));
            for w in &ready.windows {
                let pct = format!("{:.1}%", w.remaining_fraction_ppm as f64 / 10_000.0);
                out.push_str(&format!(
                    "  - {:<14}  {:>4} remaining  calibration #{}  headroom {} credits\n",
                    w.semantic_key.as_str(),
                    pct,
                    w.calibration_id,
                    format_credit_interval_commas(w.headroom)
                ));
            }
            out.push_str(&format!(
                "  - lowest remaining percentage: {}\n",
                ready.lowest_percentage_window.as_str()
            ));
            out.push_str(&format!(
                "  - limiting calibrated window:  {}\n",
                ready.limiting_window.as_str()
            ));
            for w in &ready.windows {
                if let Some(resets) = w.resets_at {
                    out.push_str(&format!(
                        "  - {} resets {}\n",
                        w.semantic_key.as_str(),
                        format_time_hh_mm(resets)
                    ));
                }
            }
            out.push('\n');

            out.push_str("historical exact task evidence:\n");
            out.push_str(&format!("  - n = {}\n", ready.task_evidence.sample_count));
            out.push_str(&format!(
                "  - median = {} credits\n",
                format_credit_int_commas(ready.task_evidence.median)
            ));
            out.push_str(&format!(
                "  - p25–p75 = {} credits\n",
                format_credit_interval_commas(ready.task_evidence.central_range)
            ));
            out.push_str(&format!(
                "  - p90 = {} credits\n\n",
                format_credit_int_commas(ready.task_evidence.upper_reference)
            ));

            out.push_str("calibration:\n");
            out.push_str(&format!("  - {}\n", ready.calibration_summary.description));
            if ready.calibration_summary.token_kind_coverage_complete {
                out.push_str("  - complete token-kind coverage\n\n");
            } else {
                out.push_str("  - incomplete token-kind coverage\n\n");
            }

            out.push_str("comparison:\n");
            for c in &ready.comparisons {
                out.push_str(&format!(
                    "  - {:<14}  central range leaves {} credits\n",
                    c.semantic_key.as_str(),
                    format_credit_interval_commas(c.margin)
                ));
            }
            if let Some(u) = &ready.upper_reference_comparison {
                if u.exceeds {
                    out.push_str(&format!(
                        "  - p90 reference exceeds {} headroom by {} credits\n\n",
                        u.limiting_window.as_str(),
                        format_credit_interval_commas(u.diff)
                    ));
                } else {
                    out.push_str(&format!(
                        "  - p90 reference leaves {} credits of {} headroom\n\n",
                        format_credit_interval_commas(u.diff),
                        u.limiting_window.as_str()
                    ));
                }
            } else {
                out.push('\n');
            }

            out.push_str(&format!("assessment: {}\n", ready.assessment.as_str()));
            out.push_str(&format!(
                "limiting window: {}\n",
                ready.limiting_window.as_str()
            ));
            out
        }
        crate::report::CanRunOutcome::Refused(refused) => {
            let mut out = format!("assessment: {}\n", refused.verdict.as_str());
            if refused.missing.len() == 1 && refused.attribution_quality.is_none() {
                out.push_str(&format!("reason: {}\n", refused.missing[0].reason));
            } else {
                out.push_str("missing:\n");
                for fact in &refused.missing {
                    out.push_str(&format!("  - {}: {}\n", fact.subject, fact.reason));
                }
                if let Some(attr) = &refused.attribution_quality {
                    let num_cr = format_int_with_commas(attr.numerator_micros / 1_000_000);
                    let denom_cr = format_int_with_commas(attr.denominator_micros / 1_000_000);
                    out.push_str(&format!(
                        "  - attribution_coverage_below_floor: numerator={}cr, denominator={}cr, fraction={:.2}, floor={:.2}, window={}, group={}, exclusions: unknown_tokens={}, unknown_account={}, incomplete_segmentation={}, estimated_tokens={}\n",
                        num_cr,
                        denom_cr,
                        attr.observed_fraction_ppm as f64 / 1_000_000.0,
                        attr.required_floor_ppm as f64 / 1_000_000.0,
                        attr.selection_window,
                        attr.group,
                        attr.unknown_token_components,
                        attr.unknown_account_attribution,
                        attr.incomplete_segmentation,
                        attr.estimated_tokens
                    ));
                }
            }
            out
        }
    }
}

/// Renders one `calibrate show` entry: the active coefficient together with
/// its residual, its uncertainty, the cost-model version and its token-kind
/// coverage, the plan tier, the fit date, the method, the evidence experiment,
/// the input hash, the fitter version and the health state. The fitted value
/// never appears without its residual and uncertainty on the same rendering.
pub fn render_calibrate_show_entry(entry: &crate::report::CalibrateShowEntry) -> String {
    let mut out = String::new();
    if entry.is_active {
        out.push_str(&format!(
            "active window calibration {}\n",
            entry.calibration_id
        ));
    } else {
        out.push_str(&format!(
            "window calibration {} ({})\n",
            entry.calibration_id, entry.health_label
        ));
    }
    out.push('\n');
    out.push_str(&format!(
        "provider/window: {} / {}\n",
        entry.provider, entry.window_semantic_key
    ));
    out.push_str(&format!("plan:            {}\n", entry.plan_tier));
    out.push_str(&format!(
        "cost model:      {}\n",
        entry.cost_model.cost_model_id
    ));
    out.push_str(&format!("method:          {}\n", entry.statistical_method));
    if entry.evidence_experiment_ids.is_empty() {
        out.push_str("evidence:        none recorded\n");
    } else {
        out.push_str(&format!(
            "evidence:        experiment {}\n",
            entry.evidence_experiment_ids.join(", experiment ")
        ));
    }
    out.push_str(&format!(
        "fitted:          {} micros/point\n",
        entry.fitted_micros_per_point
    ));
    out.push_str(&format!(
        "window capacity: {} micros\n",
        entry.equivalent_full_window_capacity_micros
    ));
    out.push_str(&format!(
        "uncertainty:     [{}..={}] micros/point\n",
        entry.uncertainty_low_micros_per_point, entry.uncertainty_high_micros_per_point
    ));
    out.push_str(&format!(
        "residual:        {} micros\n",
        entry.fit_residual_micros
    ));
    match entry.out_of_sample_residual_micros {
        Some(held_out) => out.push_str(&format!("held-out:        {held_out} micros\n")),
        None => out.push_str("held-out:        none recorded\n"),
    }
    out.push_str(&format!(
        "input hash:      {} (count {})\n",
        entry.inputs_digest_hex, entry.inputs_count
    ));
    out.push_str(&format!(
        "fitter:          {} / revision {}\n",
        entry.aub_version, entry.source_revision
    ));
    out.push_str(&format!(
        "fit date:        {} nanos\n",
        entry.fit_timestamp_nanos
    ));
    out.push_str(&format!("health:          {}\n", entry.health_label));
    out.push('\n');
    if entry.cost_model.cost_model_found {
        out.push_str(&format!("cost model {}:\n", entry.cost_model.cost_model_id));
    } else {
        out.push_str(&format!(
            "cost model {} (not in ledger):\n",
            entry.cost_model.cost_model_id
        ));
    }
    for kind in &entry.cost_model.kinds {
        let state = if kind.modeled { "modeled" } else { "missing" };
        out.push_str(&format!("  - {:<14} {state}\n", kind.kind_label));
    }
    if entry.cost_model.unknown_kinds.is_empty() {
        out.push_str("  - unknown kinds  none\n");
    } else {
        out.push_str(&format!(
            "  - unknown kinds  {}\n",
            entry.cost_model.unknown_kinds.join(", ")
        ));
    }
    out
}

/// Renders the `calibrate show` report: one block per active scope.
pub fn render_calibrate_show_report(report: &crate::report::CalibrateShowReport) -> String {
    if report.entries.is_empty() {
        return "no active calibration; fit and activate one with `aub calibrate fit` and `aub calibrate activate`"
            .to_string();
    }
    report
        .entries
        .iter()
        .map(render_calibrate_show_entry)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders the `calibrate history` report: every calibration with its health
/// state and its activation and supersession events. Each entry carries its
/// fitted value with its residual and uncertainty, never a bare coefficient.
pub fn render_calibrate_history_report(report: &crate::report::CalibrateHistoryReport) -> String {
    if report.entries.is_empty() {
        return "no calibrations recorded".to_string();
    }
    let mut out = String::new();
    for entry in &report.entries {
        out.push_str(&format!(
            "calibration {} ({})\n",
            entry.calibration_id, entry.health_label
        ));
        out.push_str(&format!(
            "  provider/window: {} / {}\n",
            entry.provider, entry.window_semantic_key
        ));
        out.push_str(&format!("  plan: {}\n", entry.plan_tier));
        out.push_str(&format!(
            "  fitted: {} micros/point\n",
            entry.fitted_micros_per_point
        ));
        out.push_str(&format!(
            "  residual: {} micros\n",
            entry.fit_residual_micros
        ));
        out.push_str(&format!(
            "  uncertainty: [{}..={}] micros/point\n",
            entry.uncertainty_low_micros_per_point, entry.uncertainty_high_micros_per_point
        ));
        out.push_str(&format!(
            "  fit date: {} nanos\n",
            entry.fit_timestamp_nanos
        ));
        if entry.events.is_empty() {
            out.push_str("  events: none (provisional)\n");
        } else {
            out.push_str("  events:\n");
            for event in &entry.events {
                match &event.supersedes {
                    Some(predecessor) => out.push_str(&format!(
                        "    - {} at {} by {} (policy {}) supersedes {}\n",
                        event.kind_label,
                        event.event_at_nanos,
                        event.actor,
                        event.activation_policy_version,
                        predecessor
                    )),
                    None => out.push_str(&format!(
                        "    - {} at {} by {} (policy {})\n",
                        event.kind_label,
                        event.event_at_nanos,
                        event.actor,
                        event.activation_policy_version
                    )),
                }
            }
        }
    }
    out
}

/// Renders the `calibrate compare` report: the percentage difference between
/// a candidate and the active record, the candidate's activation status stated
/// plainly, and both coefficients with their residuals and uncertainties.
pub fn render_calibrate_compare_report(report: &crate::report::CalibrateCompareReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "candidate {} differs from active {} by {}\n",
        report.candidate_id,
        report.active_id,
        crate::report::format_calibrate_difference_percent(report.difference_bps)
    ));
    if report.candidate_is_active {
        out.push_str("candidate is active\n");
    } else {
        out.push_str("candidate evidence is sufficient for comparison but is not active\n");
    }
    out.push_str(&format!(
        "candidate fitted: {} micros/point; residual: {} micros; uncertainty: [{}..={}] micros/point\n",
        report.candidate_fitted_micros_per_point,
        report.candidate_fit_residual_micros,
        report.candidate_uncertainty_low_micros_per_point,
        report.candidate_uncertainty_high_micros_per_point
    ));
    out.push_str(&format!(
        "active fitted: {} micros/point; residual: {} micros; uncertainty: [{}..={}] micros/point\n",
        report.active_fitted_micros_per_point,
        report.active_fit_residual_micros,
        report.active_uncertainty_low_micros_per_point,
        report.active_uncertainty_high_micros_per_point
    ));
    out
}

/// Renders the `calibrate activate` report: the explicit activation just
/// recorded, with the activated coefficient, its residual and its
/// uncertainty.
pub fn render_calibrate_activate_report(report: &crate::report::CalibrateActivateReport) -> String {
    let mut out = String::new();
    match &report.supersedes {
        Some(predecessor) => out.push_str(&format!(
            "calibration {} active (supersedes {}) by {} under policy {} at {}\n",
            report.calibration_id,
            predecessor,
            report.actor,
            report.activation_policy_version,
            report.event_at_nanos
        )),
        None => out.push_str(&format!(
            "calibration {} active by {} under policy {} at {}\n",
            report.calibration_id,
            report.actor,
            report.activation_policy_version,
            report.event_at_nanos
        )),
    }
    out.push_str(&format!(
        "fitted: {} micros/point; residual: {} micros; uncertainty: [{}..={}] micros/point\n",
        report.fitted_micros_per_point,
        report.fit_residual_micros,
        report.uncertainty_low_micros_per_point,
        report.uncertainty_high_micros_per_point
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coverage::{CoverageFraction, Gap};
    use crate::domain::attempt::AttemptId;
    use crate::domain::freshness::Observed;
    use crate::domain::quota::QuotaFractionPpm;
    use crate::domain::time::{MeasurementBasis, MonotonicDuration, ReceivedAt};

    const NANOS_PER_SECOND: i64 = 1_000_000_000;

    fn now() -> UtcTimestamp {
        UtcTimestamp::from_unix_nanos(1_000_000 * NANOS_PER_SECOND)
    }

    // ---- coverage box fixtures -------------------------------------------------

    const COVERAGE_TEST_FLOOR_ATTEMPT: f64 = 0.98;

    /// An engine report with the given coverages and gaps; every field the
    /// renderer reads is named by the caller, the rest are neutral.
    fn coverage_engine(
        attempt: Option<CoverageFraction>,
        measurement: Option<CoverageFraction>,
        longest_gap: Option<Gap>,
        reset_gaps: Vec<Gap>,
    ) -> crate::coverage::CoverageReport {
        crate::coverage::CoverageReport {
            expected_opportunities: Some(288),
            policy_covered_span: Some(MonotonicDuration::from_seconds(86_400)),
            attempted_opportunities: 256,
            successful_observations: 256,
            started_without_terminal_result: 0,
            attempt_coverage: attempt,
            measurement_coverage: measurement,
            longest_no_attempt_gap: longest_gap,
            longest_no_observation_gap: None,
            reset_spanning_gaps: reset_gaps,
            most_recent_timer_run: None,
            most_recent_successful_observation: None,
            severe: false,
        }
    }

    fn gap_6m() -> Gap {
        let start = UtcTimestamp::from_unix_nanos(0);
        Gap {
            start,
            end: UtcTimestamp::from_unix_nanos(360 * NANOS_PER_SECOND),
        }
    }

    fn coverage_account(
        name: &str,
        engine: crate::coverage::CoverageReport,
        failures: crate::report::coverage::CoverageFailureTally,
        error_classifications: Vec<crate::report::CoverageErrorClassification>,
        resets_in_gaps: Vec<crate::report::CoverageReset>,
        configured: bool,
    ) -> crate::report::CoverageAccount {
        crate::report::CoverageAccount {
            name: crate::logging::LogicalName::new(name.to_string()),
            engine,
            failures,
            error_classifications,
            resets_in_gaps,
            legacy_evidence_present: false,
            configured,
            provenance: crate::report::ProvenanceNode::new(
                [] as [crate::domain::provenance::EvidenceId; 0],
                [] as [crate::domain::provenance::WitnessId; 0],
                crate::domain::provenance::QuerySemantics::new("coverage", "test"),
                1,
                1,
                crate::report::ValueArithmetic::Count,
            ),
        }
    }

    fn coverage_report(
        accounts: Vec<crate::report::CoverageAccount>,
        threshold: crate::report::CoverageThreshold,
    ) -> CoverageReport {
        let at = UtcTimestamp::from_unix_nanos(0);
        let until = UtcTimestamp::from_unix_nanos(86_400 * NANOS_PER_SECOND);
        CoverageReport::new(
            crate::report::ReportMetadata::new(
                at,
                at,
                crate::report::LedgerGeneration::new(1),
                None,
            ),
            at,
            until,
            false,
            threshold,
            accounts,
        )
    }

    fn floors_met(attempt: f64, measurement: f64) -> crate::report::CoverageThreshold {
        crate::report::CoverageThreshold {
            attempt_floor: crate::config::CoverageFloor::new(attempt).unwrap(),
            measurement_floor: crate::config::CoverageFloor::new(measurement).unwrap(),
            met: true,
            breaches: Vec::new(),
        }
    }

    fn attempt_breach(name: &str, coverage: CoverageFraction) -> crate::report::CoverageBreach {
        crate::report::CoverageBreach {
            account: crate::logging::LogicalName::new(name.to_string()),
            dimension: crate::report::CoverageBreachDimension::Attempt,
            coverage,
            floor: crate::config::CoverageFloor::new(COVERAGE_TEST_FLOOR_ATTEMPT).unwrap(),
        }
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
                Some(NominalWindowDuration::from_nanos(5 * 3_600 * NANOS_PER_SECOND as u64).into(),),
            ),
            "38% left · 5h"
        );
        assert_eq!(
            render_meter_reading(
                &fresh,
                "%",
                precision,
                now,
                envelope,
                Some(LimitingWindowDisplay::NotStarted),
            ),
            "38% left · no window in progress"
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

    /// Each window renders at its own reported resolution (aub-v8wt): whole
    /// percent at 10_000 ppm, tenths at 1_000, hundredths at 100, down to four
    /// decimals at 1 ppm. The opencode pair that collided at `2%` renders
    /// `1.6%` and `2.4%`. Trailing zeros trim, so a whole value at a fine
    /// resolution reads `53%`, never `53.0%`. The planted negative is the naive
    /// whole-percent rounder: it prints `2%` for both opencode values.
    #[test]
    fn status_percent_text_renders_each_window_at_its_reported_resolution() {
        use crate::domain::window::{QuantizationSemantics, WindowResetState, WindowScope};
        let text = |used_ppm: i32, resolution_ppm: i32| {
            status_used_percent_text(&status_window_with_resolution(
                "w",
                WindowScope::AccountWide,
                used_ppm,
                5 * 3_600,
                WindowResetState::Known(UtcTimestamp::from_unix_nanos(now().unix_nanos())),
                resolution_ppm,
                QuantizationSemantics::Exact,
            ))
        };
        // The opencode collision pair.
        assert_eq!(text(16_000, 1_000), "1.6%");
        assert_eq!(text(24_000, 1_000), "2.4%");
        // Whole-percent granularity never gains a decimal.
        assert_eq!(text(20_000, 10_000), "2%");
        assert_eq!(text(530_000, 10_000), "53%");
        // Trailing zeros trim at fine resolutions too.
        assert_eq!(text(530_000, 1_000), "53%");
        assert_eq!(text(531_000, 1_000), "53.1%");
        // Hundredths and below.
        assert_eq!(text(385_500, 100), "38.55%");
        assert_eq!(text(380_000, 100), "38%");
        assert_eq!(text(1, 1), "0.0001%");
        assert_eq!(text(10, 1), "0.001%");
        // Half-up rounding at the rendered resolution.
        assert_eq!(text(16_500, 1_000), "1.7%");
        assert_eq!(text(15_000, 10_000), "2%");
        assert_eq!(text(1_000_000, 1_000), "100%");
        assert_eq!(text(0, 1_000), "0%");
    }

    /// A quantization other than `exact` never renders a bare number
    /// (aub-v8wt): the provider asserted an interval, not a scalar, so the
    /// text carries the approximate marker the stale reader already uses. The
    /// decimals stay the reported resolution: the marker qualifies the
    /// precision, it does not coarsen it. The planted negative is the bare
    /// `1.6%` for a rounded reading, which claims exactness.
    #[test]
    fn status_percent_text_marks_non_exact_quantization_approximate() {
        use crate::domain::window::{QuantizationSemantics, WindowResetState, WindowScope};
        let text = |quantization: QuantizationSemantics| {
            status_used_percent_text(&status_window_with_resolution(
                "w",
                WindowScope::AccountWide,
                16_000,
                5 * 3_600,
                WindowResetState::Known(UtcTimestamp::from_unix_nanos(now().unix_nanos())),
                1_000,
                quantization,
            ))
        };
        assert_eq!(text(QuantizationSemantics::Exact), "1.6%");
        assert_eq!(text(QuantizationSemantics::RoundedToNearest), "~1.6%");
        assert_eq!(text(QuantizationSemantics::RoundedDown), "~1.6%");
        assert_eq!(text(QuantizationSemantics::RoundedUp), "~1.6%");
        assert_eq!(text(QuantizationSemantics::Unknown), "~1.6%");
    }

    /// Bar fill and percent text derive from the same `quota_used` (aub-v8wt):
    /// the text parses back to within the rendered resolution of the value
    /// the bar was filled from, at every supported resolution. The bar cell is
    /// a thirtieth of the cap, far coarser than any text resolution, so
    /// "agree" means the text round-trips within the resolution, not within a
    /// bar cell; the bar pins its exact fill formula beside it. The planted
    /// negative derives the bar from the whole-percent-rounded value: at
    /// 16_000 ppm the exact fill is 0 cells and the rounded fill is 1.
    #[test]
    fn status_bar_fill_and_percent_text_agree_within_the_rendered_resolution() {
        use crate::domain::window::{QuantizationSemantics, WindowResetState, WindowScope};
        fn displayed_ppm(text: &str) -> u32 {
            let digits = text.trim_start_matches('~').strip_suffix('%').unwrap();
            let (int, frac) = match digits.split_once('.') {
                Some((int, frac)) => (int, frac),
                None => (digits, ""),
            };
            let padded = format!("{frac:0<4}");
            int.parse::<u32>().unwrap() * 10_000 + padded[..4].parse::<u32>().unwrap()
        }
        for (used_ppm, resolution_ppm) in [
            (0, 10_000),
            (16_000, 1_000),
            (24_000, 1_000),
            (620_000, 10_000),
            (849_999, 1_000),
            (385_500, 100),
            (1, 1),
            (999_999, 1),
            (500_000, 10_000),
        ] {
            let window = status_window_with_resolution(
                "w",
                WindowScope::AccountWide,
                used_ppm,
                5 * 3_600,
                WindowResetState::Known(UtcTimestamp::from_unix_nanos(now().unix_nanos())),
                resolution_ppm,
                QuantizationSemantics::Exact,
            );
            let text = status_used_percent_text(&window);
            let round_trip = displayed_ppm(&text);
            assert!(
                (i64::from(round_trip) - i64::from(used_ppm)).abs() <= i64::from(resolution_ppm),
                "text {text} round-trips within {resolution_ppm} ppm of {used_ppm}"
            );
            let tone = status_used_tone(Style::plain(), used_ppm as u32, &window);
            let bar = status_bar(&window, Style::plain(), tone);
            let expected = ((u64::from(used_ppm as u32) * STATUS_BAR_CELLS as u64 + 500_000)
                / 1_000_000) as usize;
            assert_eq!(
                bar.chars().filter(|c| *c == '\u{2501}').count(),
                expected,
                "bar fill derives from the same {used_ppm} ppm the text renders"
            );
        }
    }

    /// The account-wide label names the window by what it is: `week` only for
    /// seven days, `month` for 28 to 31 days, and the rendered length for
    /// anything else. The planted negative is the 24-hour window: it used to
    /// read `week` because every window of a day or more did.
    #[test]
    fn the_account_wide_label_tells_a_month_from_a_week() {
        let label = |seconds: i64| status_window_label(&account_wide(0, seconds, seconds));
        assert_eq!(label(5 * 3_600), "5h");
        assert_eq!(label(86_400), "1d");
        assert_eq!(label(7 * 86_400), "week");
        assert_eq!(label(28 * 86_400), "month");
        assert_eq!(label(30 * 86_400), "month");
        assert_eq!(label(31 * 86_400), "month");
        assert_eq!(label(60 * 86_400), "60d");
    }

    /// A rendered block holding the opencode shape (5h, weekly, monthly) labels
    /// the third row `month`, so two rows with different figures never share a
    /// label.
    #[test]
    fn the_status_block_labels_a_thirty_day_window_month() {
        use crate::logging::LogicalName;
        use crate::report::MeterAccount;

        let opencode = MeterAccount::from_projection(
            LogicalName::new("opencode"),
            Freshness::Fresh {
                observed: observed(20_000, UtcTimestamp::from_unix_nanos(now().unix_nanos())),
                latest_attempt: crate::domain::attempt::AttemptId::new(1),
            },
            None,
            vec![],
            None,
        )
        .with_provider("opencode")
        .with_windows(vec![
            account_wide(0, 5 * 3_600, 3 * 3_600),
            account_wide(10_000, 7 * 86_400, 4 * 86_400),
            account_wide(20_000, 30 * 86_400, 20 * 86_400),
        ]);

        let rendered = render_status_report(
            &grid_report(vec![opencode]),
            now(),
            envelope(),
            Style::plain(),
        );
        let window_rows: Vec<&str> = rendered
            .lines()
            .filter(|line| line.starts_with("    ") && !line.trim().is_empty())
            .collect();
        assert_eq!(window_rows.len(), 3, "{rendered}");
        assert!(window_rows[0].contains("5h  "), "{:?}", window_rows[0]);
        assert!(window_rows[1].contains("week"), "{:?}", window_rows[1]);
        assert!(window_rows[2].contains("month"), "{:?}", window_rows[2]);
        assert!(!window_rows[2].contains("week"), "{:?}", window_rows[2]);
    }

    fn status_window(
        semantic_key: &str,
        scope: crate::domain::window::WindowScope,
        used_ppm: i32,
        nominal_seconds: i64,
        reset: crate::domain::window::WindowResetState,
    ) -> crate::report::StatusWindow {
        status_window_with_resolution(
            semantic_key,
            scope,
            used_ppm,
            nominal_seconds,
            reset,
            10_000,
            crate::domain::window::QuantizationSemantics::Exact,
        )
    }

    fn status_window_with_resolution(
        semantic_key: &str,
        scope: crate::domain::window::WindowScope,
        used_ppm: i32,
        nominal_seconds: i64,
        reset: crate::domain::window::WindowResetState,
        resolution_ppm: i32,
        quantization: crate::domain::window::QuantizationSemantics,
    ) -> crate::report::StatusWindow {
        crate::report::StatusWindow {
            semantic_key: semantic_key.to_string(),
            scope,
            quota_used: crate::domain::quota::QuotaUsed::new(
                QuotaFractionPpm::new(used_ppm).unwrap(),
            ),
            reported_resolution_ppm: crate::domain::window::ReportedResolution::new(
                QuotaFractionPpm::new(resolution_ppm).unwrap(),
            )
            .unwrap(),
            quantization,
            reset_state: reset,
            nominal_duration: NominalWindowDuration::from_nanos(
                (nominal_seconds * NANOS_PER_SECOND) as u64,
            ),
            rate: crate::domain::burn_rate::BurnRate::from_window(
                used_ppm.max(0) as u32,
                Some(0.5),
            ),
            capped_at: None,
            observation: Freshness::Fresh {
                observed: observed(
                    500_000,
                    UtcTimestamp::from_unix_nanos(now().unix_nanos() - 90 * NANOS_PER_SECOND),
                ),
                latest_attempt: crate::domain::attempt::AttemptId::new(1),
            },
        }
    }

    fn account_wide(
        used_ppm: i32,
        nominal_seconds: i64,
        reset_offset_seconds: i64,
    ) -> crate::report::StatusWindow {
        status_window(
            "w",
            crate::domain::window::WindowScope::AccountWide,
            used_ppm,
            nominal_seconds,
            crate::domain::window::WindowResetState::Known(UtcTimestamp::from_unix_nanos(
                now().unix_nanos() + reset_offset_seconds * NANOS_PER_SECOND,
            )),
        )
    }

    fn grid_report(accounts: Vec<crate::report::MeterAccount>) -> crate::report::StatusReport {
        use crate::report::{LedgerGeneration, ReportMetadata, StatusReport};
        let now = now();
        StatusReport::new(
            ReportMetadata::new(now, now, LedgerGeneration::new(0), None),
            accounts,
            vec![],
            crate::report::ProjectionReadState::Read,
        )
    }

    /// A two-account provider block: the header line, one `anthropic` rule, both
    /// accounts, and per account the `5h` and `week` rows plus one `fable` row
    /// where a model-scoped window exists. Every non-blank row is the same
    /// length and the fixed column starts line up. The planted negative is the
    /// order: `fable` must come after `week`, and the row lengths must not drift
    /// when the model row is present.
    #[test]
    fn the_two_account_provider_block_renders_the_fixed_grid() {
        use crate::domain::window::{ModelId, WindowResetState, WindowScope};
        use crate::logging::LogicalName;
        use crate::report::MeterAccount;

        let fable = status_window(
            "weekly_scoped_fable",
            WindowScope::ModelSpecific(ModelId::new("fable".to_string())),
            880_000,
            7 * 86_400,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(
                now().unix_nanos() + 3 * 86_400 * NANOS_PER_SECOND,
            )),
        );
        let primary = MeterAccount::from_projection(
            LogicalName::new("primary"),
            Freshness::Fresh {
                observed: observed(380_000, UtcTimestamp::from_unix_nanos(now().unix_nanos())),
                latest_attempt: crate::domain::attempt::AttemptId::new(1),
            },
            None,
            vec![],
            None,
        )
        .with_provider("anthropic")
        .with_windows(vec![
            account_wide(620_000, 5 * 3_600, 3 * 3_600),
            account_wide(410_000, 7 * 86_400, 4 * 86_400),
            fable,
        ]);
        let gmail = MeterAccount::from_projection(
            LogicalName::new("gmail"),
            Freshness::Fresh {
                observed: observed(950_000, UtcTimestamp::from_unix_nanos(now().unix_nanos())),
                latest_attempt: crate::domain::attempt::AttemptId::new(2),
            },
            None,
            vec![],
            None,
        )
        .with_provider("anthropic")
        .with_windows(vec![
            account_wide(120_000, 5 * 3_600, 3 * 3_600),
            account_wide(50_000, 7 * 86_400, 4 * 86_400),
        ]);

        let rendered = render_status_report(
            &grid_report(vec![primary, gmail]),
            now(),
            envelope(),
            Style::plain(),
        );
        let lines: Vec<&str> = rendered.lines().collect();

        assert_eq!(lines[0].chars().take(5).collect::<String>(), "QUOTA");
        assert_eq!(
            lines[0].chars().count(),
            80,
            "the header fills the default width: {:?}",
            lines[0]
        );
        assert!(lines[1].is_empty(), "one blank line under the header");
        assert!(
            lines[2].starts_with("anthropic ") && lines[2].chars().count() == 80,
            "the provider rule reaches the right edge: {:?}",
            lines[2]
        );
        assert_eq!(lines[3], "  primary  anthropic");

        // The five window rows across both accounts, in order, all the same length.
        let window_rows: Vec<&&str> = lines
            .iter()
            .filter(|line| line.starts_with("    ") && !line.trim().is_empty())
            .collect();
        assert_eq!(
            window_rows.len(),
            5,
            "three rows for primary, two for gmail"
        );
        let row_len = window_rows[0].chars().count();
        for row in &window_rows {
            assert_eq!(
                row.chars().count(),
                row_len,
                "every row is the same width: {row:?}"
            );
            // Fixed column starts: label at 4, bar at 13, percent group after it.
            assert_eq!(&row[..4], "    ");
        }
        assert!(window_rows[0].contains("5h  "));
        assert!(window_rows[1].contains("week"));
        assert!(
            window_rows[2].contains("fable"),
            "the model row follows the weekly one: {:?}",
            window_rows[2]
        );

        // The 5h row of primary: 62% used, a 30-cell bar with 19 filled.
        let bar: String = window_rows[0]
            .chars()
            .filter(|c| *c == '\u{2501}' || *c == '\u{2500}')
            .collect();
        assert_eq!(bar.chars().count(), 30);
        assert_eq!(bar.chars().filter(|c| *c == '\u{2501}').count(), 19);
        assert!(window_rows[0].contains(" 62% "));
    }

    /// A quota-group account renders one dim sub-block line per group with
    /// the group's window rows beneath it, in the order the provider
    /// reported the groups and shortest nominal length first within one
    /// (`aub-n8yx`). The negative: the group line is not a window row, so
    /// the bar/percent grid cannot count it as one, and a `--model`
    /// selector hides the group sub-blocks the same way it hides other
    /// models' windows.
    #[test]
    fn quota_groups_render_as_sub_blocks_beneath_the_account() {
        use crate::domain::window::{GroupName, WindowResetState, WindowScope};
        use crate::logging::LogicalName;
        use crate::report::MeterAccount;

        let group_window = |key: &str, group: &str, used_ppm: i32, nominal_seconds: i64| {
            status_window(
                key,
                WindowScope::ModelGroup(GroupName::new(group.to_string())),
                used_ppm,
                nominal_seconds,
                WindowResetState::Known(UtcTimestamp::from_unix_nanos(
                    now().unix_nanos() + 3 * 3_600 * NANOS_PER_SECOND,
                )),
            )
        };
        let agy = MeterAccount::from_projection(
            LogicalName::new("agy"),
            Freshness::Fresh {
                observed: observed(930_000, UtcTimestamp::from_unix_nanos(now().unix_nanos())),
                latest_attempt: crate::domain::attempt::AttemptId::new(1),
            },
            None,
            vec![],
            None,
        )
        .with_provider("agy")
        .with_windows(vec![
            group_window("weekly", "Gemini Models", 935_441, 7 * 86_400),
            group_window("5h", "Gemini Models", 85_528, 5 * 3_600),
            group_window("weekly", "Claude and GPT models", 1_000_000, 7 * 86_400),
            group_window("5h", "Claude and GPT models", 0, 5 * 3_600),
        ]);

        let rendered =
            render_status_report(&grid_report(vec![agy]), now(), envelope(), Style::plain());
        let lines: Vec<&str> = rendered.lines().collect();
        let group_lines: Vec<&str> = lines
            .iter()
            .filter(|line| {
                line.starts_with("    ") && !line.contains('\u{2501}') && !line.contains('%')
            })
            .map(|line| line.as_ref())
            .collect();
        assert_eq!(
            group_lines,
            vec!["    Gemini Models", "    Claude and GPT models"],
            "one dim line per group, in first-seen order: {:?}",
            group_lines
        );

        // Beneath each group line, the group's two window rows, 5h first.
        let all_lines: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
        let gemini_index = all_lines
            .iter()
            .position(|line| line.contains("Gemini Models"))
            .unwrap();
        assert!(all_lines[gemini_index + 1].contains("5h"));
        assert!(all_lines[gemini_index + 2].contains("week"));
        let thirdp_index = all_lines
            .iter()
            .position(|line| line.contains("Claude and GPT models"))
            .unwrap();
        assert!(all_lines[thirdp_index + 1].contains("5h"));
        assert!(all_lines[thirdp_index + 2].contains("week"));

        // The group rows are ordinary grid rows: label column, bar, percent.
        let group_row = &all_lines[gemini_index + 1];
        assert_eq!(&group_row[..4], "    ");
        assert!(group_row.contains('\u{2501}'));

        // The negative: under a --model selector the group sub-blocks are
        // not the selected model's own windows, so they do not join the grid
        // (the same set the reading is computed over).
        let selected = MeterAccount::from_projection(
            LogicalName::new("agy"),
            Freshness::Fresh {
                observed: observed(930_000, UtcTimestamp::from_unix_nanos(now().unix_nanos())),
                latest_attempt: crate::domain::attempt::AttemptId::new(1),
            },
            None,
            vec![],
            Some(crate::domain::window::ModelId::new("gemini-pro")),
        )
        .with_provider("agy")
        .with_windows(vec![group_window("5h", "Gemini Models", 85_528, 5 * 3_600)]);
        let rendered_selected = render_status_report(
            &grid_report(vec![selected]),
            now(),
            envelope(),
            Style::plain(),
        );
        assert!(
            !rendered_selected.contains("Gemini Models"),
            "a selected model's grid shows no group sub-block: {rendered_selected}"
        );
    }

    /// A stale account dims the whole block and every row's trailing column
    /// carries `cached <age> ago`; a fresh account carries no such note. The
    /// negative: the fresh block must not gain the cached note.
    #[test]
    fn a_stale_account_block_is_dimmed_and_notes_the_cache_age() {
        use crate::domain::window::{WindowResetState, WindowScope};
        use crate::logging::LogicalName;
        use crate::report::MeterAccount;

        let stale_observation = Freshness::Stale {
            last_good: Some(observed(
                380_000,
                UtcTimestamp::from_unix_nanos(
                    now().unix_nanos() - (2 * 86_400 + 22 * 3_600) * NANOS_PER_SECOND,
                ),
            )),
            latest_attempt: crate::domain::attempt::AttemptId::new(2),
            reason: StaleReason::AgeExceeded,
        };
        let mut window = status_window(
            "five_hour",
            WindowScope::AccountWide,
            300_000,
            5 * 3_600,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(
                now().unix_nanos() + 3_600 * NANOS_PER_SECOND,
            )),
        );
        window.observation = stale_observation.clone();
        let account = MeterAccount::from_projection(
            LogicalName::new("primary"),
            stale_observation,
            None,
            vec![],
            None,
        )
        .with_provider("anthropic")
        .with_windows(vec![window]);

        let plain = render_status_report(
            &grid_report(vec![account]),
            now(),
            envelope(),
            Style::plain(),
        );
        assert!(
            plain.contains("cached 2d 22h ago"),
            "the stale block notes the cache age: {plain}"
        );

        let coloured = render_status_report(
            &grid_report(vec![{
                // rebuild because the value was moved
                let stale_observation = Freshness::Stale {
                    last_good: Some(observed(
                        380_000,
                        UtcTimestamp::from_unix_nanos(
                            now().unix_nanos() - (2 * 86_400 + 22 * 3_600) * NANOS_PER_SECOND,
                        ),
                    )),
                    latest_attempt: crate::domain::attempt::AttemptId::new(2),
                    reason: StaleReason::AgeExceeded,
                };
                let mut window = status_window(
                    "five_hour",
                    WindowScope::AccountWide,
                    300_000,
                    5 * 3_600,
                    WindowResetState::Known(UtcTimestamp::from_unix_nanos(
                        now().unix_nanos() + 3_600 * NANOS_PER_SECOND,
                    )),
                );
                window.observation = stale_observation.clone();
                MeterAccount::from_projection(
                    LogicalName::new("primary"),
                    stale_observation,
                    None,
                    vec![],
                    None,
                )
                .with_provider("anthropic")
                .with_windows(vec![window])
            }]),
            now(),
            envelope(),
            Style::new(true, false, false),
        );
        // The dimmer escape (#3f4463) marks the stale trailing column.
        assert!(
            coloured.contains("\x1b[38;2;63;68;99mnot") || coloured.contains("\x1b[38;2;63;68;99m"),
            "a stale row's trailing column is painted dimmer: {coloured}"
        );
    }

    /// An auth-required account renders `auth!` under its header and no rows; a
    /// never-observed account renders the freshness answer. Neither substitutes
    /// a fabricated grid.
    #[test]
    fn an_account_with_no_windows_renders_the_freshness_answer() {
        use crate::domain::quota::QuotaRemaining;
        use crate::logging::LogicalName;
        use crate::report::MeterAccount;

        let auth = MeterAccount::new(
            LogicalName::new("legacy"),
            Freshness::<QuotaRemaining>::AuthRequired {
                last_good: None,
                latest_attempt: AttemptId::new(3),
            },
        )
        .with_provider("anthropic");
        let never = MeterAccount::new(
            LogicalName::new("fresh-account"),
            Freshness::Stale {
                last_good: None,
                latest_attempt: AttemptId::new(4),
                reason: StaleReason::NoSuccessfulObservation,
            },
        )
        .with_provider("anthropic");

        let rendered = render_status_report(
            &grid_report(vec![auth, never]),
            now(),
            envelope(),
            Style::plain(),
        );
        assert!(rendered.contains("  legacy  anthropic\n    auth!"));
        assert!(
            rendered.contains("  fresh-account  anthropic\n    ? · stale · no successful sample")
        );
    }

    /// The tone of a row follows percent used at the design's cut points: 0
    /// idle, 59 green, 60 yellow, 84 yellow, 85 red. Driven through the
    /// coloured style so the escape itself is the assertion.
    #[test]
    fn the_row_tone_follows_percent_used_at_every_cut_point() {
        use crate::domain::window::{WindowResetState, WindowScope};
        use crate::logging::LogicalName;
        use crate::report::MeterAccount;

        let rows = [
            (0_i32, "\x1b[38;2;63;68;99m"),      // idle
            (590_000, "\x1b[38;2;158;206;106m"), // green
            (600_000, "\x1b[38;2;224;175;104m"), // yellow
            (840_000, "\x1b[38;2;224;175;104m"), // yellow
            (850_000, "\x1b[38;2;247;118;142m"), // red
        ];
        for (used_ppm, expected_escape) in rows {
            let window = status_window(
                "w",
                WindowScope::AccountWide,
                used_ppm,
                5 * 3_600,
                WindowResetState::Known(UtcTimestamp::from_unix_nanos(
                    now().unix_nanos() + 3_600 * NANOS_PER_SECOND,
                )),
            );
            let account = MeterAccount::from_projection(
                LogicalName::new("primary"),
                Freshness::Fresh {
                    observed: observed(500_000, UtcTimestamp::from_unix_nanos(now().unix_nanos())),
                    latest_attempt: AttemptId::new(1),
                },
                None,
                vec![],
                None,
            )
            .with_provider("anthropic")
            .with_windows(vec![window]);
            let coloured = render_status_report(
                &grid_report(vec![account]),
                now(),
                envelope(),
                Style::new(true, false, false),
            );
            assert!(
                coloured.contains(expected_escape),
                "used {used_ppm} ppm must tint the row {expected_escape:?}: {coloured}"
            );
        }
    }

    // ---- the coverage box ------------------------------------------------------

    /// The bead's approved figure: two configured accounts and one retired
    /// ledger row, at the plain style's 80 columns. The retired row is not a
    /// table row; it renders on the one dim "not in config" line with its
    /// last observed date. The findings sit indented under their own row,
    /// joined by " · " while the joined line fits. The footer is the
    /// threshold verdict's next action, and the per-account breaches stay in
    /// the findings rather than repeating there.
    #[test]
    fn the_coverage_box_renders_the_approved_figure() {
        let below_floor = CoverageFraction::new(889, 1_000).unwrap();
        let reset = crate::report::CoverageReset {
            at: UtcTimestamp::from_unix_nanos(0),
            window_length: MonotonicDuration::from_seconds(3_600),
        };
        let primary = coverage_account(
            "primary",
            coverage_engine(
                Some(below_floor),
                Some(CoverageFraction::new(1, 1).unwrap()),
                Some(gap_6m()),
                vec![gap_6m(), gap_6m(), gap_6m()],
            ),
            crate::report::coverage::CoverageFailureTally::default(),
            Vec::new(),
            vec![reset, reset, reset],
            true,
        );
        let gmail = coverage_account(
            "gmail",
            coverage_engine(
                Some(CoverageFraction::new(1, 1).unwrap()),
                Some(CoverageFraction::new(766, 1_000).unwrap()),
                Some(gap_6m()),
                vec![gap_6m(), gap_6m(), gap_6m()],
            ),
            crate::report::coverage::CoverageFailureTally {
                rate_limited: 45,
                authentication: 14,
                ..Default::default()
            },
            vec![
                crate::report::CoverageErrorClassification {
                    classification: "rate_limit_error".to_owned(),
                    count: 45,
                },
                crate::report::CoverageErrorClassification {
                    classification: "authentication_error".to_owned(),
                    count: 14,
                },
            ],
            vec![reset, reset, reset],
            true,
        );
        let retired_at = UtcTimestamp::parse_rfc3339("2026-09-04T12:00:00Z").unwrap();
        let mut retired_engine = coverage_engine(
            Some(CoverageFraction::new(1, 1).unwrap()),
            Some(CoverageFraction::new(1, 1).unwrap()),
            None,
            Vec::new(),
        );
        retired_engine.most_recent_successful_observation = Some(retired_at);
        let retired = coverage_account(
            "primary-2026-09-04",
            retired_engine,
            crate::report::coverage::CoverageFailureTally::default(),
            Vec::new(),
            Vec::new(),
            false,
        );
        let mut threshold = floors_met(0.98, 0.75);
        threshold.met = false;
        threshold.breaches = vec![attempt_breach("primary", below_floor)];

        let report = coverage_report(vec![primary, gmail, retired], threshold);
        let rendered = render_coverage_report(&report, "24h", Style::plain());
        let expected = [
            "┌─ coverage · last 24h ────────────────────────────────────────────────────────┐",
            "│                                                                              │",
            "│  account  attempts  measurements  longest gap  resets unobserved             │",
            "│  ───────────────────────────────────────────────────────────────             │",
            "│  primary  88.9%     100.0%        6m           3                             │",
            "│         attempt coverage below the 98% floor                                 │",
            "│         3 resets inside a sampling hole                                      │",
            "│  gmail    100.0%    76.6%         6m           3                             │",
            "│         45 attempts refused with rate_limit_error                            │",
            "│         14 attempts refused with authentication_error                        │",
            "│         3 resets inside a sampling hole                                      │",
            "│                                                                              │",
            "│  not in config: primary-2026-09-04 (last observed 2026-09-04)                │",
            "│  next: run coverage again once the floor condition changes                   │",
            "└──────────────────────────────────────────────────────────────────────────────┘",
        ]
        .join("\n");
        assert_eq!(rendered, expected);
        for line in rendered.lines() {
            assert_eq!(line.chars().count(), 80, "{line}");
        }
    }

    /// With no retired ledger row the "not in config" line is absent: a
    /// healthy ledger says nothing about accounts it does not have. With no
    /// breach either, the footer is absent with it.
    #[test]
    fn the_coverage_box_names_no_unconfigured_line_without_retired_rows() {
        let primary = coverage_account(
            "primary",
            coverage_engine(
                Some(CoverageFraction::new(1, 1).unwrap()),
                Some(CoverageFraction::new(1, 1).unwrap()),
                Some(gap_6m()),
                Vec::new(),
            ),
            crate::report::coverage::CoverageFailureTally::default(),
            Vec::new(),
            Vec::new(),
            true,
        );
        let report = coverage_report(vec![primary], floors_met(0.98, 0.95));
        let rendered = render_coverage_report(&report, "24h", Style::plain());
        assert!(
            !rendered.contains("not in config"),
            "no retired rows, no unconfigured line: {rendered}"
        );
        assert!(
            !rendered.contains("next: run coverage again"),
            "no breach, no footer: {rendered}"
        );
        for line in rendered.lines() {
            assert_eq!(line.chars().count(), 80, "{line}");
        }
    }

    /// An account with zero attempts prints "none" in both percentage
    /// columns: the policy owed nothing, so there is no coverage number to
    /// show and no zero to mistake for a measurement. Its one finding sits
    /// under its row.
    #[test]
    fn the_coverage_box_prints_none_for_an_account_with_no_attempts() {
        let quiet_engine = crate::coverage::CoverageReport {
            expected_opportunities: Some(0),
            policy_covered_span: Some(MonotonicDuration::from_seconds(86_400)),
            attempted_opportunities: 0,
            successful_observations: 0,
            started_without_terminal_result: 0,
            attempt_coverage: None,
            measurement_coverage: None,
            longest_no_attempt_gap: Some(gap_6m()),
            longest_no_observation_gap: None,
            reset_spanning_gaps: vec![gap_6m()],
            most_recent_timer_run: None,
            most_recent_successful_observation: None,
            severe: true,
        };
        let quiet = coverage_account(
            "quiet",
            quiet_engine,
            crate::report::coverage::CoverageFailureTally::default(),
            Vec::new(),
            vec![crate::report::CoverageReset {
                at: UtcTimestamp::from_unix_nanos(0),
                window_length: MonotonicDuration::from_seconds(18_000),
            }],
            true,
        );
        let report = coverage_report(vec![quiet], floors_met(0.98, 0.95));
        let rendered = render_coverage_report(&report, "24h", Style::plain());
        let expected = [
            "┌─ coverage · last 24h ────────────────────────────────────────────────────────┐",
            "│                                                                              │",
            "│  account  attempts  measurements  longest gap  resets unobserved             │",
            "│  ───────────────────────────────────────────────────────────────             │",
            "│  quiet    none      none          6m           1                             │",
            "│         one 5h reset inside a sampling hole                                  │",
            "└──────────────────────────────────────────────────────────────────────────────┘",
        ]
        .join("\n");
        assert_eq!(rendered, expected);
    }

    /// The attempts cell names the covered span when it is shorter than the
    /// window (`98.7% of 12h 46m`) and the percentage alone when the policy
    /// covers the whole window. The planted negative is the full-coverage
    /// account: a cell that always appended the span would read `98.7% of
    /// 24h` there.
    #[test]
    fn the_attempts_cell_names_the_covered_span_only_when_partial() {
        let window = MonotonicDuration::from_seconds(86_400);
        let partial = crate::coverage::CoverageReport {
            expected_opportunities: Some(154),
            policy_covered_span: Some(MonotonicDuration::from_seconds(45_960)),
            attempted_opportunities: 152,
            successful_observations: 152,
            started_without_terminal_result: 0,
            attempt_coverage: CoverageFraction::new(987, 1_000),
            measurement_coverage: CoverageFraction::new(1, 1),
            longest_no_attempt_gap: None,
            longest_no_observation_gap: None,
            reset_spanning_gaps: Vec::new(),
            most_recent_timer_run: None,
            most_recent_successful_observation: None,
            severe: false,
        };
        assert_eq!(coverage_attempts_cell(&partial, window), "98.7% of 12h 46m");
        let full = crate::coverage::CoverageReport {
            policy_covered_span: Some(MonotonicDuration::from_seconds(86_400)),
            ..partial
        };
        assert_eq!(coverage_attempts_cell(&full, window), "98.7%");
    }

    /// A partially covered window explains itself under its row (`policy
    /// known for 12h 46m of 24h`); a fully covered window and a window with
    /// no applicable snapshot render no policy line. The planted negative is
    /// the fully covered account: a finding that appeared for every window
    /// would sit under a healthy row.
    #[test]
    fn the_detail_names_the_partial_span_and_only_then() {
        let covered = MonotonicDuration::from_seconds(45_960);
        let partial_engine = crate::coverage::CoverageReport {
            expected_opportunities: Some(154),
            policy_covered_span: Some(covered),
            attempted_opportunities: 152,
            successful_observations: 152,
            started_without_terminal_result: 0,
            attempt_coverage: CoverageFraction::new(987, 1_000),
            measurement_coverage: CoverageFraction::new(1, 1),
            longest_no_attempt_gap: None,
            longest_no_observation_gap: None,
            reset_spanning_gaps: Vec::new(),
            most_recent_timer_run: None,
            most_recent_successful_observation: None,
            severe: false,
        };
        let partial = coverage_account(
            "gmail",
            partial_engine,
            crate::report::coverage::CoverageFailureTally::default(),
            Vec::new(),
            Vec::new(),
            true,
        );
        let report = coverage_report(vec![partial], floors_met(0.98, 0.95));
        let findings =
            render_coverage_detail(&report, &report.accounts[0], "24h").unwrap_or_default();
        assert!(
            findings.contains(&"policy known for 12h 46m of 24h".to_string()),
            "a partial span must name itself: {findings:?}"
        );

        let full_engine = crate::coverage::CoverageReport {
            expected_opportunities: Some(288),
            policy_covered_span: Some(MonotonicDuration::from_seconds(86_400)),
            attempted_opportunities: 288,
            successful_observations: 288,
            started_without_terminal_result: 0,
            attempt_coverage: CoverageFraction::new(1, 1),
            measurement_coverage: CoverageFraction::new(1, 1),
            longest_no_attempt_gap: None,
            longest_no_observation_gap: None,
            reset_spanning_gaps: Vec::new(),
            most_recent_timer_run: None,
            most_recent_successful_observation: None,
            severe: false,
        };
        let full = coverage_account(
            "gmail",
            full_engine,
            crate::report::coverage::CoverageFailureTally::default(),
            Vec::new(),
            Vec::new(),
            true,
        );
        let full_report = coverage_report(vec![full], floors_met(0.98, 0.95));
        let full_findings = render_coverage_detail(&full_report, &full_report.accounts[0], "24h")
            .unwrap_or_default();
        assert!(
            !full_findings
                .iter()
                .any(|line| line.contains("policy known")),
            "a fully covered window renders no policy line: {full_findings:?}"
        );

        let unknown_engine = crate::coverage::CoverageReport {
            expected_opportunities: None,
            policy_covered_span: None,
            attempted_opportunities: 0,
            successful_observations: 0,
            started_without_terminal_result: 0,
            attempt_coverage: None,
            measurement_coverage: None,
            longest_no_attempt_gap: None,
            longest_no_observation_gap: None,
            reset_spanning_gaps: Vec::new(),
            most_recent_timer_run: None,
            most_recent_successful_observation: None,
            severe: false,
        };
        let unknown = coverage_account(
            "gmail",
            unknown_engine,
            crate::report::coverage::CoverageFailureTally::default(),
            Vec::new(),
            Vec::new(),
            true,
        );
        let unknown_report = coverage_report(vec![unknown], floors_met(0.98, 0.95));
        let unknown_findings =
            render_coverage_detail(&unknown_report, &unknown_report.accounts[0], "24h")
                .unwrap_or_default();
        assert!(
            !unknown_findings
                .iter()
                .any(|line| line.contains("policy known")),
            "no snapshot means no partial-span line: {unknown_findings:?}"
        );
    }

    /// The join boundary: two findings whose joined line lands exactly on the
    /// content budget share one line, and one character over it they keep
    /// one line each. The planted negative is the over-budget pair: a join
    /// that ignored the width would push the rail out.
    #[test]
    fn findings_join_up_to_the_width_budget_and_not_past_it() {
        let joined = coverage_finding_lines(vec!["a".repeat(30), "b".repeat(34)], 7, 74);
        assert_eq!(
            joined,
            vec![format!(
                "{}{} · {}",
                " ".repeat(7),
                "a".repeat(30),
                "b".repeat(34)
            )]
        );
        let split = coverage_finding_lines(vec!["a".repeat(31), "b".repeat(34)], 7, 74);
        assert_eq!(
            split,
            vec![
                format!("{}{}", " ".repeat(7), "a".repeat(31)),
                format!("{}{}", " ".repeat(7), "b".repeat(34))
            ]
        );
    }

    /// Three short unconfigured names share one line; segments too wide for
    /// the remaining width wrap to a second line at a name boundary, aligned
    /// under the first name.
    #[test]
    fn the_unconfigured_line_wraps_at_name_boundaries() {
        let named = |name: &str| {
            let mut engine = coverage_engine(
                Some(CoverageFraction::new(1, 1).unwrap()),
                Some(CoverageFraction::new(1, 1).unwrap()),
                None,
                Vec::new(),
            );
            engine.most_recent_successful_observation =
                Some(UtcTimestamp::parse_rfc3339("2026-09-01T00:00:00Z").unwrap());
            coverage_account(
                name,
                engine,
                Default::default(),
                Vec::new(),
                Vec::new(),
                false,
            )
        };
        let bare = |name: &str| {
            coverage_account(
                name,
                coverage_engine(
                    Some(CoverageFraction::new(1, 1).unwrap()),
                    Some(CoverageFraction::new(1, 1).unwrap()),
                    None,
                    Vec::new(),
                ),
                Default::default(),
                Vec::new(),
                Vec::new(),
                false,
            )
        };
        let short = [bare("alpha"), bare("beta"), bare("gamma")];
        let refs: Vec<&crate::report::CoverageAccount> = short.iter().collect();
        let one_line = coverage_not_in_config_lines(&refs, 74);
        assert_eq!(one_line.len(), 1, "{one_line:?}");
        assert_eq!(one_line[0], "not in config: alpha, beta, gamma");
        let dated = [named("a"), named("b"), named("c")];
        let dated_refs: Vec<&crate::report::CoverageAccount> = dated.iter().collect();
        let wrapped = coverage_not_in_config_lines(&dated_refs, 74);
        assert_eq!(wrapped.len(), 2, "{wrapped:?}");
        assert_eq!(
            wrapped[0],
            "not in config: a (last observed 2026-09-01), b (last observed 2026-09-01)"
        );
        assert_eq!(wrapped[1], "               c (last observed 2026-09-01)");
        assert!(wrapped.iter().all(|line| line.len() <= 74), "{wrapped:?}");
    }

    /// The footer is derived from the unchanged threshold message: a report
    /// whose message names breaches gets the one next action, carrying no
    /// breach text the message already carries; a report with no breach gets
    /// no footer. Pinned on three fixture shapes: a breach, a met verdict,
    /// and a zero-attempt account the verdict refuses to judge.
    #[test]
    fn the_footer_is_derived_from_the_threshold_message() {
        let below_floor = CoverageFraction::new(889, 1_000).unwrap();
        let breached = {
            let primary = coverage_account(
                "primary",
                coverage_engine(
                    Some(below_floor),
                    Some(CoverageFraction::new(1, 1).unwrap()),
                    Some(gap_6m()),
                    Vec::new(),
                ),
                crate::report::coverage::CoverageFailureTally::default(),
                Vec::new(),
                Vec::new(),
                true,
            );
            let mut threshold = floors_met(0.98, 0.95);
            threshold.met = false;
            threshold.breaches = vec![attempt_breach("primary", below_floor)];
            coverage_report(vec![primary], threshold)
        };
        let message = render_coverage_threshold_message(&breached);
        assert!(message.starts_with("primary attempt coverage"), "{message}");
        let footer = coverage_footer_lines(&breached);
        assert_eq!(
            footer,
            vec!["next: run coverage again once the floor condition changes"]
        );
        assert!(
            !footer[0].contains("primary") && !footer[0].contains("88.9"),
            "the footer repeats no breach text: {footer:?}"
        );

        let met = {
            let primary = coverage_account(
                "primary",
                coverage_engine(
                    Some(CoverageFraction::new(1, 1).unwrap()),
                    Some(CoverageFraction::new(1, 1).unwrap()),
                    Some(gap_6m()),
                    Vec::new(),
                ),
                crate::report::coverage::CoverageFailureTally::default(),
                Vec::new(),
                Vec::new(),
                true,
            );
            coverage_report(vec![primary], floors_met(0.98, 0.95))
        };
        assert_eq!(
            render_coverage_threshold_message(&met),
            "no threshold breach was recorded"
        );
        assert!(coverage_footer_lines(&met).is_empty());

        let zero = {
            let quiet_engine = crate::coverage::CoverageReport {
                expected_opportunities: Some(0),
                policy_covered_span: Some(MonotonicDuration::from_seconds(86_400)),
                attempted_opportunities: 0,
                successful_observations: 0,
                started_without_terminal_result: 0,
                attempt_coverage: None,
                measurement_coverage: None,
                longest_no_attempt_gap: Some(gap_6m()),
                longest_no_observation_gap: None,
                reset_spanning_gaps: Vec::new(),
                most_recent_timer_run: None,
                most_recent_successful_observation: None,
                severe: false,
            };
            let quiet = coverage_account(
                "quiet",
                quiet_engine,
                crate::report::coverage::CoverageFailureTally::default(),
                Vec::new(),
                Vec::new(),
                true,
            );
            coverage_report(vec![quiet], floors_met(0.98, 0.95))
        };
        assert_eq!(
            render_coverage_threshold_message(&zero),
            "no threshold breach was recorded"
        );
        assert!(coverage_footer_lines(&zero).is_empty());
    }
}
