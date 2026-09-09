//! Expected-vs-observed sample opportunities and destructive gaps.
//!
//! The coverage engine reconstructs the sampling opportunities a policy owed over an
//! interval, joins the attempts and observations that actually happened, and reports
//! attempt coverage and measurement coverage as two separate quantities. It is a pure
//! computation over injected inputs: the store layer assembles the records, and this
//! module never reads a table, a clock or a file.
//!
//! May not depend on:
//! - presentation
//! - provider adapters

use crate::domain::time::{MonotonicDuration, UtcTimestamp};

/// A resolved sampling policy in force from `effective_at` onward.
///
/// The ordinary cadence is the one value the denominator reconstruction reads; the
/// other resolved fields (freshness horizon, retry backoff, command budget) do not
/// change how many opportunities a policy owed. The retry backoff string carries
/// the `Retry-After` ceiling (`retry-after-capped-<n>s`) the scheduler honours,
/// plus the authentication-backoff threshold and ceiling
/// (`auth-<threshold>-<cap>s`, aub-x2je), so both postponements read the same
/// caps; a `none` or pre-cap shape means uncapped with no authentication
/// backoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicySnapshot {
    pub effective_at: UtcTimestamp,
    pub ordinary_cadence: MonotonicDuration,
    pub retry_backoff_policy: String,
}

/// The terminal result of one attempt, reduced to the facts coverage reads:
/// when it finished, whether a `Retry-After` postponed the next opportunity,
/// and whether it was an authentication rejection (aub-x2je).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttemptResultRecord {
    pub finished_at: UtcTimestamp,
    pub retry_after: Option<MonotonicDuration>,
    pub is_auth_required: bool,
}

/// One started attempt and its optional terminal result. A start with no result is the
/// collector-interruption state, reported separately from both coverage numbers.
/// `credential_changed` (aub-x2je) marks an attempt whose credential context
/// differs from its predecessor's: the authentication streak resets there,
/// the same rule the scheduler applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttemptRecord {
    pub started_at: UtcTimestamp,
    pub result: Option<AttemptResultRecord>,
    pub credential_changed: bool,
}

/// One successful observation, at the instant it was received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservationRecord {
    pub at: UtcTimestamp,
}

/// A known quota reset instant, read from the provider's reported window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResetRecord {
    pub at: UtcTimestamp,
}

/// A timer-triggered sample run, at the instant it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerRunRecord {
    pub at: UtcTimestamp,
}

/// Everything the coverage engine needs for one account over one interval.
///
/// Each list may arrive in any order; [`compute`] sorts defensively. The interval is
/// half-open `[interval_start, interval_end)`: an attempt or observation exactly at
/// `interval_end` belongs to the next interval, not this one.
#[derive(Debug, Clone)]
pub struct CoverageInputs {
    pub interval_start: UtcTimestamp,
    pub interval_end: UtcTimestamp,
    pub policy_snapshots: Vec<PolicySnapshot>,
    pub attempts: Vec<AttemptRecord>,
    pub observations: Vec<ObservationRecord>,
    pub resets: Vec<ResetRecord>,
    pub timer_runs: Vec<TimerRunRecord>,
}

/// A coverage fraction in `[0, 1]`: the share of expected opportunities that were
/// attempted, or the share of terminal attempts that produced observations.
///
/// Constructed from a numerator and a positive denominator and saturated at 1.0, so a
/// forced or reset-edge attempt that overshoots the reconstructed denominator reads as
/// full coverage rather than as a number above one.
///
/// The constructor is `None` on a zero denominator because a ratio over no denominator
/// is not a number the engine can justify: zero is data in this system (PLAN.md 31),
/// and a substituted zero would be indistinguishable from a real measurement of
/// nothing, reading as every opportunity missed when in fact none was owed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CoverageFraction(f64);

impl CoverageFraction {
    /// The fraction, or `None` when the denominator is zero and no ratio exists.
    pub fn new(numerator: u64, denominator: u64) -> Option<Self> {
        if denominator == 0 {
            return None;
        }
        Some(Self(
            (numerator as f64 / denominator as f64).clamp(0.0, 1.0),
        ))
    }

    /// The fraction as a bare number in `[0, 1]`, for rendering and tests.
    pub fn as_f64(self) -> f64 {
        self.0
    }

    /// The fraction in parts per million, rounded half-up, saturating at the
    /// full `1_000_000`. The named conversion from the fraction the engine
    /// holds to the one unit the rest of this project expresses fractions in,
    /// so a consumer of the JSON contract reads the same unit the quota
    /// readings already carry.
    pub fn as_ppm(self) -> u32 {
        (self.0 * 1_000_000.0).round().clamp(0.0, 1_000_000.0) as u32
    }
}

/// A maximal interval during which no attempt (or no observation) occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    pub start: UtcTimestamp,
    pub end: UtcTimestamp,
}

impl Gap {
    /// The gap's length, never negative.
    pub fn duration(self) -> MonotonicDuration {
        MonotonicDuration::from_nanos(
            (self.end.unix_nanos() - self.start.unix_nanos()).max(0) as u64
        )
    }

    /// True when the gap contains the reset instant, inclusive of both ends.
    pub fn spans(self, reset: UtcTimestamp) -> bool {
        self.start.unix_nanos() <= reset.unix_nanos() && reset.unix_nanos() <= self.end.unix_nanos()
    }
}

/// The coverage report for one account over one interval.
#[derive(Debug, Clone, PartialEq)]
pub struct CoverageReport {
    /// Reconstructed expected opportunities over the covered span. `None` when
    /// no policy snapshot applies anywhere in the interval, which is reported
    /// as `policy_unknown`; the span before the first snapshot owes nothing
    /// because the account did not exist for aub yet.
    pub expected_opportunities: Option<u64>,
    /// The span a policy snapshot covers, from the first `effective_at`
    /// inside or before the interval to its end. `None` exactly when
    /// `expected_opportunities` is `None`. A covered span shorter than the
    /// interval means the account was added part-way through the window.
    pub policy_covered_span: Option<MonotonicDuration>,
    pub attempted_opportunities: u64,
    pub successful_observations: u64,
    /// Started attempts that never acquired a terminal result: collector or process
    /// interruption, kept separate from both coverage numbers.
    pub started_without_terminal_result: u64,
    /// `None` when no policy snapshot fixes the denominator (`policy_unknown`) or
    /// when the reconstructed denominator is zero and no ratio exists.
    pub attempt_coverage: Option<CoverageFraction>,
    /// The conditional measurement coverage: successful observations over terminal
    /// attempts. Interrupted attempts stay out of the denominator, so collector or
    /// process interruption never reads as provider failure; the interruption is its
    /// own counter above. `None` when no attempt reached a terminal state, because a
    /// ratio over no known outcomes is not a number.
    pub measurement_coverage: Option<CoverageFraction>,
    pub longest_no_attempt_gap: Option<Gap>,
    pub longest_no_observation_gap: Option<Gap>,
    /// No-attempt gaps that contain a known quota reset, where the window's peak
    /// consumption may have been lost permanently.
    pub reset_spanning_gaps: Vec<Gap>,
    pub most_recent_timer_run: Option<UtcTimestamp>,
    pub most_recent_successful_observation: Option<UtcTimestamp>,
    /// True when at least one no-attempt gap spans a known reset.
    pub severe: bool,
}

/// A no-attempt interval counts as a sampling hole only when it exceeds the
/// ordinary cadence in force at the reset by more than this tolerance.
/// Shorter intervals are the sampling grid working as designed, with a
/// reset-edge attempt just before the reset and one just after.
const HOLE_TOLERANCE: MonotonicDuration = MonotonicDuration::from_seconds(60);

/// Reset instants this close together are one reset: consecutive observations
/// report the same quota reset a second apart, and each variant must not
/// count on its own.
const RESET_DEDUP_WINDOW: MonotonicDuration = MonotonicDuration::from_seconds(5);

/// A postponement interval: the span a `Retry-After` instruction covers, during which
/// the provider asked not to be called and no opportunity was owed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Postponement {
    start: UtcTimestamp,
    end: UtcTimestamp,
}

/// Computes the coverage report for one account over one interval.
pub fn compute(inputs: &CoverageInputs) -> CoverageReport {
    let start = inputs.interval_start;
    let end = inputs.interval_end;

    let mut snapshots = inputs.policy_snapshots.clone();
    snapshots.sort_by_key(|s| s.effective_at);
    let mut attempts = inputs.attempts.clone();
    attempts.sort_by_key(|a| a.started_at);
    let mut observations = inputs.observations.clone();
    observations.sort_by_key(|o| o.at);
    let mut resets = inputs.resets.clone();
    resets.sort_by_key(|r| r.at);
    let mut timer_runs = inputs.timer_runs.clone();
    timer_runs.sort_by_key(|t| t.at);

    let attempt_times: Vec<UtcTimestamp> = attempts
        .iter()
        .filter(|a| a.started_at >= start && a.started_at < end)
        .map(|a| a.started_at)
        .collect();
    let observation_times: Vec<UtcTimestamp> = observations
        .iter()
        .filter(|o| o.at >= start && o.at < end)
        .map(|o| o.at)
        .collect();

    let postponements = postponements(&attempts, &snapshots);
    let expected_and_covered = expected_opportunities(start, end, &snapshots, &postponements);
    let expected = expected_and_covered.map(|(owed, _)| owed);
    let policy_covered_span = expected_and_covered.map(|(_, covered)| covered);

    let attempted = attempt_times.len() as u64;
    let successful = observation_times.len() as u64;
    let started_without_terminal_result = attempts
        .iter()
        .filter(|a| a.started_at >= start && a.started_at < end && a.result.is_none())
        .count() as u64;

    // A no-attempt or no-observation gap starts no earlier than the policy's own
    // covered span: the head before the account's first snapshot was never a
    // sampling opportunity, so it must not silently inflate a gap presented as a
    // sampling failure. With no snapshot anywhere in the interval the whole span
    // is already reported as `policy_unknown`, and the gap falls back to the raw
    // interval start.
    let gap_start = policy_covered_start(start, end, &snapshots).unwrap_or(start);
    let attempt_gap_times: Vec<UtcTimestamp> = attempt_times
        .iter()
        .copied()
        .filter(|t| *t >= gap_start)
        .collect();
    let observation_gap_times: Vec<UtcTimestamp> = observation_times
        .iter()
        .copied()
        .filter(|t| *t >= gap_start)
        .collect();
    let no_attempt_gaps = gaps(gap_start, end, &attempt_gap_times);
    let no_observation_gaps = gaps(gap_start, end, &observation_gap_times);
    let reset_spanning_gaps = reset_spanning_gaps(&resets, &no_attempt_gaps, &snapshots);
    let severe = !reset_spanning_gaps.is_empty();

    // The measurement denominator is terminal attempts, not started attempts: an
    // attempt that never finished is collector interruption, a different fact from a
    // provider failure, and folding it in would destroy the two-stage distinction.
    let terminal_attempts = attempted.saturating_sub(started_without_terminal_result);

    CoverageReport {
        expected_opportunities: expected,
        policy_covered_span,
        attempted_opportunities: attempted,
        successful_observations: successful,
        started_without_terminal_result,
        attempt_coverage: expected.and_then(|e| CoverageFraction::new(attempted, e)),
        measurement_coverage: CoverageFraction::new(successful, terminal_attempts),
        longest_no_attempt_gap: longest_gap(&no_attempt_gaps),
        longest_no_observation_gap: longest_gap(&no_observation_gaps),
        reset_spanning_gaps,
        most_recent_timer_run: timer_runs
            .iter()
            .rev()
            .find(|t| t.at >= start && t.at < end)
            .map(|t| t.at),
        most_recent_successful_observation: observation_times.last().copied(),
        severe,
    }
}

/// The ordinary cadence in force at `at`, or `None` when no snapshot covers it.
fn cadence_at(snapshots: &[PolicySnapshot], at: UtcTimestamp) -> Option<MonotonicDuration> {
    snapshots
        .iter()
        .rev()
        .find(|s| s.effective_at.unix_nanos() <= at.unix_nanos())
        .map(|s| s.ordinary_cadence)
}

/// The `Retry-After` ceiling in force at `at`, parsed from the
/// `retry_backoff_policy` snapshot string (`retry-after-capped-<n>s`, with
/// an optional ` auth-<threshold>-<cap>s` suffix the authentication backoff
/// adds). `None` means uncapped: the snapshot records `none` or a pre-cap
/// shape.
fn retry_after_cap_at(snapshots: &[PolicySnapshot], at: UtcTimestamp) -> Option<MonotonicDuration> {
    let policy = snapshots
        .iter()
        .rev()
        .find(|s| s.effective_at.unix_nanos() <= at.unix_nanos())?
        .retry_backoff_policy
        .as_str();
    parse_capped_retry_after_policy(policy)
}

/// Parses the `retry-after-capped-<n>s` snapshot string back into its ceiling.
/// Anything else (`none`, a pre-cap backoff shape, an unreadable value) is
/// `None`: uncapped, never a guessed cap. A trailing authentication segment
/// (` auth-<threshold>-<cap>s`) is ignored here and parsed by
/// [`parse_auth_backoff_policy`].
fn parse_capped_retry_after_policy(text: &str) -> Option<MonotonicDuration> {
    let first = text.split_whitespace().next().unwrap_or(text);
    let seconds = first
        .strip_prefix("retry-after-capped-")?
        .strip_suffix('s')?;
    let seconds: u64 = seconds.parse().ok()?;
    Some(MonotonicDuration::from_seconds(seconds))
}

/// The authentication-backoff threshold and ceiling in force at `at`,
/// parsed from the `auth-<threshold>-<cap>s` segment of the
/// `retry_backoff_policy` snapshot string (aub-x2je). `None` means no
/// authentication backoff: the snapshot predates the segment, records
/// `none`, or is unreadable, and the engine then owes every cadence tick.
fn auth_backoff_at(
    snapshots: &[PolicySnapshot],
    at: UtcTimestamp,
) -> Option<(u32, MonotonicDuration)> {
    let policy = snapshots
        .iter()
        .rev()
        .find(|s| s.effective_at.unix_nanos() <= at.unix_nanos())?
        .retry_backoff_policy
        .as_str();
    parse_auth_backoff_policy(policy)
}

/// Parses the `auth-<threshold>-<cap>s` segment of a snapshot string back
/// into its threshold and ceiling. Anything without the segment is `None`.
fn parse_auth_backoff_policy(text: &str) -> Option<(u32, MonotonicDuration)> {
    let segment = text
        .split_whitespace()
        .find(|token| token.starts_with("auth-"))?;
    let rest = segment.strip_prefix("auth-")?.strip_suffix('s')?;
    let (threshold, cap_secs) = rest.split_once('-')?;
    let threshold: u32 = threshold.parse().ok()?;
    if threshold == 0 {
        return None;
    }
    let cap_secs: u64 = cap_secs.parse().ok()?;
    Some((threshold, MonotonicDuration::from_seconds(cap_secs)))
}

/// The postponement intervals owed to persisted `Retry-After` instructions
/// and to authentication-backoff holds (aub-x2je). A retry postponement
/// exists only when the retry delay exceeds the ordinary cadence in force
/// at the result's finish: a shorter delay is absorbed by the ordinary cadence and does
/// not remove an opportunity. The delay is capped by the ceiling the policy snapshot
/// covering the finish records, the same rule the scheduler applies, so a header
/// above the cap postpones only until the cap expires.
///
/// An authentication postponement exists only when the trailing streak of
/// `auth_required` results ending at this attempt reaches the threshold in
/// force at its finish: the hold is the scheduler's doubling delay
/// (`cadence * 2^(streak-threshold+1)`, capped), and only when it exceeds
/// the cadence. A credential change resets the streak first, the same rule
/// the scheduler applies, and an interruption (no terminal result) resets
/// it too. Both families merge into one interval set, so an instant held
/// by both counts once: the two holds share the due instant and the later
/// wins, never their sum.
fn postponements(attempts: &[AttemptRecord], snapshots: &[PolicySnapshot]) -> Vec<Postponement> {
    let mut out = Vec::new();
    let mut auth_streak = 0u32;
    for attempt in attempts {
        let Some(result) = &attempt.result else {
            auth_streak = 0;
            continue;
        };
        if attempt.credential_changed {
            auth_streak = 0;
        }
        if result.is_auth_required {
            auth_streak = auth_streak.saturating_add(1);
        } else {
            auth_streak = 0;
        }
        let Some(cadence) = cadence_at(snapshots, result.finished_at) else {
            continue;
        };
        if let Some(retry_after) = result.retry_after {
            let effective = match retry_after_cap_at(snapshots, result.finished_at) {
                Some(cap) => retry_after.min(cap),
                None => retry_after,
            };
            if effective.as_nanos() > cadence.as_nanos() {
                out.push(Postponement {
                    start: result.finished_at,
                    end: UtcTimestamp::from_unix_nanos(
                        result.finished_at.unix_nanos() + effective.as_nanos() as i64,
                    ),
                });
            }
        }
        if result.is_auth_required
            && let Some((threshold, cap)) = auth_backoff_at(snapshots, result.finished_at)
            && let Some(delay) =
                crate::meter::due::auth_backoff_delay(cadence, threshold, cap, auth_streak, false)
            && delay.as_nanos() > cadence.as_nanos()
        {
            out.push(Postponement {
                start: result.finished_at,
                end: UtcTimestamp::from_unix_nanos(
                    result.finished_at.unix_nanos() + delay.as_nanos() as i64,
                ),
            });
        }
    }
    merge_postponements(out)
}

/// Merges overlapping or touching postponement intervals so the denominator
/// subtraction cannot count one covered instant twice.
fn merge_postponements(mut intervals: Vec<Postponement>) -> Vec<Postponement> {
    intervals.sort_by_key(|p| (p.start.unix_nanos(), p.end.unix_nanos()));
    let mut merged: Vec<Postponement> = Vec::with_capacity(intervals.len());
    for interval in intervals {
        match merged.last_mut() {
            Some(last) if interval.start.unix_nanos() <= last.end.unix_nanos() => {
                last.end = last.end.max(interval.end);
            }
            _ => merged.push(interval),
        }
    }
    merged
}

/// The instant from which a policy snapshot covers `[instant, end)` without
/// interruption: `start` itself when a snapshot is already in force there,
/// otherwise the earliest snapshot that becomes effective inside the
/// interval. `None` when no snapshot applies anywhere in `[start, end)`, the
/// `policy_unknown` case. Shared by the denominator reconstruction and the
/// gap computation, so both agree on where the account's history for aub
/// begins: the span before this instant owes nothing and cannot be a
/// sampling failure, because the account did not exist for aub yet.
fn policy_covered_start(
    start: UtcTimestamp,
    end: UtcTimestamp,
    snapshots: &[PolicySnapshot],
) -> Option<UtcTimestamp> {
    if cadence_at(snapshots, start).is_some() {
        Some(start)
    } else {
        snapshots
            .iter()
            .map(|snapshot| snapshot.effective_at)
            .filter(|effective| *effective > start && *effective < end)
            .min()
    }
}

/// Reconstructs the expected-opportunity denominator over the covered span,
/// from the first snapshot `effective_at` inside or before `[start, end)` to
/// `end`, excluding postponement intervals. `None` when no snapshot applies
/// anywhere in the interval. The span before the first snapshot owes nothing:
/// the account did not exist for aub yet. Slicing at later snapshots and the
/// postponement arithmetic are unchanged.
fn expected_opportunities(
    start: UtcTimestamp,
    end: UtcTimestamp,
    snapshots: &[PolicySnapshot],
    postponements: &[Postponement],
) -> Option<(u64, MonotonicDuration)> {
    let covered_start = policy_covered_start(start, end, snapshots)?;
    let covered_nanos = (end.unix_nanos() - covered_start.unix_nanos()).max(0) as u64;
    let covered_span = MonotonicDuration::from_nanos(covered_nanos);
    let mut boundaries = vec![covered_start, end];
    for snapshot in snapshots {
        let effective = snapshot.effective_at;
        if effective > covered_start && effective < end {
            boundaries.push(effective);
        }
    }
    boundaries.sort();
    boundaries.dedup();

    let mut total = 0u64;
    for pair in boundaries.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let cadence = cadence_at(snapshots, a)?;
        let duration = (b.unix_nanos() - a.unix_nanos()).max(0) as u64;
        let overlap = postponement_overlap(a, b, postponements);
        total += duration.saturating_sub(overlap) / cadence.as_nanos();
    }
    Some((total, covered_span))
}

/// The total overlap of `[a, b]` with every postponement interval, in nanoseconds.
fn postponement_overlap(a: UtcTimestamp, b: UtcTimestamp, postponements: &[Postponement]) -> u64 {
    let mut total = 0u64;
    for p in postponements {
        let overlap_start = a.unix_nanos().max(p.start.unix_nanos());
        let overlap_end = b.unix_nanos().min(p.end.unix_nanos());
        if overlap_end > overlap_start {
            total += (overlap_end - overlap_start) as u64;
        }
    }
    total
}

/// Every gap between consecutive instants, including the interval start to the first
/// instant and the last instant to the interval end. With no instants, the single gap
/// is the whole interval.
fn gaps(start: UtcTimestamp, end: UtcTimestamp, times: &[UtcTimestamp]) -> Vec<Gap> {
    let mut out = Vec::with_capacity(times.len() + 1);
    let mut prev = start;
    for &t in times {
        out.push(Gap {
            start: prev,
            end: t,
        });
        prev = t;
    }
    out.push(Gap { start: prev, end });
    out
}

/// The longest gap, or `None` when the list is empty.
fn longest_gap(gaps: &[Gap]) -> Option<Gap> {
    gaps.iter().max_by_key(|g| g.duration()).copied()
}

/// The no-attempt gaps that contain a known quota reset inside a real sampling
/// hole: the gap's length exceeds the ordinary cadence in force at the reset
/// by more than the tolerance. A reset with edge attempts seconds away sits
/// in a short gap and is not counted. The `longest gap` columns keep the
/// unthresholded definition, which is the right one for them.
fn reset_spanning_gaps(
    resets: &[ResetRecord],
    no_attempt_gaps: &[Gap],
    snapshots: &[PolicySnapshot],
) -> Vec<Gap> {
    let resets = dedup_resets(resets);
    no_attempt_gaps
        .iter()
        .filter(|g| {
            resets
                .iter()
                .any(|r| g.spans(r.at) && is_sampling_hole(g, r.at, snapshots))
        })
        .copied()
        .collect()
}

/// True when the gap is longer than the ordinary cadence in force at `at`
/// plus the tolerance. An unknown cadence is not a hole: without a policy
/// the engine cannot say the interval is longer than ordinary, and a number
/// it cannot justify is not printed.
fn is_sampling_hole(gap: &Gap, at: UtcTimestamp, snapshots: &[PolicySnapshot]) -> bool {
    match cadence_at(snapshots, at) {
        Some(cadence) => gap.duration().as_nanos() > cadence.as_nanos() + HOLE_TOLERANCE.as_nanos(),
        None => false,
    }
}

/// Collapse reset instants within the dedup window of each other into one,
/// keeping the earliest of each cluster. The input may arrive in any order;
/// the output is sorted.
fn dedup_resets(resets: &[ResetRecord]) -> Vec<ResetRecord> {
    let mut sorted = resets.to_vec();
    sorted.sort_by_key(|r| r.at);
    let mut out: Vec<ResetRecord> = Vec::with_capacity(sorted.len());
    for reset in sorted {
        let duplicate = match out.last() {
            Some(last) => {
                (reset.at.unix_nanos() - last.at.unix_nanos()) as u64
                    <= RESET_DEDUP_WINDOW.as_nanos()
            }
            None => false,
        };
        if !duplicate {
            out.push(reset);
        }
    }
    out
}

/// Renders the report as plain text, one fact per line. A no-attempt gap is reported
/// as a duration and nothing else: the engine has no evidence of a cause and does not
/// invent one.
pub fn render(report: &CoverageReport) -> String {
    let mut lines = Vec::new();
    match report.expected_opportunities {
        Some(n) => lines.push(format!("expected opportunities: {n}")),
        None => lines.push("expected opportunities: policy unknown".to_string()),
    }
    lines.push(format!(
        "attempted opportunities: {}",
        report.attempted_opportunities
    ));
    lines.push(format!(
        "successful observations: {}",
        report.successful_observations
    ));
    lines.push(format!(
        "started without terminal result: {}",
        report.started_without_terminal_result
    ));
    match report.attempt_coverage {
        Some(fraction) => lines.push(format!("attempt coverage: {}", render_fraction(fraction))),
        // A zero denominator is not a zero coverage: nothing was owed, so there is no
        // ratio to print (PLAN.md 31).
        None if report.expected_opportunities == Some(0) => {
            lines.push("attempt coverage: nothing owed".to_string());
        }
        None => lines.push("attempt coverage: policy unknown".to_string()),
    }
    match report.measurement_coverage {
        Some(fraction) => {
            lines.push(format!(
                "measurement coverage: {}",
                render_fraction(fraction)
            ));
        }
        None => lines.push("measurement coverage: no terminal attempts".to_string()),
    }
    lines.push(format!(
        "longest no-attempt gap: {}",
        render_gap(report.longest_no_attempt_gap)
    ));
    lines.push(format!(
        "longest no-observation gap: {}",
        render_gap(report.longest_no_observation_gap)
    ));
    lines.push(format!(
        "gaps spanning a known quota reset: {}",
        report.reset_spanning_gaps.len()
    ));
    lines.push(format!(
        "most recent timer-triggered run: {}",
        render_timestamp(report.most_recent_timer_run)
    ));
    lines.push(format!(
        "most recent successful observation: {}",
        render_timestamp(report.most_recent_successful_observation)
    ));
    lines.push(format!(
        "severe: {}",
        if report.severe { "yes" } else { "no" }
    ));
    lines.join("\n")
}

fn render_fraction(fraction: CoverageFraction) -> String {
    format!("{:.1}%", fraction.as_f64() * 100.0)
}

fn render_gap(gap: Option<Gap>) -> String {
    match gap {
        Some(gap) => render_duration(gap.duration()),
        None => "none".to_string(),
    }
}

fn render_timestamp(timestamp: Option<UtcTimestamp>) -> String {
    match timestamp {
        Some(t) => t.unix_nanos().to_string(),
        None => "none".to_string(),
    }
}

fn render_duration(duration: MonotonicDuration) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const SECOND: i64 = 1_000_000_000;

    fn ts(seconds: i64) -> UtcTimestamp {
        UtcTimestamp::from_unix_nanos(seconds * SECOND)
    }

    fn cadence(seconds: u64) -> MonotonicDuration {
        MonotonicDuration::from_seconds(seconds)
    }

    fn snapshot(effective_secs: i64, cadence_secs: u64) -> PolicySnapshot {
        PolicySnapshot {
            effective_at: ts(effective_secs),
            ordinary_cadence: cadence(cadence_secs),
            retry_backoff_policy: "none".to_string(),
        }
    }

    fn snapshot_with_capped_retry_after(
        effective_secs: i64,
        cadence_secs: u64,
        cap_secs: u64,
    ) -> PolicySnapshot {
        PolicySnapshot {
            effective_at: ts(effective_secs),
            ordinary_cadence: cadence(cadence_secs),
            retry_backoff_policy: format!("retry-after-capped-{cap_secs}s"),
        }
    }

    fn retry_after_result(finished_secs: i64, retry_after_secs: u64) -> AttemptResultRecord {
        AttemptResultRecord {
            finished_at: ts(finished_secs),
            retry_after: Some(cadence(retry_after_secs)),
            is_auth_required: false,
        }
    }

    fn auth_result(finished_secs: i64) -> AttemptResultRecord {
        AttemptResultRecord {
            finished_at: ts(finished_secs),
            retry_after: None,
            is_auth_required: true,
        }
    }

    fn snapshot_with_auth_backoff(
        effective_secs: i64,
        cadence_secs: u64,
        retry_cap_secs: u64,
        threshold: u32,
        auth_cap_secs: u64,
    ) -> PolicySnapshot {
        PolicySnapshot {
            effective_at: ts(effective_secs),
            ordinary_cadence: cadence(cadence_secs),
            retry_backoff_policy: format!(
                "retry-after-capped-{retry_cap_secs}s auth-{threshold}-{auth_cap_secs}s"
            ),
        }
    }

    fn attempt(started_secs: i64, result: Option<AttemptResultRecord>) -> AttemptRecord {
        AttemptRecord {
            started_at: ts(started_secs),
            result,
            credential_changed: false,
        }
    }

    fn attempt_with_change(
        started_secs: i64,
        result: Option<AttemptResultRecord>,
        credential_changed: bool,
    ) -> AttemptRecord {
        AttemptRecord {
            started_at: ts(started_secs),
            result,
            credential_changed,
        }
    }

    fn success_result(finished_secs: i64) -> AttemptResultRecord {
        AttemptResultRecord {
            finished_at: ts(finished_secs),
            retry_after: None,
            is_auth_required: false,
        }
    }

    fn inputs(
        start_secs: i64,
        end_secs: i64,
        snapshots: Vec<PolicySnapshot>,
        attempts: Vec<AttemptRecord>,
        observations: Vec<ObservationRecord>,
    ) -> CoverageInputs {
        CoverageInputs {
            interval_start: ts(start_secs),
            interval_end: ts(end_secs),
            policy_snapshots: snapshots,
            attempts,
            observations,
            resets: Vec::new(),
            timer_runs: Vec::new(),
        }
    }

    /// An interval with no policy snapshot is reported as `policy_unknown`, never
    /// evaluated against a later configuration.
    #[test]
    fn an_interval_with_no_snapshot_is_policy_unknown() {
        let report = compute(&inputs(
            0,
            3_600,
            Vec::new(),
            vec![attempt(300, Some(success_result(300)))],
            vec![ObservationRecord { at: ts(300) }],
        ));
        assert_eq!(report.expected_opportunities, None);
        assert_eq!(report.policy_covered_span, None);
        assert_eq!(report.attempt_coverage, None);
        let rendered = render(&report);
        assert!(
            rendered.contains("policy unknown"),
            "the report must name the unknown policy: {rendered}"
        );
    }

    /// An interval whose only snapshot becomes effective mid-interval is
    /// covered from that snapshot: the head before it owes nothing because
    /// the account did not exist for aub yet. The head must also not
    /// inflate the longest no-attempt gap: with no attempts anywhere, the
    /// one gap the engine can justify runs from the snapshot to the
    /// interval end (1,800 s), never from the raw interval start (3,600 s).
    #[test]
    fn an_interval_with_a_head_before_the_first_snapshot_covers_from_the_snapshot() {
        let report = compute(&inputs(
            0,
            3_600,
            vec![snapshot(1_800, 300)],
            vec![],
            vec![],
        ));
        assert_eq!(report.expected_opportunities, Some(6));
        assert_eq!(
            report.policy_covered_span,
            Some(MonotonicDuration::from_seconds(1_800))
        );
        assert_eq!(report.attempt_coverage, CoverageFraction::new(0, 6));
        assert!(
            !render(&report).contains("policy unknown"),
            "a covered tail must not read as unknown: {}",
            render(&report)
        );
        assert_eq!(
            report.longest_no_attempt_gap,
            Some(Gap {
                start: ts(1_800),
                end: ts(3_600)
            }),
            "the gap must start at the snapshot, not at the raw interval start"
        );
        assert_eq!(
            report.longest_no_attempt_gap.unwrap().duration(),
            MonotonicDuration::from_seconds(1_800)
        );
    }

    /// A 24-hour window whose only snapshot is 12 hours old owes 144
    /// opportunities at a 300 s cadence, all of them attempted.
    #[test]
    fn a_window_with_a_snapshot_twelve_hours_old_covers_the_tail() {
        let attempts: Vec<AttemptRecord> = (0..144)
            .map(|i| {
                let at = 43_200 + i * 300;
                attempt(at, Some(success_result(at)))
            })
            .collect();
        let report = compute(&inputs(
            0,
            86_400,
            vec![snapshot(43_200, 300)],
            attempts,
            vec![],
        ));
        assert_eq!(report.expected_opportunities, Some(144));
        assert_eq!(
            report.policy_covered_span,
            Some(MonotonicDuration::from_seconds(43_200))
        );
        assert_eq!(
            report.attempt_coverage.unwrap().as_f64(),
            1.0,
            "144 attempts against 144 owed must read 100%"
        );
        assert_eq!(
            report.longest_no_attempt_gap.unwrap().duration(),
            MonotonicDuration::from_seconds(300),
            "every gap between two attempts on the grid is exactly one cadence, \
             including the tail to the interval end; there is no larger gap to find"
        );
    }

    /// Started attempts with no terminal result are counted separately from both
    /// coverage numbers.
    #[test]
    fn started_attempts_without_a_result_are_counted_separately() {
        let report = compute(&inputs(
            0,
            3_600,
            vec![snapshot(0, 300)],
            vec![
                attempt(300, Some(success_result(300))),
                attempt(600, None),
                attempt(900, Some(success_result(900))),
            ],
            vec![
                ObservationRecord { at: ts(300) },
                ObservationRecord { at: ts(900) },
            ],
        ));
        assert_eq!(report.attempted_opportunities, 3);
        assert_eq!(report.started_without_terminal_result, 1);
        assert_eq!(report.successful_observations, 2);
        // The interrupted attempt is excluded from the measurement denominator, so
        // collector interruption never reads as provider failure: the two terminal
        // attempts both produced observations. A naive successful/started ratio would
        // read 2/3 here.
        assert_eq!(report.measurement_coverage, CoverageFraction::new(2, 2));
    }

    /// A measurement ratio over no terminal attempts is undefined, never a substituted
    /// zero: zero is data in this system (PLAN.md 31), and a zero here would read as
    /// every attempt having failed when in fact none completed.
    #[test]
    fn measurement_coverage_without_terminal_attempts_is_undefined_not_zero() {
        let report = compute(&inputs(
            0,
            3_600,
            vec![snapshot(0, 300)],
            vec![attempt(300, None)],
            vec![],
        ));
        assert_eq!(report.started_without_terminal_result, 1);
        assert_eq!(report.measurement_coverage, None);
        let rendered = render(&report);
        assert!(
            rendered.contains("measurement coverage: no terminal attempts"),
            "the report must name the missing denominator: {rendered}"
        );
    }

    /// An interval shorter than the cadence owes nothing, and a ratio over a zero
    /// denominator is undefined rather than a substituted zero.
    #[test]
    fn an_interval_shorter_than_the_cadence_owes_nothing() {
        let report = compute(&inputs(0, 100, vec![snapshot(0, 300)], vec![], vec![]));
        assert_eq!(report.expected_opportunities, Some(0));
        assert_eq!(report.attempt_coverage, None);
        assert_eq!(report.measurement_coverage, None);
        let rendered = render(&report);
        assert!(
            rendered.contains("attempt coverage: nothing owed"),
            "the report must name the empty denominator: {rendered}"
        );
    }

    /// A no-attempt gap spanning a known reset marks the report severe.
    #[test]
    fn a_gap_spanning_a_reset_marks_the_report_severe() {
        let mut report_inputs = inputs(
            0,
            3_600,
            vec![snapshot(0, 300)],
            vec![attempt(300, Some(success_result(300)))],
            vec![ObservationRecord { at: ts(300) }],
        );
        report_inputs.resets = vec![ResetRecord { at: ts(2_000) }];
        let report = compute(&report_inputs);
        assert!(report.severe, "a reset-spanning gap must be severe");
        assert_eq!(report.reset_spanning_gaps.len(), 1);
    }

    /// A reset with a reset-edge attempt 120 s before and one 4 s after, at a
    /// 300 s cadence, is not counted: the containing gap is shorter than the
    /// cadence plus tolerance, so the grid worked as designed.
    #[test]
    fn a_reset_with_edge_attempts_either_side_is_not_unobserved() {
        let reset = 1_000;
        let mut report_inputs = inputs(
            0,
            3_600,
            vec![snapshot(0, 300)],
            vec![
                attempt(reset - 120, Some(success_result(reset - 120))),
                attempt(reset + 4, Some(success_result(reset + 4))),
            ],
            vec![],
        );
        report_inputs.resets = vec![ResetRecord { at: ts(reset) }];
        let report = compute(&report_inputs);
        assert!(
            report.reset_spanning_gaps.is_empty(),
            "a reset between edge attempts must not count: {:?}",
            report.reset_spanning_gaps
        );
        assert!(!report.severe, "no hole means not severe");
    }

    /// A reset inside a 45-minute interval with no attempt is counted: the
    /// containing gap far exceeds the 300 s cadence plus tolerance.
    #[test]
    fn a_reset_inside_a_45_minute_silence_is_unobserved() {
        let mut report_inputs = inputs(
            0,
            3_600,
            vec![snapshot(0, 300)],
            vec![
                attempt(300, Some(success_result(300))),
                attempt(3_000, Some(success_result(3_000))),
            ],
            vec![],
        );
        report_inputs.resets = vec![ResetRecord { at: ts(1_500) }];
        let report = compute(&report_inputs);
        assert_eq!(report.reset_spanning_gaps.len(), 1);
        assert!(report.severe, "a reset inside a hole must be severe");
        assert_eq!(
            report.longest_no_attempt_gap,
            Some(Gap {
                start: ts(300),
                end: ts(3_000)
            }),
            "the longest gap is between the two attempts that bracket the hole"
        );
        assert_eq!(
            report.longest_no_attempt_gap.unwrap().duration(),
            MonotonicDuration::from_seconds(2_700)
        );
    }

    /// Two reset instants one second apart count as one reset: consecutive
    /// observations report the same quota reset twice. The variants straddle
    /// an attempt here, so without dedup each would claim its own long gap.
    #[test]
    fn two_reset_instants_one_second_apart_count_as_one_reset() {
        let mut report_inputs = inputs(
            0,
            7_200,
            vec![snapshot(0, 300)],
            vec![
                attempt(0, Some(success_result(0))),
                attempt(1_000, Some(success_result(1_000))),
                attempt(4_600, Some(success_result(4_600))),
            ],
            vec![],
        );
        report_inputs.resets = vec![ResetRecord { at: ts(999) }, ResetRecord { at: ts(1_000) }];
        let report = compute(&report_inputs);
        assert_eq!(
            report.reset_spanning_gaps.len(),
            1,
            "one logical reset must count once: {:?}",
            report.reset_spanning_gaps
        );
    }

    /// A cadence change mid-interval produces a denominator that follows the
    /// historical policy, not the current configuration.
    #[test]
    fn a_cadence_change_mid_interval_follows_the_historical_policy() {
        let report = compute(&inputs(
            0,
            3_600,
            vec![snapshot(0, 300), snapshot(1_800, 900)],
            Vec::new(),
            Vec::new(),
        ));
        // 6 opportunities at 5 minutes, then 2 at 15 minutes.
        assert_eq!(report.expected_opportunities, Some(8));
    }

    /// The Retry-After denominator case: a persisted postponement longer than the
    /// cadence removes its interval from the expected opportunities.
    #[test]
    fn a_retry_after_postponement_reduces_the_denominator() {
        let without = compute(&inputs(
            0,
            3_600,
            vec![snapshot(0, 300)],
            vec![attempt(1_000, Some(success_result(1_000)))],
            Vec::new(),
        ));
        let with = compute(&inputs(
            0,
            3_600,
            vec![snapshot(0, 300)],
            vec![attempt(
                1_000,
                Some(AttemptResultRecord {
                    finished_at: ts(1_000),
                    retry_after: Some(cadence(600)),
                    is_auth_required: false,
                }),
            )],
            Vec::new(),
        ));
        assert_eq!(without.expected_opportunities, Some(12));
        assert_eq!(with.expected_opportunities, Some(10));
    }

    /// A Retry-After shorter than the cadence is absorbed by the ordinary cadence and
    /// does not remove an opportunity.
    #[test]
    fn a_retry_after_shorter_than_the_cadence_does_not_reduce_the_denominator() {
        let report = compute(&inputs(
            0,
            3_600,
            vec![snapshot(0, 300)],
            vec![attempt(
                1_000,
                Some(AttemptResultRecord {
                    finished_at: ts(1_000),
                    retry_after: Some(cadence(30)),
                    is_auth_required: false,
                }),
            )],
            Vec::new(),
        ));
        assert_eq!(report.expected_opportunities, Some(12));
    }

    /// A Retry-After above the policy cap postpones only until the cap expires,
    /// the same rule the scheduler applies: a 7200 s header under a 3600 s cap
    /// ends at finish + 3600 s, not at finish + 7200 s.
    #[test]
    fn a_retry_after_above_the_cap_ends_at_finish_plus_the_cap() {
        let snapshots = vec![snapshot_with_capped_retry_after(0, 300, 3600)];
        let attempts = vec![attempt(1_000, Some(retry_after_result(1_000, 7200)))];
        let postponed = postponements(&attempts, &snapshots);
        assert_eq!(postponed.len(), 1);
        assert_eq!(postponed[0].start, ts(1_000));
        assert_eq!(postponed[0].end, ts(1_000 + 3600));
    }

    /// The paired case below the cap: an 1800 s header under a 3600 s cap
    /// passes through whole and ends at finish + 1800 s. An implementation
    /// that clamped every postponement to the cap would still pass the test
    /// above and fail this one.
    #[test]
    fn a_retry_after_below_the_cap_ends_at_finish_plus_the_header() {
        let snapshots = vec![snapshot_with_capped_retry_after(0, 300, 3600)];
        let attempts = vec![attempt(1_000, Some(retry_after_result(1_000, 1800)))];
        let postponed = postponements(&attempts, &snapshots);
        assert_eq!(postponed.len(), 1);
        assert_eq!(postponed[0].start, ts(1_000));
        assert_eq!(postponed[0].end, ts(1_000 + 1800));
    }

    /// A snapshot recording no cap lets a long header through whole: the `none`
    /// and pre-cap shapes predate the ceiling, so the postponement keeps the
    /// verbatim delay rather than guessing a cap.
    #[test]
    fn a_snapshot_without_a_cap_passes_a_long_header_through_whole() {
        for policy in ["none", "exponential-3", "exponential-2-250ms"] {
            let snapshots = vec![PolicySnapshot {
                effective_at: ts(0),
                ordinary_cadence: cadence(300),
                retry_backoff_policy: policy.to_string(),
            }];
            let attempts = vec![attempt(1_000, Some(retry_after_result(1_000, 7200)))];
            let postponed = postponements(&attempts, &snapshots);
            assert_eq!(postponed.len(), 1, "policy {policy:?} must stay uncapped");
            assert_eq!(postponed[0].end, ts(1_000 + 7200), "policy {policy:?}");
        }
    }

    /// aub-x2je: an old snapshot without the auth segment owes every tick:
    /// three consecutive rejections under `retry-after-capped-3600s` alone
    /// postpone nothing, because the sampler that wrote those rows never
    /// backed off. The paired positive below differs only in the snapshot
    /// carrying the segment.
    #[test]
    fn auth_streak_without_an_auth_segment_in_the_snapshot_postpones_nothing() {
        let snapshots = vec![snapshot_with_capped_retry_after(0, 300, 3600)];
        let attempts = vec![
            attempt(0, Some(auth_result(0))),
            attempt(300, Some(auth_result(300))),
            attempt(600, Some(auth_result(600))),
        ];
        assert!(postponements(&attempts, &snapshots).is_empty());
    }

    /// aub-x2je: three consecutive rejections under a snapshot carrying
    /// `auth-3-21600s` postpone the third's finish by twice the cadence
    /// (600 s at a 300 s cadence), so the denominator loses those
    /// opportunities instead of counting them missed.
    #[test]
    fn auth_streak_at_threshold_postpones_twice_the_cadence() {
        let snapshots = vec![snapshot_with_auth_backoff(0, 300, 3600, 3, 21_600)];
        let attempts = vec![
            attempt(0, Some(auth_result(0))),
            attempt(300, Some(auth_result(300))),
            attempt(600, Some(auth_result(600))),
        ];
        let postponed = postponements(&attempts, &snapshots);
        assert_eq!(postponed.len(), 1);
        assert_eq!(postponed[0].start, ts(600));
        assert_eq!(postponed[0].end, ts(1_200));
    }

    /// aub-x2je: below the threshold nothing postpones: two rejections at a
    /// threshold of three leave the denominator whole.
    #[test]
    fn auth_streak_below_threshold_postpones_nothing() {
        let snapshots = vec![snapshot_with_auth_backoff(0, 300, 3600, 3, 21_600)];
        let attempts = vec![
            attempt(0, Some(auth_result(0))),
            attempt(300, Some(auth_result(300))),
        ];
        assert!(postponements(&attempts, &snapshots).is_empty());
    }

    /// aub-x2je: a credential change resets the streak, so the third
    /// rejection after a change (streak one under the new credential)
    /// postpones nothing. The near-identical negative to the threshold
    /// positive above, differing only in the change flag on the last
    /// attempt.
    #[test]
    fn auth_streak_after_a_credential_change_postpones_nothing() {
        let snapshots = vec![snapshot_with_auth_backoff(0, 300, 3600, 3, 21_600)];
        let attempts = vec![
            attempt(0, Some(auth_result(0))),
            attempt(300, Some(auth_result(300))),
            attempt_with_change(600, Some(auth_result(600)), true),
        ];
        assert!(postponements(&attempts, &snapshots).is_empty());
    }

    /// aub-x2je: a success breaks the streak, so auth-success-auth never
    /// reaches a threshold of three and postpones nothing.
    #[test]
    fn auth_streak_broken_by_a_success_postpones_nothing() {
        let snapshots = vec![snapshot_with_auth_backoff(0, 300, 3600, 3, 21_600)];
        let attempts = vec![
            attempt(0, Some(auth_result(0))),
            attempt(300, Some(success_result(300))),
            attempt(600, Some(auth_result(600))),
        ];
        assert!(postponements(&attempts, &snapshots).is_empty());
    }

    /// The report names the most recent timer-triggered run and the most recent
    /// successful observation inside the interval, each taken as the latest of its
    /// records regardless of the order the caller supplied them in.
    #[test]
    fn the_report_names_the_most_recent_timer_run_and_observation() {
        let mut report_inputs = inputs(
            0,
            3_600,
            vec![snapshot(0, 300)],
            vec![attempt(300, Some(success_result(300)))],
            vec![
                ObservationRecord { at: ts(900) },
                ObservationRecord { at: ts(300) },
            ],
        );
        report_inputs.timer_runs = vec![
            TimerRunRecord { at: ts(1_200) },
            TimerRunRecord { at: ts(600) },
        ];
        let report = compute(&report_inputs);
        assert_eq!(report.most_recent_timer_run, Some(ts(1_200)));
        assert_eq!(report.most_recent_successful_observation, Some(ts(900)));
        let rendered = render(&report);
        assert!(
            rendered.contains("most recent timer-triggered run: 1200000000000"),
            "the report must carry the latest timer run: {rendered}"
        );
    }

    /// The rendered text of a no-attempt gap states no cause: the engine has no
    /// evidence of one and does not invent it.
    #[test]
    fn the_rendered_text_states_no_cause_for_a_no_attempt_gap() {
        // A simulated sleep: one attempt, then nothing for two hours.
        let report = compute(&inputs(
            0,
            7_200,
            vec![snapshot(0, 300)],
            vec![attempt(0, Some(success_result(0)))],
            vec![ObservationRecord { at: ts(0) }],
        ));
        let rendered = render(&report);
        assert!(
            rendered.contains("longest no-attempt gap: 2h"),
            "the gap must be reported as a duration: {rendered}"
        );
        for cause in [
            "sleep",
            "scheduler",
            "laptop",
            "died",
            "disabled",
            "unavailable",
        ] {
            assert!(
                !rendered.to_lowercase().contains(cause),
                "the report must not state a cause ({cause}): {rendered}"
            );
        }
    }

    proptest! {
        /// Over generated histories, successful observations never exceed terminal
        /// attempts, terminal attempts never exceed started attempts, every ratio
        /// stays within 0 to 1, and the conditional measurement ratio exists exactly
        /// when a terminal attempt exists. No ordering is imposed between attempt
        /// coverage and conditional measurement coverage.
        #[test]
        fn prop_counts_and_ratios_stay_within_bounds(
            cadence_secs in 1u64..1000u64,
            periods in 1u64..20u64,
            attempt_seconds in proptest::collection::vec(0u64..60_000u64, 0..30),
            result_flags in proptest::collection::vec(proptest::bool::ANY, 0..30),
            observation_flags in proptest::collection::vec(proptest::bool::ANY, 0..30),
        ) {
            let start = ts(0);
            let end_seconds = cadence_secs * periods;
            let end = ts(end_seconds as i64);
            let snapshots = vec![snapshot(0, cadence_secs)];

            let n = attempt_seconds
                .len()
                .min(result_flags.len())
                .min(observation_flags.len());
            let mut attempts = Vec::new();
            let mut observations = Vec::new();
            for i in 0..n {
                let started_at = ts((attempt_seconds[i] % end_seconds) as i64);
                let has_result = result_flags[i];
                let has_observation = observation_flags[i] && has_result;
                let result = has_result.then_some(AttemptResultRecord {
                    finished_at: started_at,
                    retry_after: None,
                    is_auth_required: false,
                });
                attempts.push(AttemptRecord {
                    started_at,
                    result,
                    credential_changed: false,
                });
                if has_observation {
                    observations.push(ObservationRecord { at: started_at });
                }
            }

            let report = compute(&CoverageInputs {
                interval_start: start,
                interval_end: end,
                policy_snapshots: snapshots,
                attempts,
                observations,
                resets: Vec::new(),
                timer_runs: Vec::new(),
            });

            let terminal = report.attempted_opportunities - report.started_without_terminal_result;
            prop_assert!(report.successful_observations <= terminal);
            prop_assert!(terminal <= report.attempted_opportunities);
            prop_assert_eq!(report.measurement_coverage.is_some(), terminal > 0);
            if let Some(measurement) = report.measurement_coverage {
                prop_assert!((0.0..=1.0).contains(&measurement.as_f64()));
            }
            // The generated policy always fixes a positive denominator, so the attempt
            // ratio exists; a policy-unknown or zero-denominator interval is the unit
            // tests' subject, not this property's.
            prop_assert!(report.attempt_coverage.is_some());
            if let Some(attempt_coverage) = report.attempt_coverage {
                prop_assert!((0.0..=1.0).contains(&attempt_coverage.as_f64()));
            }
        }
    }

    /// Retained hand-picked regression for the property: a fixed history with an
    /// interrupted attempt and a failed attempt keeps every bound intact.
    #[test]
    fn counts_and_ratios_stay_within_bounds_hand_picked() {
        let report = compute(&inputs(
            0,
            3_600,
            vec![snapshot(0, 300)],
            vec![
                attempt(300, Some(success_result(300))),
                attempt(600, None),
                attempt(900, Some(success_result(900))),
            ],
            vec![ObservationRecord { at: ts(300) }],
        ));
        let terminal = report.attempted_opportunities - report.started_without_terminal_result;
        assert_eq!(terminal, 2);
        assert!(report.successful_observations <= terminal);
        assert!(terminal <= report.attempted_opportunities);
        assert_eq!(report.measurement_coverage, CoverageFraction::new(1, 2));
        assert_eq!(report.attempt_coverage, CoverageFraction::new(3, 12));
    }
}
