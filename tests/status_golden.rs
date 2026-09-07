//! The status contract's golden renderings of the grouped grid, driven
//! through the production path: a seeded projection, the reader, the freshness
//! machine and the presentation renderer, with nothing constructed by hand on
//! the way.
//!
//! Local time is pinned to `America/Sao_Paulo` (a DST-less `-03` zone) for
//! every rendering here so the header stamp and the reset labels are
//! byte-stable; `LocalWallClock` reads `TZ` on every call.

use std::path::Path;

use agent_usage_book::domain::time::{Clock, ClockSkewEnvelope, FakeClock, MonotonicDuration};
use agent_usage_book::logging::LogicalName;
use agent_usage_book::presentation::Style;
use agent_usage_book::presentation::render::{
    ExplainMode, render_status_report_with_explain, render_window_duration,
};
use agent_usage_book::projection::reader::account_reading;
use agent_usage_book::projection::{ProjectedAccount, Projection};
use agent_usage_book::report::{
    MeterAccount, ProjectionReadState, ReportMetadata, StatusReport, StatusWindow,
};

use agent_usage_book::domain::attempt::AttemptId;
use agent_usage_book::domain::window::{
    ModelId, NominalWindowDuration, WindowResetState, WindowScope,
};

/// 2026-09-07 05:01:12 UTC, which is 2026-09-07 02:01 in `America/Sao_Paulo`.
const NOW_NANOS: i64 = 1_788_768_072 * 1_000_000_000;

const NANOS_PER_SECOND: i64 = 1_000_000_000;

fn nanos(seconds: i64) -> i64 {
    seconds * NANOS_PER_SECOND
}

/// Pins the zone `LocalWallClock` resolves. Every test calls this before
/// rendering; each sets the same value, so the parallel test threads do not
/// race on a meaningful difference.
fn pin_local_zone() {
    // SAFETY: every test in this binary sets TZ to the same value and none
    // unsets it, so no thread observes a different zone than it wrote.
    unsafe { std::env::set_var("TZ", "America/Sao_Paulo") };
}

fn window(
    semantic_key: &str,
    scope: WindowScope,
    used_ppm: i32,
    duration_seconds: i64,
    reset_offset_seconds: i64,
) -> agent_usage_book::projection::ProjectedWindow {
    use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaUsed};
    use agent_usage_book::domain::window::{QuantizationSemantics, ReportedResolution};
    agent_usage_book::projection::ProjectedWindow {
        semantic_key: semantic_key.to_string(),
        scope,
        quota_used_ppm: QuotaUsed::new(QuotaFractionPpm::new(used_ppm).unwrap()),
        reported_resolution_ppm: ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap())
            .unwrap(),
        quantization: QuantizationSemantics::Exact,
        resets_at: agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(
            NOW_NANOS + nanos(reset_offset_seconds),
        )
        .into(),
        nominal_duration_nanos: NominalWindowDuration::from_nanos(nanos(duration_seconds) as u64),
        is_active: true,
        severity: agent_usage_book::domain::window::WindowSeverity::unknown(),
    }
}

fn account_wide(
    used_ppm: i32,
    duration_seconds: i64,
    reset_offset_seconds: i64,
) -> agent_usage_book::projection::ProjectedWindow {
    window(
        "w",
        WindowScope::AccountWide,
        used_ppm,
        duration_seconds,
        reset_offset_seconds,
    )
}

fn success_observation(
    windows: Vec<agent_usage_book::projection::ProjectedWindow>,
    received_seconds_ago: i64,
) -> agent_usage_book::projection::SuccessfulObservation {
    use agent_usage_book::domain::time::MeasurementBasis;
    agent_usage_book::projection::SuccessfulObservation {
        observation_id: agent_usage_book::store::meter_evidence::ObservationRowId::new(7),
        provider_contract_id: agent_usage_book::domain::ids::ProviderContractId::new("contract-v1"),
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

fn success(seconds_ago: i64) -> Option<agent_usage_book::projection::TerminalOutcome> {
    Some(agent_usage_book::projection::TerminalOutcome {
        completed_at: agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(
            NOW_NANOS - nanos(seconds_ago),
        ),
        outcome: agent_usage_book::domain::attempt::AttemptOutcome::Success,
    })
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
        MonotonicDuration::from_seconds(720),
        MonotonicDuration::from_seconds(8),
        ClockSkewEnvelope::new(MonotonicDuration::from_seconds(60)),
    )
}

/// Builds the report account for one projected account through the same joins
/// `cli::projection_accounts` performs: the freshness reading, the derived
/// limiting window, the full window list and the provider.
fn status_account(projected: &ProjectedAccount, clock: &FakeClock) -> MeterAccount {
    let (fresh, command, skew) = horizon();
    let reading = account_reading(Some(projected), None, fresh, command, skew, clock);
    let observation_freshness = reading.freshness.clone();
    let mut account = MeterAccount::from_projection(
        LogicalName::new(projected.logical_name.clone()),
        reading.freshness,
        reading
            .limiting_window
            .map(|limit| agent_usage_book::report::LimitingWindow {
                scope: limit.scope,
                nominal_duration: limit.nominal_duration,
                reset_state: limit.reset_state,
            }),
        reading.included_scopes,
        None,
    )
    .with_provider(projected.provider.clone());
    if let Some(observed) = projected.last_successful_observation.as_ref() {
        let windows = observed
            .windows
            .iter()
            .map(|w| StatusWindow {
                semantic_key: w.semantic_key.clone(),
                scope: w.scope.clone(),
                quota_used: w.quota_used_ppm,
                reset_state: w.resets_at,
                nominal_duration: w.nominal_duration_nanos,
                rate: agent_usage_book::report::burn_rate::live_burn_rate(
                    w.quota_used_ppm,
                    w.resets_at,
                    w.nominal_duration_nanos,
                    clock.now(),
                ),
                capped_at: None,
                observation: observation_freshness.clone(),
            })
            .collect();
        account = account.with_windows(windows).with_meter_explanation(
            agent_usage_book::report::MeterExplanation {
                provider_contract_id: observed.provider_contract_id.clone(),
                windows: observed
                    .windows
                    .iter()
                    .map(|w| agent_usage_book::report::MeterWindowExplanation {
                        semantic_key: w.semantic_key.clone(),
                        scope: w.scope.clone(),
                        is_active: w.is_active,
                        severity: w.severity.clone(),
                        rate: None,
                    })
                    .collect(),
            },
        );
    }
    account
}

fn clock() -> FakeClock {
    FakeClock::new(agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(NOW_NANOS))
}

fn render(report: &StatusReport) -> String {
    pin_local_zone();
    render_status_report_with_explain(
        report,
        clock().now(),
        ClockSkewEnvelope::new(MonotonicDuration::from_seconds(60)),
        ExplainMode::Off,
        Style::plain(),
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

fn seeded_report(accounts: Vec<ProjectedAccount>) -> StatusReport {
    let clock = clock();
    let seeded = projection(accounts);
    let report_accounts = seeded
        .accounts
        .iter()
        .map(|projected| status_account(projected, &clock))
        .collect();
    report_with(report_accounts, ProjectionReadState::Read)
}

/// The two-account fixture golden: the `QUOTA` header, one `anthropic` block,
/// both accounts, and per account the `5h` and `week` rows plus one `fable`
/// row for `primary`, matched byte for byte.
#[test]
fn two_account_grid_golden() {
    let primary = account(
        "primary",
        Some(success_observation(
            vec![
                account_wide(620_000, 5 * 3_600, 3 * 3_600),
                account_wide(410_000, 7 * 86_400, 4 * 86_400),
                window(
                    "weekly_scoped_fable",
                    WindowScope::ModelSpecific(ModelId::new("fable".to_string())),
                    880_000,
                    7 * 86_400,
                    3 * 86_400,
                ),
            ],
            120,
        )),
        Some(latest_attempt(120, success(120))),
    );
    let gmail = account(
        "gmail",
        Some(success_observation(
            vec![
                account_wide(120_000, 5 * 3_600, 3 * 3_600),
                account_wide(50_000, 7 * 86_400, 4 * 86_400),
            ],
            120,
        )),
        Some(latest_attempt(120, success(120))),
    );
    let rendered = render(&seeded_report(vec![primary, gmail]));
    if let Ok(path) = std::env::var("AUB_BLESS_STATUS_GRID") {
        std::fs::write(path, format!("{rendered}\n")).unwrap();
    }
    let expected =
        std::fs::read_to_string("tests/fixtures/presentation/status_grid_two_accounts.txt")
            .unwrap();
    assert_eq!(rendered, expected.trim_end_matches('\n'));
}

/// Every non-blank row of the grid is the same visible width and the fixed
/// columns line up: label at 4, then bar, percent, rate. The planted negative
/// is the model row's position: `fable` after `week`, not before it.
#[test]
fn the_grid_columns_line_up_and_the_model_row_follows_the_weekly_one() {
    let primary = account(
        "primary",
        Some(success_observation(
            vec![
                account_wide(620_000, 5 * 3_600, 3 * 3_600),
                account_wide(410_000, 7 * 86_400, 4 * 86_400),
                window(
                    "weekly_scoped_fable",
                    WindowScope::ModelSpecific(ModelId::new("fable".to_string())),
                    880_000,
                    7 * 86_400,
                    3 * 86_400,
                ),
                window(
                    "weekly_scoped_aria",
                    WindowScope::ModelSpecific(ModelId::new("aria".to_string())),
                    120_000,
                    7 * 86_400,
                    3 * 86_400,
                ),
            ],
            120,
        )),
        Some(latest_attempt(120, success(120))),
    );
    let rendered = render(&seeded_report(vec![primary]));
    let rows: Vec<&str> = rendered
        .lines()
        .filter(|line| line.starts_with("    ") && !line.trim().is_empty())
        .collect();
    assert_eq!(rows.len(), 4);
    let width = rows[0].chars().count();
    for row in &rows {
        assert_eq!(row.chars().count(), width, "row width drift: {row:?}");
    }
    assert!(rows[0].trim_start().starts_with("5h"));
    assert!(rows[1].trim_start().starts_with("week"));
    assert!(
        rows[2].trim_start().starts_with("aria"),
        "models sort by name: {:?}",
        rows[2]
    );
    assert!(rows[3].trim_start().starts_with("fable"));
}

/// The bar is 30 cells and the fill count is `round(used_ppm * 30 / 1_000_000)`
/// at the design's sample points.
#[test]
fn the_bar_has_thirty_cells_and_rounds_the_fill() {
    for (used_ppm, filled) in [
        (0, 0),
        (10_000, 0),   // 1% -> round(0.3) = 0
        (490_000, 15), // 49% -> round(14.7) = 15
        (500_000, 15), // 50% -> round(15.0) = 15
        (990_000, 30), // 99% -> round(29.7) = 30
        (1_000_000, 30),
    ] {
        let primary = account(
            "primary",
            Some(success_observation(
                vec![account_wide(used_ppm, 5 * 3_600, 3 * 3_600)],
                120,
            )),
            Some(latest_attempt(120, success(120))),
        );
        let rendered = render(&seeded_report(vec![primary]));
        let row = rendered
            .lines()
            .find(|line| line.trim_start().starts_with("5h"))
            .unwrap();
        let bar: String = row
            .chars()
            .filter(|c| *c == '\u{2501}' || *c == '\u{2500}')
            .collect();
        assert_eq!(bar.chars().count(), 30, "bar is 30 cells at {used_ppm} ppm");
        assert_eq!(
            bar.chars().filter(|c| *c == '\u{2501}').count(),
            filled,
            "fill count at {used_ppm} ppm"
        );
    }
}

/// A reset instant renders as a local weekday and `HH:MM`, and a not-started
/// window reads `not started`.
#[test]
fn reset_labels_render_local_and_not_started() {
    let primary = account(
        "primary",
        Some(success_observation(
            vec![
                // NOW is Mon 08:01:12 UTC (Mon 05:01 -03); +1 day resets Tue 05:01 -03.
                account_wide(300_000, 5 * 3_600, 86_400),
                {
                    let mut idle = account_wide(0, 7 * 86_400, 0);
                    idle.resets_at = WindowResetState::NotStarted;
                    idle
                },
            ],
            120,
        )),
        Some(latest_attempt(120, success(120))),
    );
    let rendered = render(&seeded_report(vec![primary]));
    assert!(
        rendered.contains("Tue 05:01"),
        "local reset label: {rendered}"
    );
    assert!(
        rendered.contains("not started"),
        "not-started label: {rendered}"
    );
}

/// A never-observed account renders the freshness answer under its header, not
/// a fabricated grid.
#[test]
fn never_successfully_observed_renders_the_freshness_answer() {
    let rendered = render(&seeded_report(vec![account("primary", None, None)]));
    assert!(rendered.contains("  primary  anthropic\n    ? · stale · no successful sample"));
}

/// An auth-required account renders `auth!` and no rows.
#[test]
fn auth_required_renders_the_marker() {
    let rendered = render(&seeded_report(vec![account(
        "primary",
        Some(success_observation(
            vec![account_wide(620_000, 5 * 3_600, 3 * 3_600)],
            300,
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
    )]));
    assert!(
        rendered.contains("  primary  anthropic\n    auth!"),
        "{rendered}"
    );
}

/// A stale block carries `cached <age> ago` and the explain block still
/// follows the grid unchanged in content.
#[test]
fn stale_block_notes_the_cache_age_and_explain_still_appends() {
    let projected = account(
        "primary",
        Some(success_observation(
            vec![account_wide(380_000, 5 * 3_600, 3 * 3_600)],
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
    let report = seeded_report(vec![projected]);
    let plain = render(&report);
    assert!(plain.contains("cached 14m ago"), "{plain}");

    pin_local_zone();
    let explained = render_status_report_with_explain(
        &report,
        clock().now(),
        ClockSkewEnvelope::new(MonotonicDuration::from_seconds(60)),
        ExplainMode::Summary,
        Style::plain(),
    );
    assert!(explained.contains("meter explain:"), "{explained}");
    assert!(explained.contains("provider contract: contract-v1"));
}

/// The projection itself being missing is still the bare question mark.
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

    pin_local_zone();
    let with_reason = render_status_report_with_explain(
        &report,
        agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(NOW_NANOS),
        ClockSkewEnvelope::new(MonotonicDuration::from_seconds(60)),
        ExplainMode::Summary,
        Style::plain(),
    );
    assert_eq!(with_reason, "aub ? · projection not found");
}

/// A model-scoped weekly window is one row of the grid, labelled with the
/// model display name, and it is the limiting window when it is the most used.
#[test]
fn scoped_weekly_window_is_a_row_and_the_limit() {
    let mut scoped = window(
        "weekly_scoped_sonnet",
        WindowScope::ModelSpecific(ModelId::new("sonnet".to_string())),
        240_000,
        7 * 86_400,
        3 * 86_400,
    );
    scoped.severity = agent_usage_book::domain::window::WindowSeverity::new("critical");
    let projected = account(
        "primary",
        Some(success_observation(
            vec![
                account_wide(80_000, 5 * 3_600, 3 * 3_600),
                account_wide(210_000, 7 * 86_400, 4 * 86_400),
                scoped,
            ],
            120,
        )),
        Some(latest_attempt(120, success(120))),
    );
    let report = seeded_report(vec![projected]);
    let rendered = render(&report);
    assert!(
        rendered
            .lines()
            .any(|line| line.trim_start().starts_with("sonnet")),
        "the model row is labelled with the display name: {rendered}"
    );
    let account = &report.accounts[0];
    assert_eq!(
        account
            .limiting_status_window()
            .map(|w| w.semantic_key.as_str()),
        Some("weekly_scoped_sonnet"),
    );
}

/// The window duration labels the grid's account-wide rows carry.
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

/// A projection the reader refused at the path level is also the question
/// mark: this walks a real file.
#[test]
fn an_unreadable_projection_file_renders_the_question_mark_at_the_path() {
    let scratch = std::env::temp_dir().join(format!("aub-status-golden-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).expect("scratch dir");
    let path = scratch.join("projection");
    std::fs::write(
        &path,
        "{\"schema_version\":99,\"ledger_generation\":1,\"accounts\":[]}",
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
