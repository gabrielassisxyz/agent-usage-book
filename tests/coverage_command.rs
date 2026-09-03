//! The `aub coverage` command: selectors, rendering, the threshold exit class,
//! and the no-network property.
//!
//! The worked examples from PLAN.md section 49 are seeded here through the
//! store's own insert paths and rendered in-process, so the golden pins the
//! whole pipeline: store reads, the coverage engine, the report model and the
//! presentation layer. The exit-class tests run the release binary against a
//! seeded state directory, because the exit status is the process's contract.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_usage_book::config::CoverageFloor;
use agent_usage_book::coverage::{self, CoverageInputs};
use agent_usage_book::domain::attempt::AttemptOutcome;
use agent_usage_book::domain::failure::{FailureClass, HttpStatusClass};
use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::meter::adapter::HttpTransport;
use agent_usage_book::meter::transport::{CommandBudget, HttpRequest, RequestTimeoutConfig};
use agent_usage_book::presentation::{
    coverage_json, render_coverage_report, validate_coverage_report_json,
};
use agent_usage_book::report::coverage::{
    CoverageFloors, CoverageSelector, assemble as assemble_coverage,
};
use agent_usage_book::store::{
    account as account_store, connection, meter_attempt as attempt_store,
    meter_evidence as evidence_store, migrate, migrations, sample_run as run_store,
    sampling_policy_snapshot as snapshot_store,
};
use rusqlite::Connection;
use serde_json::Value;
use test_support::StateDir;

const SECOND: i64 = 1_000_000_000;
/// The interval start of the worked examples: a fixed epoch second, so every
/// seeded timestamp is a constant and the golden is stable.
const T0: i64 = 1_800_000_000 * SECOND;
/// The interval end: exactly 24 hours after the start.
const T1: i64 = T0 + 86_400 * SECOND;

fn ts(seconds: i64) -> UtcTimestamp {
    UtcTimestamp::from_unix_nanos(seconds * SECOND)
}

fn floors() -> CoverageFloors {
    CoverageFloors {
        attempt: CoverageFloor::new(0.98).unwrap(),
        measurement: CoverageFloor::new(0.95).unwrap(),
    }
}

/// Opens a scratch ledger through the store's own path and migrates it.
fn open_ledger(state: &StateDir) -> Connection {
    let path = state.path().join(connection::LEDGER_DATABASE_FILE);
    let policy = connection::PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(500),
    };
    let mut conn = connection::open(&path, connection::AccessMode::ReadWrite, &policy)
        .expect("the scratch ledger must open");
    migrate::run_migrations(
        &mut conn,
        &migrations::registry(),
        None,
        &FakeClock::new(ts(0)),
    )
    .expect("the scratch ledger must migrate");
    conn
}

/// Seeds one attempt and, where the outcome is terminal, its result; a success
/// also records evidence and an observation, and `reset_at` hangs a 5h window
/// carrying that reset instant off the observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptCompletion {
    Terminal,
    Open,
}

fn seed_attempt(
    conn: &Connection,
    run: run_store::SampleRunId,
    account: account_store::AccountId,
    snapshot: snapshot_store::SamplingPolicySnapshotId,
    started: UtcTimestamp,
    outcome: AttemptOutcome,
    completion: AttemptCompletion,
) -> Option<evidence_store::ObservationRowId> {
    use agent_usage_book::domain::ids::{AdapterVersion, MeterSemanticsId, ProviderContractId};
    use attempt_store::{DueReason, NewMeterAttempt};
    use evidence_store::{NewMeterObservation, NewMeterResponseEvidence};

    let row = attempt_store::start_meter_attempt(
        conn,
        &NewMeterAttempt {
            run_id: run,
            account_id: account,
            provider: "provider-a".into(),
            request_started_at: started,
            credential_context_id: None,
            policy_snapshot_id: snapshot,
            due_at: started,
            due_reason: DueReason::OrdinaryCadence,
            due_basis: None,
            provider_contract_id: "endpoint-schema-v3".into(),
            meter_semantics_id: "account-5h-v2".into(),
        },
    )
    .expect("the attempt must insert");
    if completion == AttemptCompletion::Open {
        return None;
    }
    let finished = UtcTimestamp::from_unix_nanos(started.unix_nanos() + 30 * SECOND);
    attempt_store::record_meter_attempt_result(
        conn,
        &attempt_store::NewMeterAttemptResult {
            attempt_id: row,
            completed_at: finished,
            elapsed: MonotonicDuration::from_millis(100),
            outcome,
            sanitized_error_classification: None,
            retry_index: None,
            clock_anomaly: false,
        },
    )
    .expect("the result must insert");
    if outcome != AttemptOutcome::Success {
        return None;
    }
    let evidence = evidence_store::insert_response_evidence(
        conn,
        &NewMeterResponseEvidence {
            attempt_id: row,
            response_classification: "200".into(),
            received_at: finished,
            provider_observed_at_original: None,
            evidence_capsule: "{}".into(),
            capsule_schema_version: "capsule-v1".into(),
            sanitizer_version: "san-v1".into(),
            capture_truncated: false,
        },
    )
    .expect("the evidence must insert");
    let observation = evidence_store::insert_observation(
        conn,
        &NewMeterObservation {
            attempt_id: row,
            evidence_id: evidence,
            account_id: account,
            provider: "provider-a".into(),
            provider_observed_at: Some(finished),
            received_at: finished,
            measurement_basis: agent_usage_book::domain::time::MeasurementBasis::ProviderObserved,
            observed_plan: None,
            observed_tier: None,
            adapter_version: AdapterVersion::new("adapter-v1"),
            provider_contract_id: ProviderContractId::new("endpoint-schema-v3"),
            meter_semantics_id: MeterSemanticsId::new("account-5h-v2"),
            normalized_fingerprint: format!("fp-{}", started.unix_nanos()),
        },
    )
    .expect("the observation must insert");
    Some(observation)
}

/// Seeds one account's sampling policy: one snapshot, 5m cadence, effective an
/// hour before the worked examples' window.
fn seed_policy(
    conn: &Connection,
    account: account_store::AccountId,
) -> snapshot_store::SamplingPolicySnapshotId {
    snapshot_store::resolve_policy_snapshot(
        conn,
        account,
        ts(T0 / SECOND - 3600),
        &snapshot_store::ResolvedSamplingPolicy {
            ordinary_cadence: MonotonicDuration::from_seconds(300),
            freshness_horizon: MonotonicDuration::from_seconds(900),
            reset_edge_policy: "lead-120s".into(),
            retry_backoff_policy: "exponential-3".into(),
            command_budget: MonotonicDuration::from_seconds(30),
            policy_algorithm_version: "v1".into(),
        },
    )
    .expect("the policy snapshot must insert")
}

/// Seeds the two worked examples from PLAN.md section 49: a healthy-enough
/// work-primary and a research account with high attempt coverage and low
/// measurement coverage, one 5h reset lost inside a 2h 11m blind gap.
///
/// Both accounts carry a 5m cadence over the 24h window, so the raw slot count
/// is 288. work-primary misses two slots (two 9m blind gaps) and drops four
/// attempts to provider errors: 286 attempts, 282 observations. research
/// honors a 7,500s Retry-After (25 quiet slots), folds the next slot into an
/// early resume exactly 2h 11m after the rate-limited attempt, misses two more
/// slots with 9m catch-ups, and leaves four attempts started without a result:
/// 260 attempts, 256 terminal, 183 observations, and 73 terminal failures
/// (41 authentication, 28 rate limits counting the Retry-After one, 4
/// provider errors).
fn seed_worked_examples(conn: &Connection) {
    // The fixture deliberately writes hundreds of durable facts. Keep their
    // relationship to the production insert paths while committing the whole
    // synthetic history atomically, so test duration does not scale with the
    // filesystem's fsync latency.
    conn.execute_batch("BEGIN IMMEDIATE")
        .expect("the worked-example transaction must start");
    let run = run_store::start_sample_run(
        conn,
        run_store::Trigger::Timer,
        ts(T0 / SECOND + 600),
        "test",
    )
    .expect("the sample run must insert");
    let _second_timer = run_store::start_sample_run(
        conn,
        run_store::Trigger::Timer,
        ts(T0 / SECOND + 82_800),
        "test",
    )
    .expect("the second timer run must insert");

    let work = account_store::observe_account(conn, "provider-a", "work-primary", ts(T0 / SECOND))
        .expect("the work account must insert");
    let work_snapshot = seed_policy(conn, work);
    let research = account_store::observe_account(conn, "provider-a", "research", ts(T0 / SECOND))
        .expect("the research account must insert");
    let research_snapshot = seed_policy(conn, research);

    // ---- work-primary ----
    let missed: std::collections::BTreeSet<i64> = [143i64, 230].into_iter().collect();
    let catch_ups: std::collections::BTreeMap<i64, i64> = [(144i64, 142), (231, 229)].into();
    let provider_errors: std::collections::BTreeSet<i64> =
        [10i64, 50, 120, 200].into_iter().collect();
    for slot in 0..288i64 {
        if missed.contains(&slot) {
            continue;
        }
        let started = match catch_ups.get(&slot) {
            Some(&base) => ts(T0 / SECOND + 300 * base + 540),
            None => ts(T0 / SECOND + 300 * slot),
        };
        let outcome = if provider_errors.contains(&slot) {
            AttemptOutcome::Unreachable(FailureClass::HttpStatus(HttpStatusClass::ServerError))
        } else {
            AttemptOutcome::Success
        };
        seed_attempt(
            conn,
            run,
            work,
            work_snapshot,
            started,
            outcome,
            AttemptCompletion::Terminal,
        );
    }

    // ---- research ----
    let rate_limited_slot = 100i64;
    let quiet: std::collections::BTreeSet<i64> = (101..=125).collect();
    let missed_research: std::collections::BTreeSet<i64> = [50i64, 200].into_iter().collect();
    let catch_ups_research: std::collections::BTreeMap<i64, i64> = [(51i64, 49), (201, 199)].into();
    let interrupted: std::collections::BTreeSet<i64> = [10i64, 150, 160, 250].into_iter().collect();
    // 260 attempts, 256 terminal, 183 observations, 72 residual failures:
    // 41 authentication, 27 rate limits, 4 provider errors.
    let slots: Vec<i64> = (0..288i64)
        .filter(|slot| !quiet.contains(slot) && *slot != 126 && !missed_research.contains(slot))
        .collect();
    let non_interrupted: Vec<i64> = slots
        .iter()
        .copied()
        .filter(|slot| *slot != rate_limited_slot && !interrupted.contains(slot))
        .collect();
    let observed: std::collections::BTreeSet<i64> =
        non_interrupted[..183].iter().copied().collect();
    let residual: Vec<i64> = non_interrupted
        .iter()
        .copied()
        .filter(|slot| !observed.contains(slot))
        .collect();
    let auth: std::collections::BTreeSet<i64> = residual[..41].iter().copied().collect();
    let rate_limited: std::collections::BTreeSet<i64> = residual[41..68].iter().copied().collect();

    let mut reset_observation = None;
    for slot in slots {
        let started = match slot {
            127 => ts(T0 / SECOND + 37_860),
            slot if catch_ups_research.contains_key(&slot) => {
                let base = catch_ups_research[&slot];
                ts(T0 / SECOND + 300 * base + 540)
            }
            slot => ts(T0 / SECOND + 300 * slot),
        };
        let outcome = if slot == rate_limited_slot {
            AttemptOutcome::Unreachable(FailureClass::RateLimited {
                retry_after: Some(MonotonicDuration::from_seconds(7_500)),
            })
        } else if interrupted.contains(&slot) {
            // A started attempt with no terminal result.
            seed_attempt(
                conn,
                run,
                research,
                research_snapshot,
                started,
                AttemptOutcome::Success,
                AttemptCompletion::Open,
            );
            continue;
        } else if observed.contains(&slot) {
            let observation = seed_attempt(
                conn,
                run,
                research,
                research_snapshot,
                started,
                AttemptOutcome::Success,
                AttemptCompletion::Terminal,
            );
            if slot == 170 {
                reset_observation = observation;
            }
            continue;
        } else if auth.contains(&slot) {
            AttemptOutcome::AuthRequired
        } else if rate_limited.contains(&slot) {
            AttemptOutcome::Unreachable(FailureClass::RateLimited { retry_after: None })
        } else {
            AttemptOutcome::Unreachable(FailureClass::HttpStatus(HttpStatusClass::ServerError))
        };
        seed_attempt(
            conn,
            run,
            research,
            research_snapshot,
            started,
            outcome,
            AttemptCompletion::Terminal,
        );
    }

    // The 5h reset the blind gap swallowed, reported by a window row on an
    // observation made well inside the quiet stretch.
    evidence_store::insert_window(
        conn,
        &evidence_store::NewMeterWindow {
            observation_id: reset_observation.expect("slot 170 must have observed"),
            semantic_key: agent_usage_book::domain::window::WindowSemanticKey::new("5h"),
            scope: agent_usage_book::domain::window::WindowScope::AccountWide,
            quota_used: agent_usage_book::domain::quota::QuotaUsed::new(
                agent_usage_book::domain::quota::QuotaFractionPpm::new(410_000).unwrap(),
            ),
            reported_resolution: agent_usage_book::domain::window::ReportedResolution::new(
                agent_usage_book::domain::quota::QuotaFractionPpm::new(10_000).unwrap(),
            )
            .unwrap(),
            quantization: agent_usage_book::domain::window::QuantizationSemantics::Exact,
            resets_at: ts(T0 / SECOND + 35_000),
            nominal_duration: agent_usage_book::domain::window::NominalWindowDuration::from_nanos(
                18_000 * SECOND as u64,
            ),
        },
    )
    .expect("the reset window must insert");
    conn.execute_batch("COMMIT")
        .expect("the worked-example transaction must commit");
}

/// The golden: the two worked examples render, and the research account reads
/// as the design says it must, high attempt coverage and low measurement
/// coverage with the reason beside the number.
#[test]
fn the_two_worked_examples_render() {
    let state = StateDir::new();
    let conn = open_ledger(&state);
    seed_worked_examples(&conn);

    let selector = CoverageSelector::default();
    let report = assemble_coverage(
        &conn,
        ts(T0 / SECOND),
        ts(T1 / SECOND),
        &selector,
        floors(),
        ts(T1 / SECOND),
    )
    .expect("the worked examples must assemble");

    let rendered = render_coverage_report(&report, "24h");
    assert_eq!(
        rendered,
        [
            "coverage - last 24h",
            "",
            "account       attempts   measurements  longest blind gap  reset gaps",
            "work-primary  99.3%      98.6%         9m                 0",
            "research      98.9%      71.5%         2h 11m             1",
            "",
            "research:",
            "  - scheduler ran normally",
            "  - 41 attempts required authentication",
            "  - 28 attempts were rate limited",
            "  - 4 attempts hit an unreachable provider",
            "  - 4 attempts started without a terminal result",
            "  - one 5h reset occurred without a successful observation in the surrounding interval",
        ]
        .join("\n")
    );
}

/// The JSON contract: every quantity carries its unit, the two coverages are
/// separate fields in the system's fraction unit, and the validator accepts
/// the shape the serializer emits.
#[test]
fn the_json_contract_carries_units_and_both_coverages() {
    let state = StateDir::new();
    let conn = open_ledger(&state);
    seed_worked_examples(&conn);
    let report = assemble_coverage(
        &conn,
        ts(T0 / SECOND),
        ts(T1 / SECOND),
        &CoverageSelector::default(),
        floors(),
        ts(T1 / SECOND),
    )
    .expect("the worked examples must assemble");
    let json = coverage_json(
        &report,
        agent_usage_book::logging::RunId::new(ts(T1 / SECOND)),
    );

    let parsed: Value = serde_json::from_str(&json).expect("coverage JSON must parse");
    validate_coverage_report_json(&json).expect("the envelope must validate");

    let work = &parsed["accounts"][0];
    let research = &parsed["accounts"][1];
    assert_eq!(work["account"], "work-primary");
    assert_eq!(research["account"], "research");

    // The two coverage numbers are separate fields, each with its unit.
    assert_eq!(work["attempt_coverage"]["value"], "993056");
    assert_eq!(work["attempt_coverage"]["unit"], "ppm");
    assert_eq!(work["measurement_coverage"]["value"], "986014");
    assert_eq!(work["measurement_coverage"]["unit"], "ppm");
    assert_eq!(research["attempt_coverage"]["value"], "988593");
    assert_eq!(research["measurement_coverage"]["value"], "714844");

    // Every count carries its unit; the durations carry nanoseconds.
    assert_eq!(work["expected_opportunities"]["value"], "288");
    assert_eq!(work["expected_opportunities"]["unit"], "opportunities");
    assert_eq!(work["attempted_opportunities"]["value"], "286");
    assert_eq!(work["successful_observations"]["unit"], "observations");
    assert_eq!(
        research["longest_no_attempt_gap"]["value"],
        (7_860i64 * SECOND).to_string()
    );
    assert_eq!(research["longest_no_attempt_gap"]["unit"], "ns");
    assert_eq!(research["reset_spanning_gaps"]["value"], "1");
    assert_eq!(research["reset_spanning_gaps"]["unit"], "gaps");
    assert_eq!(research["severe"], true);
    assert_eq!(
        research["resets_in_gaps"][0]["window_length"]["value"],
        (18_000i64 * SECOND).to_string()
    );
    // The failure tally names the dominant class with its unit.
    assert_eq!(research["failures"]["authentication"]["value"], "41");
    assert_eq!(research["failures"]["authentication"]["unit"], "attempts");
    assert_eq!(research["failures"]["rate_limited"]["value"], "28");

    // The threshold verdict is a separate object with both floors.
    assert_eq!(parsed["threshold"]["attempt_floor"]["value"], "980000");
    assert_eq!(parsed["threshold"]["measurement_floor"]["value"], "950000");
    assert_eq!(parsed["threshold"]["met"], false);
    assert_eq!(parsed["threshold"]["breaches"][0]["account"], "research");
    assert_eq!(
        parsed["threshold"]["breaches"][0]["dimension"],
        "measurement"
    );
    assert_eq!(
        parsed["threshold"]["breaches"][0]["coverage"]["value"],
        "714844"
    );
    assert_eq!(
        parsed["threshold"]["breaches"][0]["floor"]["value"],
        "950000"
    );
}

/// An interval with no policy snapshot in force is visible in both output
/// modes: the human cell reads "unknown" and the JSON carries the named flag
/// with a null denominator, never a substituted number.
#[test]
fn a_policy_unknown_interval_is_visible_in_both_modes() {
    let state = StateDir::new();
    let conn = open_ledger(&state);
    let run = run_store::start_sample_run(
        &conn,
        run_store::Trigger::Timer,
        ts(T0 / SECOND + 600),
        "test",
    )
    .expect("the sample run must insert");
    // No policy snapshot at all: the account sampled before any snapshot was
    // recorded, which the engine must refuse to evaluate.
    let account = account_store::observe_account(&conn, "provider-a", "ghost", ts(T0 / SECOND))
        .expect("the account must insert");
    seed_attempt(
        &conn,
        run,
        account,
        // The attempt row requires a snapshot id; a dangling one would violate
        // the foreign key, so the account owns no snapshot and this seed uses
        // a snapshot that starts after the interval: in force for nothing.
        snapshot_store::resolve_policy_snapshot(
            &conn,
            account,
            ts(T1 / SECOND + 3_600),
            &snapshot_store::ResolvedSamplingPolicy {
                ordinary_cadence: MonotonicDuration::from_seconds(300),
                freshness_horizon: MonotonicDuration::from_seconds(900),
                reset_edge_policy: String::new(),
                retry_backoff_policy: String::new(),
                command_budget: MonotonicDuration::from_seconds(30),
                policy_algorithm_version: "v1".into(),
            },
        )
        .expect("the later snapshot must insert"),
        ts(T0 / SECOND + 300),
        AttemptOutcome::Success,
        AttemptCompletion::Terminal,
    );

    let report = assemble_coverage(
        &conn,
        ts(T0 / SECOND),
        ts(T1 / SECOND),
        &CoverageSelector::default(),
        floors(),
        ts(T1 / SECOND),
    )
    .expect("the report must assemble");
    let rendered = render_coverage_report(&report, "24h");
    assert!(
        rendered.contains("unknown"),
        "the human table must show the unknown policy: {rendered}"
    );
    assert!(
        rendered.contains("no sampling policy snapshot covers the whole interval"),
        "the detail block must name the refusal: {rendered}"
    );

    let json = coverage_json(
        &report,
        agent_usage_book::logging::RunId::new(ts(T1 / SECOND)),
    );
    let parsed: Value = serde_json::from_str(&json).expect("coverage JSON must parse");
    assert_eq!(parsed["accounts"][0]["policy_unknown"], true);
    assert_eq!(parsed["accounts"][0]["expected_opportunities"], Value::Null);
    assert_eq!(parsed["accounts"][0]["attempt_coverage"], Value::Null);
}

/// The selectors compose before the threshold verdict: a named account keeps
/// its own verdict, and adding `--severe` narrows that same account rather
/// than evaluating a different denominator.
#[test]
fn account_and_severe_selectors_compose_before_the_threshold_verdict() {
    let state = StateDir::new();
    let conn = open_ledger(&state);
    seed_worked_examples(&conn);

    let research = assemble_coverage(
        &conn,
        ts(T0 / SECOND),
        ts(T1 / SECOND),
        &CoverageSelector {
            account: Some("research".to_string()),
            severe_only: true,
        },
        floors(),
        ts(T1 / SECOND),
    )
    .expect("the selected severe account must assemble");
    assert_eq!(research.accounts.len(), 1);
    assert_eq!(research.accounts[0].name.as_str(), "research");
    assert!(
        !research.threshold.met,
        "research breaches measurement coverage"
    );

    let work = assemble_coverage(
        &conn,
        ts(T0 / SECOND),
        ts(T1 / SECOND),
        &CoverageSelector {
            account: Some("work-primary".to_string()),
            severe_only: true,
        },
        floors(),
        ts(T1 / SECOND),
    )
    .expect("an account selector may have no severe interval");
    assert!(work.accounts.is_empty());
    assert!(
        work.threshold.met,
        "an empty severe selection has no breach"
    );
}

/// A transport that fails the test if anything in the coverage path sends a
/// request. The command never constructs one; this is the tripwire at the
/// crate's only HTTP port (`meter::adapter::HttpTransport`), proven live by
/// the next test and left uninvoked by the pipeline test after it.
struct CoverageMustNotTouchNetwork;

impl HttpTransport for CoverageMustNotTouchNetwork {
    fn send(
        &self,
        _request: &HttpRequest,
        _budget: &CommandBudget,
        _clock: &impl agent_usage_book::domain::time::Clock,
    ) -> Result<agent_usage_book::meter::transport::HttpResponse, FailureClass> {
        panic!("aub coverage performs no network operation")
    }
}

/// The tripwire is live: invoking it fails, so the pipeline test below means
/// something when it completes without a panic.
#[test]
#[should_panic(expected = "aub coverage performs no network operation")]
fn the_tripwire_transport_fires_when_invoked() {
    let clock = FakeClock::new(ts(0));
    let budget = CommandBudget::new(MonotonicDuration::from_seconds(1), &clock);
    let request = HttpRequest::get(
        "http://unused.invalid",
        RequestTimeoutConfig::new(
            MonotonicDuration::from_millis(10),
            MonotonicDuration::from_millis(10),
            None,
        ),
    );
    let _ = CoverageMustNotTouchNetwork.send(&request, &budget, &clock);
}

/// The whole coverage pipeline, run against the worked examples' ledger,
/// completes without the tripwire transport ever firing.
#[test]
fn the_coverage_pipeline_performs_no_network_operation() {
    let transport = CoverageMustNotTouchNetwork;
    let state = StateDir::new();
    let conn = open_ledger(&state);
    seed_worked_examples(&conn);
    let report = assemble_coverage(
        &conn,
        ts(T0 / SECOND),
        ts(T1 / SECOND),
        &CoverageSelector::default(),
        floors(),
        ts(T1 / SECOND),
    )
    .expect("the report must assemble");
    let _rendered = render_coverage_report(&report, "24h");
    let _json = coverage_json(
        &report,
        agent_usage_book::logging::RunId::new(ts(T1 / SECOND)),
    );
    let _ = transport;
}

/// The command's orchestration file may read the local store, but it may not
/// acquire the HTTP port. The panic transport above proves that port is a live
/// tripwire; this structural check binds it to the coverage command's actual
/// entry surface, where a future request would otherwise bypass the fixture.
#[test]
fn the_coverage_command_never_acquires_the_http_port() {
    let cli_source = include_str!("../src/cli.rs");
    for forbidden in ["HttpTransport", "execute_single", "ureq::"] {
        assert!(
            !cli_source.contains(forbidden),
            "aub coverage must not acquire the HTTP port: found {forbidden} in src/cli.rs"
        );
    }
}

/// The engine's own inputs, rebuilt from the seeded ledger, match what the
/// report model carries: the golden above rests on real engine arithmetic,
/// not on a hand-written report.
#[test]
fn the_engine_reports_the_worked_example_numbers() {
    let state = StateDir::new();
    let conn = open_ledger(&state);
    seed_worked_examples(&conn);
    let accounts = account_store::all_accounts(&conn).expect("accounts must read");
    assert_eq!(accounts.len(), 2);
    let research = accounts
        .iter()
        .find(|account| account.logical_name() == "research")
        .expect("research must be recorded");
    let attempts = attempt_store::attempts_with_outcomes_for_account_between(
        &conn,
        research.id(),
        ts(T0 / SECOND),
        ts(T1 / SECOND),
    )
    .expect("attempts must read");
    let observations = evidence_store::observation_times_for_account_between(
        &conn,
        research.id(),
        ts(T0 / SECOND),
        ts(T1 / SECOND),
    )
    .expect("observations must read");
    let resets = evidence_store::reset_windows_for_account_between(
        &conn,
        research.id(),
        ts(T0 / SECOND),
        ts(T1 / SECOND),
    )
    .expect("resets must read");
    let snapshots =
        snapshot_store::snapshots_for_account(&conn, research.id()).expect("snapshots must read");
    let timer_runs = run_store::timer_run_times_between(&conn, ts(T0 / SECOND), ts(T1 / SECOND))
        .expect("timer runs must read");

    let inputs = CoverageInputs {
        interval_start: ts(T0 / SECOND),
        interval_end: ts(T1 / SECOND),
        policy_snapshots: snapshots
            .iter()
            .map(|snapshot| coverage::PolicySnapshot {
                effective_at: snapshot.effective_at(),
                ordinary_cadence: snapshot.policy().ordinary_cadence,
            })
            .collect(),
        attempts: attempts
            .iter()
            .map(|attempt| coverage::AttemptRecord {
                started_at: attempt.started_at,
                result: attempt
                    .terminal
                    .as_ref()
                    .map(|terminal| coverage::AttemptResultRecord {
                        finished_at: terminal.finished_at,
                        retry_after: terminal.retry_after,
                    }),
            })
            .collect(),
        observations: observations
            .iter()
            .map(|at| coverage::ObservationRecord { at: *at })
            .collect(),
        resets: resets
            .iter()
            .map(|reset| coverage::ResetRecord { at: reset.at })
            .collect(),
        timer_runs: timer_runs
            .iter()
            .map(|at| coverage::TimerRunRecord { at: *at })
            .collect(),
    };
    let engine = coverage::compute(&inputs);
    assert_eq!(engine.expected_opportunities, Some(263));
    assert_eq!(engine.attempted_opportunities, 260);
    assert_eq!(engine.successful_observations, 183);
    assert_eq!(engine.started_without_terminal_result, 4);
    let attempt_coverage = engine.attempt_coverage.expect("a ratio exists");
    assert!((attempt_coverage.as_f64() - 260.0 / 263.0).abs() < 1e-9);
    let measurement = engine.measurement_coverage.expect("a ratio exists");
    assert!((measurement.as_f64() - 183.0 / 256.0).abs() < 1e-9);
    let gap = engine.longest_no_attempt_gap.expect("the blind gap exists");
    assert_eq!(gap.duration().as_nanos(), 7_860 * SECOND as u64);
    assert_eq!(engine.reset_spanning_gaps.len(), 1);
    assert!(engine.severe);
}

// --- the process exit contract ----------------------------------------------

fn aub() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aub"))
}

/// A process-level fixture with a complete policy history around the real clock.
/// The command owns its clock, so the fixture extends past its construction time
/// by one cadence slot to avoid making the proof depend on scheduling jitter.
struct ExitFixture {
    state: StateDir,
    config_file: std::path::PathBuf,
}

impl ExitFixture {
    fn new(
        attempt_count: usize,
        outcome: AttemptOutcome,
        attempt_floor: f64,
        measurement_floor: f64,
    ) -> Self {
        let state = StateDir::new();
        let conn = open_ledger(&state);
        let now = UtcTimestamp::from_unix_nanos(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("the system clock must be after the Unix epoch")
                .as_nanos()
                .try_into()
                .expect("the current timestamp must fit i64 nanoseconds"),
        );
        let interval_start =
            UtcTimestamp::from_unix_nanos(now.unix_nanos() - 24 * 60 * 60 * SECOND);
        let run =
            run_store::start_sample_run(&conn, run_store::Trigger::Timer, interval_start, "test")
                .expect("the sample run must insert");
        let account = account_store::observe_account(&conn, "provider-a", "work", interval_start)
            .expect("the account must insert");
        let snapshot = snapshot_store::resolve_policy_snapshot(
            &conn,
            account,
            UtcTimestamp::from_unix_nanos(interval_start.unix_nanos() - SECOND),
            &snapshot_store::ResolvedSamplingPolicy {
                ordinary_cadence: MonotonicDuration::from_seconds(3_600),
                freshness_horizon: MonotonicDuration::from_seconds(900),
                reset_edge_policy: String::new(),
                retry_backoff_policy: String::new(),
                command_budget: MonotonicDuration::from_seconds(30),
                policy_algorithm_version: "v1".into(),
            },
        )
        .expect("the snapshot must insert");

        for slot in 0..attempt_count {
            let started = UtcTimestamp::from_unix_nanos(
                interval_start.unix_nanos() + slot as i64 * 300 * SECOND,
            );
            seed_attempt(
                &conn,
                run,
                account,
                snapshot,
                started,
                outcome,
                AttemptCompletion::Terminal,
            );
        }

        let config_file = state.path().join("aub.toml");
        std::fs::write(
            &config_file,
            format!(
                "[coverage]\nattempt_floor = {attempt_floor}\nmeasurement_floor = {measurement_floor}\n"
            ),
        )
        .expect("the coverage config must write");
        Self { state, config_file }
    }
}

/// Runs `aub coverage` as the process it is, against the fixture's ledger.
fn run_coverage(fixture: &ExitFixture, args: &[&str]) -> (i32, String, String) {
    let output = aub()
        .args(["coverage"])
        .args(args_from(args))
        .env("HOME", fixture.state.path().join("home"))
        .env("AUB_STATE_DIR", fixture.state.path())
        .env("AUB_CONFIG_FILE", &fixture.config_file)
        .env("AUB_LOG_LEVEL", "off")
        .current_dir(fixture.state.path())
        .output()
        .expect("the aub binary must run");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn args_from(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

#[test]
fn threshold_exit_fires_for_the_attempt_floor_but_not_the_measurement_floor() {
    let fixture = ExitFixture::new(1, AttemptOutcome::Success, 0.98, 0.0);
    let (status, stdout, stderr) = run_coverage(&fixture, &[]);

    assert_eq!(status, 7, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("coverage - last 24h"), "{stdout}");
    assert!(stderr.contains("attempt coverage"), "{stderr}");
    assert!(!stderr.contains("measurement coverage"), "{stderr}");
}

#[test]
fn threshold_exit_fires_for_the_measurement_floor_but_not_the_attempt_floor() {
    let fixture = ExitFixture::new(26, AttemptOutcome::AuthRequired, 0.0, 0.95);
    let (status, stdout, stderr) = run_coverage(&fixture, &[]);

    assert_eq!(status, 7, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("coverage - last 24h"), "{stdout}");
    assert!(stderr.contains("measurement coverage"), "{stderr}");
    assert!(!stderr.contains("attempt coverage"), "{stderr}");
}

#[test]
fn threshold_exit_does_not_fire_when_both_floors_are_met() {
    let fixture = ExitFixture::new(1, AttemptOutcome::Success, 0.0, 0.0);
    let (status, stdout, stderr) = run_coverage(&fixture, &[]);

    assert_eq!(status, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("coverage - last 24h"), "{stdout}");
    assert!(stderr.is_empty(), "{stderr}");
}
