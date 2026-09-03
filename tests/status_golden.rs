//! The status contract's golden renderings, driven through the production
//! path: a seeded projection, the reader, the freshness machine and the
//! presentation renderer, with nothing constructed by hand on the way.

use std::path::Path;

use agent_usage_book::domain::time::{Clock, ClockSkewEnvelope, FakeClock, MonotonicDuration};
use agent_usage_book::logging::LogicalName;
use agent_usage_book::presentation::render::{
    ExplainMode, render_status_report_with_explain, render_window_duration,
};
use agent_usage_book::projection::reader::account_reading;
use agent_usage_book::projection::{ProjectedAccount, Projection};
use agent_usage_book::report::{
    LimitingWindow, MeterAccount, ProjectionReadState, ReportMetadata, StatusReport,
};

use agent_usage_book::domain::attempt::AttemptId;
use agent_usage_book::domain::freshness::Freshness;
use agent_usage_book::domain::quota::QuotaRemaining;
use agent_usage_book::domain::window::{ModelId, NominalWindowDuration, WindowScope};

/// One minute and twelve seconds after the Unix epoch: the fixed now every
/// rendering below is computed against.
const NOW_NANOS: i64 = 72 * 1_000_000_000;

const NANOS_PER_SECOND: i64 = 1_000_000_000;

fn nanos(seconds: i64) -> i64 {
    seconds * NANOS_PER_SECOND
}

fn window(
    used_ppm: i32,
    model: Option<&str>,
    duration_seconds: i64,
) -> agent_usage_book::projection::ProjectedWindow {
    use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaUsed};
    use agent_usage_book::domain::window::{QuantizationSemantics, ReportedResolution};
    agent_usage_book::projection::ProjectedWindow {
        semantic_key: "five_hour".to_string(),
        scope: match model {
            None => WindowScope::AccountWide,
            Some(name) => WindowScope::ModelSpecific(ModelId::new(name.to_string())),
        },
        quota_used_ppm: QuotaUsed::new(QuotaFractionPpm::new(used_ppm).unwrap()),
        reported_resolution_ppm: ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap())
            .unwrap(),
        quantization: QuantizationSemantics::Exact,
        resets_at: agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(nanos(10_800)),
        nominal_duration_nanos: NominalWindowDuration::from_nanos(nanos(duration_seconds) as u64),
    }
}

fn success_observation(
    windows: Vec<agent_usage_book::projection::ProjectedWindow>,
    received_seconds_ago: i64,
) -> agent_usage_book::projection::SuccessfulObservation {
    use agent_usage_book::domain::time::MeasurementBasis;
    agent_usage_book::projection::SuccessfulObservation {
        observation_id: agent_usage_book::store::meter_evidence::ObservationRowId::new(7),
        provider_observed_at: Some(
            agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(
                NOW_NANOS - nanos(received_seconds_ago),
            ),
        ),
        received_at: agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(
            NOW_NANOS - nanos(received_seconds_ago),
        ),
        measurement_basis: MeasurementBasis::ProviderObserved,
        windows,
    }
}

fn latest_attempt(
    started_seconds_ago: i64,
    result: Option<agent_usage_book::projection::TerminalOutcome>,
) -> agent_usage_book::projection::LatestAttempt {
    agent_usage_book::projection::LatestAttempt {
        attempt_id: AttemptId::new(9),
        request_started_at: agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(
            NOW_NANOS - nanos(started_seconds_ago),
        ),
        credential_context_id: Some("credential-context-v1".to_string()),
        result,
    }
}

fn account(
    name: &str,
    last_success: Option<agent_usage_book::projection::SuccessfulObservation>,
    attempt: Option<agent_usage_book::projection::LatestAttempt>,
) -> ProjectedAccount {
    ProjectedAccount {
        account_id: agent_usage_book::store::account::AccountId::new(1),
        logical_name: name.to_string(),
        provider: "anthropic".to_string(),
        last_successful_observation: last_success,
        latest_attempt: attempt,
    }
}

fn projection(accounts: Vec<ProjectedAccount>) -> Projection {
    Projection {
        ledger_generation: agent_usage_book::store::ledger_generation::Generation::new(12),
        accounts,
    }
}

fn horizon() -> (MonotonicDuration, MonotonicDuration, ClockSkewEnvelope) {
    (
        // freshness.meter's provisional horizon: an observation inside it is fresh.
        MonotonicDuration::from_seconds(720),
        // sampling.command_budget: a resultless attempt past it is interrupted.
        MonotonicDuration::from_seconds(8),
        ClockSkewEnvelope::new(MonotonicDuration::from_seconds(60)),
    )
}

fn reading_for(
    projected: Option<&ProjectedAccount>,
    freshness: Freshness<QuotaRemaining>,
    scopes: Vec<WindowScope>,
    limit: Option<(WindowScope, NominalWindowDuration)>,
) -> MeterAccount {
    MeterAccount::from_projection(
        LogicalName::new(
            projected
                .map(|account| account.logical_name.clone())
                .unwrap_or_else(|| "work-primary".to_string()),
        ),
        freshness,
        limit.map(|(scope, nominal_duration)| LimitingWindow {
            scope,
            nominal_duration,
        }),
        scopes,
        None,
    )
}

fn render(report: &StatusReport) -> String {
    let clock =
        FakeClock::new(agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(NOW_NANOS));
    render_status_report_with_explain(
        report,
        clock.now(),
        ClockSkewEnvelope::new(MonotonicDuration::from_seconds(60)),
        ExplainMode::Off,
    )
}

fn report_with(accounts: Vec<MeterAccount>, projection_state: ProjectionReadState) -> StatusReport {
    let timestamp = agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(NOW_NANOS);
    let metadata = ReportMetadata::new(
        timestamp,
        timestamp,
        agent_usage_book::report::LedgerGeneration::new(12),
        None,
    );
    StatusReport::new(metadata, accounts, vec![], projection_state)
}

/// Rendering 1: a fresh last attempt.
#[test]
fn fresh_last_attempt() {
    let account = account(
        "work-primary",
        Some(success_observation(
            vec![window(620_000, None, 5 * 3_600)],
            41,
        )),
        Some(latest_attempt(
            42,
            Some(agent_usage_book::projection::TerminalOutcome {
                completed_at: agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(
                    NOW_NANOS - nanos(41),
                ),
                outcome: agent_usage_book::domain::attempt::AttemptOutcome::Success,
            }),
        )),
    );
    let seeded = projection(vec![account]);
    let clock =
        FakeClock::new(agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(NOW_NANOS));
    let (fresh, command, skew) = horizon();
    let reading = account_reading(
        Some(&seeded.accounts[0]),
        None,
        fresh,
        command,
        skew,
        &clock,
    );

    let report = report_with(
        vec![reading_for(
            Some(&seeded.accounts[0]),
            reading.freshness,
            reading.included_scopes,
            reading
                .limiting_window
                .map(|limit| (limit.scope, limit.nominal_duration)),
        )],
        ProjectionReadState::Read,
    );
    assert_eq!(render(&report), "aub work-primary 38% left · 5h");
}

/// Rendering 2: no successful recent reading because the provider timed out.
#[test]
fn stale_after_a_timeout() {
    let account = account(
        "work-primary",
        Some(success_observation(
            vec![window(620_000, None, 5 * 3_600)],
            14 * 60,
        )),
        Some(latest_attempt(
            60,
            Some(agent_usage_book::projection::TerminalOutcome {
                completed_at: agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(
                    NOW_NANOS - nanos(30),
                ),
                outcome: agent_usage_book::domain::attempt::AttemptOutcome::Unreachable(
                    agent_usage_book::domain::failure::FailureClass::ConnectTimeout,
                ),
            }),
        )),
    );
    let seeded = projection(vec![account]);
    let clock =
        FakeClock::new(agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(NOW_NANOS));
    let (fresh, command, skew) = horizon();
    let reading = account_reading(
        Some(&seeded.accounts[0]),
        None,
        fresh,
        command,
        skew,
        &clock,
    );

    let report = report_with(
        vec![reading_for(
            Some(&seeded.accounts[0]),
            reading.freshness,
            reading.included_scopes,
            reading
                .limiting_window
                .map(|limit| (limit.scope, limit.nominal_duration)),
        )],
        ProjectionReadState::Read,
    );
    assert_eq!(
        render(&report),
        "aub work-primary ~38% · stale 14m · timeout"
    );
}

/// Rendering 3: the provider requires authentication.
#[test]
fn auth_required() {
    let account = account(
        "work-primary",
        Some(success_observation(
            vec![window(620_000, None, 5 * 3_600)],
            5 * 60,
        )),
        Some(latest_attempt(
            30,
            Some(agent_usage_book::projection::TerminalOutcome {
                completed_at: agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(
                    NOW_NANOS - nanos(29),
                ),
                outcome: agent_usage_book::domain::attempt::AttemptOutcome::AuthRequired,
            }),
        )),
    );
    let seeded = projection(vec![account]);
    let clock =
        FakeClock::new(agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(NOW_NANOS));
    let (fresh, command, skew) = horizon();
    let reading = account_reading(
        Some(&seeded.accounts[0]),
        None,
        fresh,
        command,
        skew,
        &clock,
    );

    let report = report_with(
        vec![reading_for(
            Some(&seeded.accounts[0]),
            reading.freshness,
            reading.included_scopes,
            reading
                .limiting_window
                .map(|limit| (limit.scope, limit.nominal_duration)),
        )],
        ProjectionReadState::Read,
    );
    assert_eq!(render(&report), "aub work-primary auth!");
}

/// Rendering 4: the collector started an attempt and never finished it.
#[test]
fn collector_interrupted() {
    let account = account(
        "work-primary",
        Some(success_observation(
            vec![window(620_000, None, 5 * 3_600)],
            9 * 60,
        )),
        Some(latest_attempt(60, None)),
    );
    let seeded = projection(vec![account]);
    let clock =
        FakeClock::new(agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(NOW_NANOS));
    let (fresh, command, skew) = horizon();
    let reading = account_reading(
        Some(&seeded.accounts[0]),
        None,
        fresh,
        command,
        skew,
        &clock,
    );

    let report = report_with(
        vec![reading_for(
            Some(&seeded.accounts[0]),
            reading.freshness,
            reading.included_scopes,
            reading
                .limiting_window
                .map(|limit| (limit.scope, limit.nominal_duration)),
        )],
        ProjectionReadState::Read,
    );
    assert_eq!(
        render(&report),
        "aub work-primary ~38% · stale 9m · collector interrupted"
    );
}

/// Rendering 5: never successfully observed.
#[test]
fn never_successfully_observed() {
    let seeded = projection(vec![account("work-primary", None, None)]);
    let clock =
        FakeClock::new(agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(NOW_NANOS));
    let (fresh, command, skew) = horizon();
    let reading = account_reading(
        Some(&seeded.accounts[0]),
        None,
        fresh,
        command,
        skew,
        &clock,
    );

    let report = report_with(
        vec![reading_for(
            Some(&seeded.accounts[0]),
            reading.freshness,
            reading.included_scopes,
            reading
                .limiting_window
                .map(|limit| (limit.scope, limit.nominal_duration)),
        )],
        ProjectionReadState::Read,
    );
    assert_eq!(
        render(&report),
        "aub work-primary ? · stale · no successful sample"
    );
}

/// Rendering 6: the projection itself is missing.
#[test]
fn projection_missing() {
    let report = report_with(
        vec![],
        ProjectionReadState::Unavailable {
            state: "missing",
            reason: "projection not found".to_string(),
        },
    );
    assert_eq!(render(&report), "aub ?");

    // Where the output mode permits, the reason rides along.
    let with_reason = render_status_report_with_explain(
        &report,
        agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(NOW_NANOS),
        ClockSkewEnvelope::new(MonotonicDuration::from_seconds(60)),
        ExplainMode::Summary,
    );
    assert_eq!(with_reason, "aub ? · projection not found");
}

/// The window duration labels the fresh line carries, at the ladder's every rung.
#[test]
fn window_duration_labels() {
    assert_eq!(
        render_window_duration(NominalWindowDuration::from_nanos(30_000_000_000)),
        "30s"
    );
    assert_eq!(
        render_window_duration(NominalWindowDuration::from_nanos(nanos(5 * 60) as u64)),
        "5m"
    );
    assert_eq!(
        render_window_duration(NominalWindowDuration::from_nanos(nanos(5 * 3_600) as u64)),
        "5h"
    );
    assert_eq!(
        render_window_duration(NominalWindowDuration::from_nanos(nanos(7 * 86_400) as u64)),
        "7d"
    );
}

/// A projection the reader refused at the path level is also the question mark:
/// this walks a real file, so the whole chain from filesystem to rendering is
/// the thing under test.
#[test]
fn an_unreadable_projection_file_renders_the_question_mark_at_the_path() {
    let scratch = std::env::temp_dir().join(format!("aub-status-golden-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).expect("scratch dir");
    let path = scratch.join("projection");
    std::fs::write(
        &path,
        format!("{{\"schema_version\":99,\"ledger_generation\":1,\"accounts\":[]}}"),
    )
    .unwrap();

    let read = agent_usage_book::projection::reader::read_projection(Path::new(&path));
    let state = match read {
        agent_usage_book::projection::reader::ProjectionRead::Available(_) => {
            panic!("schema version 99 must be refused")
        }
        agent_usage_book::projection::reader::ProjectionRead::Unavailable(unavailable) => {
            ProjectionReadState::Unavailable {
                state: unavailable.state_name(),
                reason: unavailable.reason(),
            }
        }
    };
    let ProjectionReadState::Unavailable { state, reason } = state else {
        panic!("unreachable")
    };
    assert_eq!(state, "unsupported_schema");
    let report = report_with(vec![], ProjectionReadState::Unavailable { state, reason });
    assert_eq!(render(&report), "aub ?");

    let _ = std::fs::remove_dir_all(&scratch);
}
