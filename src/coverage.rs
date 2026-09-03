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
/// change how many opportunities a policy owed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicySnapshot {
    pub effective_at: UtcTimestamp,
    pub ordinary_cadence: MonotonicDuration,
}

/// The terminal result of one attempt, reduced to the two facts coverage reads: when
/// it finished, and whether a `Retry-After` postponed the next opportunity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttemptResultRecord {
    pub finished_at: UtcTimestamp,
    pub retry_after: Option<MonotonicDuration>,
}

/// One started attempt and its optional terminal result. A start with no result is the
/// collector-interruption state, reported separately from both coverage numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttemptRecord {
    pub started_at: UtcTimestamp,
    pub result: Option<AttemptResultRecord>,
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
    /// Reconstructed expected opportunities. `None` when any sub-interval has no
    /// policy snapshot in force, which is reported as `policy_unknown` rather than
    /// evaluated against a later configuration.
    pub expected_opportunities: Option<u64>,
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
    let expected = expected_opportunities(start, end, &snapshots, &postponements);

    let attempted = attempt_times.len() as u64;
    let successful = observation_times.len() as u64;
    let started_without_terminal_result = attempts
        .iter()
        .filter(|a| a.started_at >= start && a.started_at < end && a.result.is_none())
        .count() as u64;

    let no_attempt_gaps = gaps(start, end, &attempt_times);
    let no_observation_gaps = gaps(start, end, &observation_times);
    let reset_spanning_gaps = reset_spanning_gaps(&resets, &no_attempt_gaps);
    let severe = !reset_spanning_gaps.is_empty();

    // The measurement denominator is terminal attempts, not started attempts: an
    // attempt that never finished is collector interruption, a different fact from a
    // provider failure, and folding it in would destroy the two-stage distinction.
    let terminal_attempts = attempted.saturating_sub(started_without_terminal_result);

    CoverageReport {
        expected_opportunities: expected,
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

/// The postponement intervals owed to persisted `Retry-After` instructions. A
/// postponement exists only when the retry delay exceeds the ordinary cadence in force
/// at the result's finish: a shorter delay is absorbed by the ordinary cadence and does
/// not remove an opportunity.
fn postponements(attempts: &[AttemptRecord], snapshots: &[PolicySnapshot]) -> Vec<Postponement> {
    let mut out = Vec::new();
    for attempt in attempts {
        let Some(result) = &attempt.result else {
            continue;
        };
        let Some(retry_after) = result.retry_after else {
            continue;
        };
        let Some(cadence) = cadence_at(snapshots, result.finished_at) else {
            continue;
        };
        if retry_after.as_nanos() <= cadence.as_nanos() {
            continue;
        }
        out.push(Postponement {
            start: result.finished_at,
            end: UtcTimestamp::from_unix_nanos(
                result.finished_at.unix_nanos() + retry_after.as_nanos() as i64,
            ),
        });
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

/// Reconstructs the expected-opportunity denominator from the policy snapshots in
/// force over `[start, end]`, excluding postponement intervals. `None` when any
/// sub-interval has no snapshot in force.
fn expected_opportunities(
    start: UtcTimestamp,
    end: UtcTimestamp,
    snapshots: &[PolicySnapshot],
    postponements: &[Postponement],
) -> Option<u64> {
    let mut boundaries = vec![start, end];
    for snapshot in snapshots {
        let effective = snapshot.effective_at;
        if effective > start && effective < end {
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
    Some(total)
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

/// The no-attempt gaps that contain a known quota reset.
fn reset_spanning_gaps(resets: &[ResetRecord], no_attempt_gaps: &[Gap]) -> Vec<Gap> {
    no_attempt_gaps
        .iter()
        .filter(|g| resets.iter().any(|r| g.spans(r.at)))
        .copied()
        .collect()
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
        }
    }

    fn attempt(started_secs: i64, result: Option<AttemptResultRecord>) -> AttemptRecord {
        AttemptRecord {
            started_at: ts(started_secs),
            result,
        }
    }

    fn success_result(finished_secs: i64) -> AttemptResultRecord {
        AttemptResultRecord {
            finished_at: ts(finished_secs),
            retry_after: None,
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
        assert_eq!(report.attempt_coverage, None);
        let rendered = render(&report);
        assert!(
            rendered.contains("policy unknown"),
            "the report must name the unknown policy: {rendered}"
        );
    }

    /// An interval whose only snapshot becomes effective mid-interval is also
    /// `policy_unknown`: the head of the interval had no policy in force, and the
    /// engine refuses to evaluate it against the later configuration.
    #[test]
    fn an_interval_with_a_head_before_the_first_snapshot_is_policy_unknown() {
        let report = compute(&inputs(
            0,
            3_600,
            vec![snapshot(1_800, 300)],
            vec![],
            vec![],
        ));
        assert_eq!(report.expected_opportunities, None);
        assert_eq!(report.attempt_coverage, None);
        assert!(
            render(&report).contains("policy unknown"),
            "the report must name the unknown policy"
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
                }),
            )],
            Vec::new(),
        ));
        assert_eq!(report.expected_opportunities, Some(12));
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
                let result = has_result.then(|| AttemptResultRecord {
                    finished_at: started_at,
                    retry_after: None,
                });
                attempts.push(AttemptRecord { started_at, result });
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
