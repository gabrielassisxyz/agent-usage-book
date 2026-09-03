//! The nine coverage scenarios from the design (PLAN.md 34.26), exercised through the
//! public coverage engine: a synthetic expected schedule joined with attempts and
//! observations, asserting that attempt coverage and measurement coverage differ
//! appropriately in each case.

use agent_usage_book::coverage::{
    AttemptRecord, AttemptResultRecord, CoverageFraction, CoverageInputs, ObservationRecord,
    PolicySnapshot, ResetRecord, compute, render,
};
use agent_usage_book::domain::time::{MonotonicDuration, UtcTimestamp};

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

fn observation(at_secs: i64) -> ObservationRecord {
    ObservationRecord { at: ts(at_secs) }
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

/// Scenario 1: the timer never ran. No attempts, no observations, full attempt
/// coverage failure; the measurement ratio is undefined because no attempt ever
/// reached a terminal state, which is a different fact from endpoints failing.
#[test]
fn timer_never_ran() {
    let report = compute(&inputs(0, 3_600, vec![snapshot(0, 300)], vec![], vec![]));
    assert_eq!(report.expected_opportunities, Some(12));
    assert_eq!(report.attempted_opportunities, 0);
    assert_eq!(report.successful_observations, 0);
    assert_eq!(report.attempt_coverage.unwrap().as_f64(), 0.0);
    assert_eq!(report.measurement_coverage, None);
    assert_eq!(
        report.longest_no_attempt_gap.unwrap().duration(),
        cadence(3_600)
    );
}

/// Scenario 2: the timer ran but every endpoint failed. Full attempt coverage, zero
/// measurement coverage.
#[test]
fn timer_ran_but_every_endpoint_failed() {
    let attempts: Vec<AttemptRecord> = (0..12)
        .map(|i| attempt(i * 300, Some(success_result(i * 300))))
        .collect();
    let report = compute(&inputs(0, 3_600, vec![snapshot(0, 300)], attempts, vec![]));
    assert_eq!(report.attempted_opportunities, 12);
    assert_eq!(report.successful_observations, 0);
    assert_eq!(report.attempt_coverage.unwrap().as_f64(), 1.0);
    assert_eq!(report.measurement_coverage, CoverageFraction::new(0, 12));
}

/// Scenario 3: intermittent failures. Attempt and measurement coverage are both
/// partial and differ.
#[test]
fn intermittent_failures() {
    let attempts: Vec<AttemptRecord> = (1..=6)
        .map(|i| attempt(i * 300, Some(success_result(i * 300))))
        .collect();
    let observations = vec![observation(300), observation(900), observation(1_500)];
    let report = compute(&inputs(
        0,
        3_600,
        vec![snapshot(0, 300)],
        attempts,
        observations,
    ));
    assert_eq!(report.attempted_opportunities, 6);
    assert_eq!(report.successful_observations, 3);
    assert_eq!(report.attempt_coverage.unwrap().as_f64(), 0.5);
    assert_eq!(report.measurement_coverage, CoverageFraction::new(3, 6));
}

/// Scenario 4: a no-attempt interval corresponding to simulated machine sleep. The
/// gap is reported as a duration, with no cause.
#[test]
fn simulated_sleep() {
    let report = compute(&inputs(
        0,
        7_200,
        vec![snapshot(0, 300)],
        vec![attempt(0, Some(success_result(0)))],
        vec![observation(0)],
    ));
    assert_eq!(
        report.longest_no_attempt_gap.unwrap().duration(),
        cadence(7_200)
    );
    let rendered = render(&report);
    assert!(
        rendered.contains("longest no-attempt gap: 2h"),
        "the gap must be a duration: {rendered}"
    );
    assert!(
        !rendered.to_lowercase().contains("sleep"),
        "the report must not name a cause: {rendered}"
    );
}

/// Scenario 5: an attempt started and never completed. Counted separately from both
/// coverage numbers, and excluded from the measurement denominator so collector
/// interruption never reads as provider failure.
#[test]
fn attempt_started_and_never_completed() {
    let report = compute(&inputs(
        0,
        3_600,
        vec![snapshot(0, 300)],
        vec![
            attempt(300, Some(success_result(300))),
            attempt(600, None),
            attempt(900, Some(success_result(900))),
        ],
        vec![observation(300), observation(900)],
    ));
    assert_eq!(report.attempted_opportunities, 3);
    assert_eq!(report.started_without_terminal_result, 1);
    assert_eq!(report.successful_observations, 2);
    // Both terminal attempts produced observations, so the conditional ratio is full;
    // a naive successful/started ratio would read 2/3 and fold the interruption in.
    assert_eq!(report.measurement_coverage, CoverageFraction::new(2, 2));
    assert_ne!(report.measurement_coverage, CoverageFraction::new(2, 3));
}

/// Scenario 6: a gap spanning a reset is marked severe.
#[test]
fn gap_spanning_reset() {
    let mut report_inputs = inputs(
        0,
        3_600,
        vec![snapshot(0, 300)],
        vec![attempt(300, Some(success_result(300)))],
        vec![observation(300)],
    );
    report_inputs.resets = vec![ResetRecord { at: ts(2_000) }];
    let report = compute(&report_inputs);
    assert!(report.severe);
    assert_eq!(report.reset_spanning_gaps.len(), 1);
}

/// Scenario 7: a normal reset-edge sample. The extra attempt is counted, and attempt
/// coverage saturates at 1.0 rather than exceeding it.
#[test]
fn normal_reset_edge_sample() {
    let mut attempts: Vec<AttemptRecord> = (0..12)
        .map(|i| attempt(i * 300, Some(success_result(i * 300))))
        .collect();
    // The reset-edge sample fires early, before the reset at 1800.
    attempts.push(attempt(1_650, Some(success_result(1_650))));
    let report = compute(&inputs(0, 3_600, vec![snapshot(0, 300)], attempts, vec![]));
    assert_eq!(report.expected_opportunities, Some(12));
    assert_eq!(report.attempted_opportunities, 13);
    assert_eq!(report.attempt_coverage.unwrap().as_f64(), 1.0);
}

/// Scenario 8: a cadence change mid-interval. The denominator follows the policy that
/// was in force, not the current configuration.
#[test]
fn cadence_change_mid_interval() {
    let report = compute(&inputs(
        0,
        3_600,
        vec![snapshot(0, 300), snapshot(1_800, 900)],
        vec![],
        vec![],
    ));
    assert_eq!(report.expected_opportunities, Some(8));
}

/// Scenario 9: authentication backoff. Auth failures are attempts without
/// observations, so measurement coverage falls while attempt coverage stays full.
#[test]
fn authentication_backoff() {
    let attempts: Vec<AttemptRecord> = (0..12)
        .map(|i| attempt(i * 300, Some(success_result(i * 300))))
        .collect();
    // Only the first four attempts produced a valid observation; the rest failed
    // authentication and produced none.
    let observations = vec![
        observation(0),
        observation(300),
        observation(600),
        observation(900),
    ];
    let report = compute(&inputs(
        0,
        3_600,
        vec![snapshot(0, 300)],
        attempts,
        observations,
    ));
    assert_eq!(report.attempt_coverage.unwrap().as_f64(), 1.0);
    assert_eq!(report.measurement_coverage, CoverageFraction::new(4, 12));
}
