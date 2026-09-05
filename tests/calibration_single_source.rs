//! Integration tests for `aub-c0b.13` and `aub-cab.6`: a single stored
//! calibration record is the only source the consumers read from.
//!
//! `aub-c0b.13` proves it for the two Phase 8 consumers (`calibrate show`
//! and the calibrated spend-to-window conversion,
//! `aub spend --window-equivalent`). `aub-cab.6` extends the same proof
//! through the Phase 11 consumer (`aub can-run --cached`): all three name
//! and use one calibration identifier, then all move to its successor
//! without source or configuration changes (PLAN.md 3.3, 34.20, 43
//! workflows 1, 3 and 6).
//!
//! An absence check (no literal in source) cannot prove consumers resolve one
//! shared record: a consumer that copied a coefficient into a local constant
//! would pass that check and still be wrong. Each test here seeds one record
//! with a conspicuous synthetic value through the real store chain (the
//! `__calibration-fixture` / `__cost-model-fixture` hooks, the only production
//! path into these tables from outside the crate), reads every consumer
//! through the release binary, supersedes the record append-only, and reads
//! every consumer again with no source or configuration edit in between. A
//! consumer that cached or copied the value instead of resolving it through
//! the repository would keep reporting the superseded number.
//!
//! The `aub-cab.6` test additionally asserts each consumer's repository read
//! in-process through the library: `load_active_at` for `calibrate show`,
//! `load_active_at` plus `conversion::convert` for spend, and
//! `load_active_at` plus the headroom the can-run join computes from the
//! loaded row's uncertainty. The binary assertions prove the shipped path;
//! the library assertions pin each consumer to the shared row rather than to
//! a coinciding label.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_usage_book::advice::headroom::{
    CalibratedWindowConstraint, CalibrationHealth as AdviceHealth, window_credit_headroom,
};
use agent_usage_book::calibration::conversion::{WindowConversionContext, convert};
use agent_usage_book::calibration::health::{
    ApplicabilityContext, CalibrationFacts, CalibrationHealth, HealthInputs, LifecycleState,
    compute_health,
};
use agent_usage_book::domain::credits::Credits;
use agent_usage_book::domain::ids::{
    AdapterVersion, MeterSemanticsId, NativeSessionId, ProviderContractId, SessionId,
    SourceNamespace,
};
use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaUsed};
use agent_usage_book::domain::time::{MonotonicDuration, UtcTimestamp};
use agent_usage_book::domain::window::{
    MeterWindow, NominalWindowDuration, QuantizationSemantics, ReportedResolution,
    WindowResetState, WindowScope, WindowSemanticKey,
};
use agent_usage_book::evidence::{
    CoverageCompleteness, Derivation, EvidenceQuality, Provenance, Qualified,
};
use agent_usage_book::store::calibration::{CalibrationScope, PlanTier, load_active_at};
use agent_usage_book::store::connection::{self, AccessMode, PragmaPolicy};
use agent_usage_book::store::cost_model::ProviderKey;
use agent_usage_book::store::meter_attempt::{
    DueReason, NewMeterAttempt, NewMeterAttemptResult, record_meter_attempt_result,
    start_meter_attempt,
};
use agent_usage_book::store::meter_evidence::{
    NewMeterObservation, NewMeterResponseEvidence, NewMeterWindow, insert_observation,
    insert_response_evidence, insert_window,
};
use agent_usage_book::store::session_account_marker::{
    EvidenceDesignation, MarkerSource, NewSessionAccountMarker, insert_marker,
};
use test_support::StateDir;

fn aub(state: &StateDir) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aub"));
    cmd.env("HOME", state.path().join("home"))
        .env("AUB_STATE_DIR", state.path())
        .env("AUB_CONFIG_FILE", state.path().join("aub.toml"))
        .env("AUB_LOG_LEVEL", "off");
    cmd
}

/// Runs one `aub` invocation to completion and returns its captured stdout,
/// panicking with both streams on a non-zero exit so a failing step names
/// itself instead of surfacing as a confusing later assertion failure.
fn run(state: &StateDir, args: &[&str]) -> String {
    let output = aub(state)
        .args(args)
        .output()
        .expect("the aub binary must be spawnable");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "aub {args:?} must succeed, got {:?}.\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
    stdout
}

/// One transcript message: a session id, which token kind it reports, and how
/// many. Keeping every other kind at zero isolates each scenario's credits
/// contribution to one cost-model term.
struct Message {
    session_id: &'static str,
    kind: &'static str,
    count: u64,
}

fn usage_line(message: &Message) -> String {
    let fields = [
        (
            "input_tokens",
            if message.kind == "input" {
                message.count
            } else {
                0
            },
        ),
        (
            "output_tokens",
            if message.kind == "output" {
                message.count
            } else {
                0
            },
        ),
        (
            "cache_read_input_tokens",
            if message.kind == "cache_read" {
                message.count
            } else {
                0
            },
        ),
        (
            "cache_creation_input_tokens",
            if message.kind == "cache_write" {
                message.count
            } else {
                0
            },
        ),
    ];
    let usage = fields
        .iter()
        .map(|(name, value)| format!("\"{name}\":{value}"))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"type\":\"assistant\",\"timestamp\":\"2026-08-25T10:00:00.000Z\",\"sessionId\":\"{}\",\"message\":{{\"id\":\"m-{}\",\"model\":\"claude-3-5-sonnet\",\"usage\":{{{usage}}}}}}}\n",
        message.session_id, message.session_id
    )
}

/// Writes the config and a transcript carrying one message per given session,
/// then ingests it. Every session still needs its own account marker
/// (`mark_account`) before `spend` can attribute it.
fn seed_and_ingest(state: &StateDir, messages: &[Message]) {
    let corpus = state.path().join("transcripts/claude-code/project");
    std::fs::create_dir_all(&corpus).unwrap();
    let body = messages.iter().map(usage_line).collect::<Vec<_>>().concat();
    std::fs::write(corpus.join("sessions.jsonl"), body).unwrap();

    let config = format!(
        "state.dir = \"{}\"\n\n[[transcripts]]\nname = \"claude-code\"\nroot = \"{}\"\npattern = \"**/*.jsonl\"\nformat = \"claude-code\"\n",
        state.path().display(),
        state.path().join("transcripts/claude-code").display(),
    );
    std::fs::write(state.path().join("aub.toml"), config).unwrap();

    run(state, &["ingest", "transcripts"]);
}

/// Marks `session_id` as belonging to account `work` at the strongest evidence
/// rank, through the real repository function rather than hand-written SQL
/// against a schema this file does not own.
fn mark_account(state: &StateDir, session_id: &str) {
    let path = state.path().join(connection::LEDGER_DATABASE_FILE);
    let conn = connection::open(
        &path,
        AccessMode::ReadWrite,
        &PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1_000),
        },
    )
    .expect("the ingested ledger must already exist and open");
    insert_marker(
        &conn,
        &NewSessionAccountMarker {
            session_id: SessionId::new(
                SourceNamespace::new("claude-code"),
                NativeSessionId::new(session_id),
            ),
            observed_at: UtcTimestamp::parse_rfc3339("2026-08-25T09:00:00Z")
                .expect("the marker timestamp must parse"),
            source_ordering_key: None,
            logical_account: "work".to_string(),
            resolved_account_id: None,
            marker_source: MarkerSource::new("hook"),
            run_id: None,
            evidence_designation: EvidenceDesignation::ExplicitLauncherOrHook,
        },
    )
    .expect("the account marker must insert");
}

const SPEND_WINDOW_ARGS: [&str; 6] = [
    "--since",
    "2026-08-25",
    "--days",
    "1",
    "--group-by",
    "account",
];

/// The window-equivalent line for one calibrated account child, so a test can
/// assert on the exact interval and calibration id together rather than on
/// two separately matched substrings that could belong to different lines.
fn window_equivalent_line<'a>(spend_text: &'a str, account: &str) -> &'a str {
    let account_marker = format!("account={account}");
    let account_at = spend_text
        .find(&account_marker)
        .unwrap_or_else(|| panic!("account={account} must appear in:\n{spend_text}"));
    spend_text[account_at..]
        .lines()
        .find(|line| line.contains("window equivalent"))
        .unwrap_or_else(|| {
            panic!("no window-equivalent line after account={account} in:\n{spend_text}")
        })
}

/// The `- <kind> ...` cost-model coverage line for one token kind, so a test
/// can assert whether that kind reads `modeled` or `missing`.
fn cost_model_kind_line<'a>(show_text: &'a str, kind: &str) -> &'a str {
    show_text
        .lines()
        .find(|line| line.trim_start().starts_with(&format!("- {kind}")))
        .unwrap_or_else(|| panic!("no cost-model coverage line for {kind} in:\n{show_text}"))
}

/// Criterion: one seeded calibration identifier appears in `calibrate show`
/// and calibrated spend-conversion provenance; append-only supersession moves
/// both Phase 8 calibration consumers in the same test run; no source or
/// configuration file changes between the two halves.
#[test]
fn calibration_supersession_moves_calibrate_show_and_spend_window_equivalent_together() {
    let state = StateDir::new();
    seed_and_ingest(
        &state,
        &[Message {
            session_id: "s-single-source-work",
            kind: "input",
            count: 1_000_000,
        }],
    );
    mark_account(&state, "s-single-source-work");
    run(&state, &["__cost-model-fixture", "complete"]);

    // First half: a conspicuous synthetic coefficient. 1,000,000 input tokens
    // at the complete model's 3 micros/token price 3,000,000 micros of
    // credits; divided by this coefficient that is exactly 10.0000 percentage
    // points, a value chosen for a clean division rather than realism.
    run(&state, &["__calibration-fixture", "five_hour", "30"]);

    let show_before = run(&state, &["calibrate", "show"]);
    assert!(
        show_before.contains("active window calibration five_hour-fixture-calibration\n"),
        "{show_before}"
    );
    assert!(
        show_before.contains("fitted:          30 micros/point"),
        "{show_before}"
    );

    let mut spend_before_args = vec!["spend"];
    spend_before_args.extend(SPEND_WINDOW_ARGS);
    spend_before_args.extend(["--window-equivalent", "five_hour", "--refresh", "never"]);
    let spend_before = run(&state, &spend_before_args);
    let line_before = window_equivalent_line(&spend_before, "work");
    assert!(spend_before.contains("3.00 credits"), "{spend_before}");
    assert!(
        line_before.contains("[10.0000, 10.0000] percentage points"),
        "{line_before}"
    );
    assert!(
        line_before.contains("calibration five_hour-fixture-calibration)"),
        "{line_before}"
    );

    // Second half: append-only supersession, no source or configuration edit.
    // The new coefficient is a third of the first, so the same 3,000,000
    // micros of credits now convert to exactly 30.0000 percentage points.
    run(&state, &["__calibration-fixture", "five_hour", "10"]);

    let show_after = run(&state, &["calibrate", "show"]);
    assert!(
        show_after.contains("active window calibration five_hour-fixture-calibration-1"),
        "{show_after}"
    );
    assert!(
        show_after.contains("fitted:          10 micros/point"),
        "{show_after}"
    );
    assert!(
        !show_after.contains("active window calibration five_hour-fixture-calibration\n"),
        "the superseded record must no longer be the active one:\n{show_after}"
    );

    let mut spend_after_args = vec!["spend"];
    spend_after_args.extend(SPEND_WINDOW_ARGS);
    spend_after_args.extend(["--window-equivalent", "five_hour", "--refresh", "never"]);
    let spend_after = run(&state, &spend_after_args);
    let line_after = window_equivalent_line(&spend_after, "work");
    assert!(
        line_after.contains("[30.0000, 30.0000] percentage points"),
        "{line_after}"
    );
    assert!(
        line_after.contains("calibration five_hour-fixture-calibration-1)"),
        "{line_after}"
    );

    // Both consumers named the successor and neither still named the
    // predecessor: the coefficient moved with the repository row, not with a
    // per-consumer copy.
    assert_ne!(line_before, line_after);
}

/// Criterion: the same structure proves all Phase 8 cost-model consumers move
/// together. `calibrate show`'s cost-model coverage and plain `spend
/// --credits`'s computed credits both trace the active cost model row.
///
/// The "before" half never has an active calibration: PLAN.md 23.8's
/// cache-write completeness rule refuses to activate any window calibration
/// whose referenced cost model is incomplete, regardless of workload, so a
/// calibration referencing the incomplete model does not exist to show. That
/// refusal is itself the single-source guarantee working as designed (a
/// second, independent consumer cannot be handed a stale coefficient by a
/// calibration that should never have activated), so `calibrate show`'s
/// "before" state is the honest empty one rather than a populated wrong-
/// coverage one. Supersession then both completes the cost model and, for
/// the first time, lets a calibration referencing it activate.
#[test]
fn cost_model_supersession_moves_calibrate_show_and_spend_credits_together() {
    let state = StateDir::new();
    seed_and_ingest(
        &state,
        &[Message {
            session_id: "s-single-source-cache-write",
            kind: "cache_write",
            count: 1_000_000,
        }],
    );
    mark_account(&state, "s-single-source-cache-write");

    // First half: the incomplete model has no cache_write term, so it prices
    // nothing of this workload, and no calibration can activate against it.
    run(&state, &["__cost-model-fixture", "incomplete"]);

    let show_before = run(&state, &["calibrate", "show"]);
    assert!(
        show_before.contains("no active calibration"),
        "{show_before}"
    );

    let mut spend_before_args = vec!["spend"];
    spend_before_args.extend(SPEND_WINDOW_ARGS);
    spend_before_args.extend(["--credits", "--refresh", "never"]);
    let spend_before = run(&state, &spend_before_args);
    assert!(
        spend_before.contains("credits unavailable: cache_write rate"),
        "{spend_before}"
    );

    // Second half: append-only supersession to the complete model, which
    // covers the workload, followed by the calibration that can only activate
    // now that its referenced cost model is complete. No source or
    // configuration edit accompanies either step.
    run(&state, &["__cost-model-fixture", "complete"]);
    run(&state, &["__calibration-fixture", "five_hour", "1000000"]);

    let show_after = run(&state, &["calibrate", "show"]);
    assert!(
        show_after.contains("cost model:      anthropic-claude-messages-v1"),
        "{show_after}"
    );
    assert!(
        cost_model_kind_line(&show_after, "cache_write").contains("modeled"),
        "{show_after}"
    );

    let mut spend_after_args = vec!["spend"];
    spend_after_args.extend(SPEND_WINDOW_ARGS);
    spend_after_args.extend(["--credits", "--refresh", "never"]);
    let spend_after = run(&state, &spend_after_args);
    // 1,000,000 cache_write tokens at 3.75 micros/token now that the active
    // model carries that term.
    assert!(spend_after.contains("3.75 credits"), "{spend_after}");
    assert!(
        !spend_after.contains("credits unavailable"),
        "the account total must no longer refuse for the previously-missing term:\n{spend_after}"
    );
}

/// Runs one `aub` invocation with extra environment entries, returning
/// captured stdout and stderr separately so a test can assert on the
/// structured run diagnostics (`run_started`, `report_rendered`) a command
/// emits alongside its report.
fn run_with_env(state: &StateDir, args: &[&str], extra_env: &[(&str, &str)]) -> (String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aub"));
    cmd.env("HOME", state.path().join("home"))
        .env("AUB_STATE_DIR", state.path())
        .env("AUB_CONFIG_FILE", state.path().join("aub.toml"));
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    let output = cmd
        .args(args)
        .output()
        .expect("the aub binary must be spawnable");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "aub {args:?} must succeed, got {:?}.\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
    (stdout, stderr)
}

/// Wall-clock now as Unix nanos, for seeding a meter observation the
/// `--cached` freshness policy still accepts: the binary reads it back
/// seconds later against the default twelve minute horizon.
fn now_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the system clock must be after the Unix epoch")
        .as_nanos()
        .try_into()
        .expect("the current timestamp must fit i64 nanoseconds")
}

/// Seeds the thinnest meter chain `can-run --cached` accepts, through the
/// store's own insert functions: one account, run, policy snapshot, attempt
/// with a successful result, evidence, observation and a single account-wide
/// `five_hour` window at 620,000 ppm used (38.0 percent remaining, mirroring
/// the worked example). No network is involved: every row is written
/// directly, and the binary's own drain publishes the projection from them
/// before the cached read.
fn seed_meter_five_hour(state: &StateDir) {
    use agent_usage_book::domain::time::MeasurementBasis;
    use agent_usage_book::store::account::observe_account;
    use agent_usage_book::store::sample_run::{Trigger, start_sample_run};
    use agent_usage_book::store::sampling_policy_snapshot::{
        ResolvedSamplingPolicy, resolve_policy_snapshot,
    };

    let at = UtcTimestamp::from_unix_nanos(now_nanos());
    let path = state.path().join(connection::LEDGER_DATABASE_FILE);
    let conn = connection::open(
        &path,
        AccessMode::ReadWrite,
        &PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1_000),
        },
    )
    .expect("the ledger must already exist and open");
    let account_id = observe_account(&conn, "anthropic", "work-primary", at)
        .expect("the meter account must insert");
    let run_id = start_sample_run(&conn, Trigger::Manual, at, "single-source-can-run")
        .expect("the sample run must insert");
    let snapshot_id = resolve_policy_snapshot(
        &conn,
        account_id,
        at,
        &ResolvedSamplingPolicy {
            ordinary_cadence: MonotonicDuration::from_seconds(300),
            freshness_horizon: MonotonicDuration::from_seconds(900),
            reset_edge_policy: "lead-120s".to_string(),
            retry_backoff_policy: "none".to_string(),
            command_budget: MonotonicDuration::from_seconds(30),
            policy_algorithm_version: "v1".to_string(),
        },
    )
    .expect("the policy snapshot must insert");
    let attempt_id = start_meter_attempt(
        &conn,
        &NewMeterAttempt {
            run_id,
            account_id,
            provider: "anthropic".to_string(),
            request_started_at: at,
            credential_context_id: None,
            policy_snapshot_id: snapshot_id,
            due_at: at,
            due_reason: DueReason::ForcedOrManual,
            due_basis: None,
            provider_contract_id: "contract-v1".to_string(),
            meter_semantics_id: "meter-v1".to_string(),
        },
    )
    .expect("the attempt must insert");
    record_meter_attempt_result(
        &conn,
        &NewMeterAttemptResult {
            attempt_id,
            completed_at: UtcTimestamp::from_unix_nanos(at.unix_nanos() + 50_000_000),
            elapsed: MonotonicDuration::from_millis(50),
            outcome: agent_usage_book::domain::attempt::AttemptOutcome::Success,
            sanitized_error_classification: None,
            retry_index: None,
            clock_anomaly: false,
        },
    )
    .expect("the attempt result must insert");
    let evidence_id = insert_response_evidence(
        &conn,
        &NewMeterResponseEvidence {
            attempt_id,
            response_classification: "200".to_string(),
            received_at: at,
            provider_observed_at_original: None,
            evidence_capsule: "{}".to_string(),
            capsule_schema_version: "capsule-v1".to_string(),
            sanitizer_version: "san-v1".to_string(),
            capture_truncated: false,
        },
    )
    .expect("the evidence must insert");
    let observation_id = insert_observation(
        &conn,
        &NewMeterObservation {
            attempt_id,
            evidence_id,
            account_id,
            provider: "anthropic".to_string(),
            provider_observed_at: None,
            received_at: at,
            measurement_basis: MeasurementBasis::LocallyReceived,
            observed_plan: None,
            observed_tier: None,
            adapter_version: AdapterVersion::new("adapter-v1"),
            provider_contract_id: ProviderContractId::new("contract-v1"),
            meter_semantics_id: MeterSemanticsId::new("meter-v1"),
            normalized_fingerprint: "fp-single-source-can-run".to_string(),
        },
    )
    .expect("the observation must insert");
    insert_window(
        &conn,
        &NewMeterWindow {
            observation_id,
            semantic_key: WindowSemanticKey::new("five_hour"),
            scope: WindowScope::AccountWide,
            quota_used: QuotaUsed::new(QuotaFractionPpm::new(620_000).unwrap()),
            reported_resolution: ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap())
                .unwrap(),
            quantization: QuantizationSemantics::Exact,
            resets_at: WindowResetState::Known(UtcTimestamp::from_unix_nanos(
                at.unix_nanos() + 18_000_000_000_000,
            )),
            nominal_duration: NominalWindowDuration::from_nanos(18_000_000_000_000),
        },
    )
    .expect("the window must insert");
}

/// The in-process half of the `aub-cab.6` proof: every consumer's repository
/// read against the seeded ledger must name `expected_id`, and the values
/// derived from the shared row must equal the asserted points and headroom.
///
/// This pins the consumers to the shared row rather than to coinciding
/// labels: `calibrate show` reports `load_active_at`, spend converts through
/// `conversion::convert` over that same row, and can-run's per-window
/// headroom is `window_credit_headroom` over the row's uncertainty. A local
/// coefficient copy in any consumer would keep the superseded value here
/// while the binary moved on, failing the matching binary assertion below.
fn assert_library_reads(
    state: &StateDir,
    expected_id: &str,
    expected_points: i32,
    expected_headroom_micros: i64,
) {
    let at = UtcTimestamp::from_unix_nanos(now_nanos());
    let path = state.path().join(connection::LEDGER_DATABASE_FILE);
    let conn = connection::open(
        &path,
        AccessMode::ReadWrite,
        &PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1_000),
        },
    )
    .expect("the ledger must already exist and open");
    let scope = CalibrationScope {
        provider: ProviderKey::new("anthropic"),
        plan_tier: PlanTier::new("default"),
        window_semantic_key: WindowSemanticKey::new("five_hour"),
    };
    let calibration = load_active_at(&conn, &scope, at)
        .expect("the active calibration must load")
        .expect("one calibration must be active");
    assert_eq!(calibration.id().as_str(), expected_id);

    let model = agent_usage_book::store::cost_model::load_active_at(&conn, at)
        .expect("the active cost model must load")
        .expect("one cost model must be active");
    let credits = Derivation::Available(Qualified::new(
        Credits::from_micros(3_000_000),
        CoverageCompleteness::Complete,
        EvidenceQuality::Measured,
        Provenance::new(["cost-model:anthropic-claude-messages-v1".to_string()]),
    ));
    let context = WindowConversionContext::new(
        Some("spend".to_string()),
        ProviderKey::new("anthropic"),
        PlanTier::new("default"),
        WindowSemanticKey::new("five_hour"),
        calibration.meter_semantics_id().clone(),
        model.billing_semantics_id().clone(),
        Some(model.id().clone()),
    );
    let health = compute_health(
        &HealthInputs {
            calibration: &CalibrationFacts {
                plan_tier: calibration.plan_tier().clone(),
                meter_semantics_id: calibration.meter_semantics_id().clone(),
                billing_semantics_id: calibration.billing_semantics_id().clone(),
            },
            context: &ApplicabilityContext {
                plan_tier: context.plan_tier.clone(),
                meter_semantics_id: context.meter_semantics_id.clone(),
                billing_semantics_id: context.billing_semantics_id.clone(),
            },
            lifecycle: LifecycleState::Active,
            cost_model_superseded: agent_usage_book::store::cost_model::is_superseded(
                &conn,
                model.id(),
            )
            .expect("the supersession check must run"),
            drift: None,
            review_due_at: None,
        },
        at,
    );
    assert_eq!(health, CalibrationHealth::Current);
    let converted = convert(&credits, &calibration, &context, health);
    let agent_usage_book::report::WindowEquivalentDerivation::Available(value) = converted else {
        panic!("the applicable calibration must convert: {converted:?}");
    };
    assert_eq!(value.calibration_id.as_str(), expected_id);
    assert_eq!(value.interval.lower().get(), expected_points);
    assert_eq!(value.interval.upper().get(), expected_points);

    let constraint =
        CalibratedWindowConstraint::new(calibration.uncertainty(), AdviceHealth::Current);
    let window = MeterWindow::new(
        WindowSemanticKey::new("five_hour"),
        WindowScope::AccountWide,
        QuotaUsed::new(QuotaFractionPpm::new(620_000).unwrap()),
        ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap(),
        QuantizationSemantics::Exact,
        UtcTimestamp::from_unix_nanos(at.unix_nanos() + 18_000_000_000_000),
        NominalWindowDuration::from_nanos(18_000_000_000_000),
    );
    let agent_usage_book::advice::headroom::WindowHeadroom::Known { headroom, .. } =
        window_credit_headroom(&window, Some(&constraint))
    else {
        panic!("the current calibration must yield known headroom");
    };
    assert_eq!(headroom.lower().micros(), expected_headroom_micros);
    assert_eq!(headroom.upper().micros(), expected_headroom_micros);
}

/// Seeds the `task ingest` input the can-run history needs: three completed
/// tasks with the worked usage shapes, so the historical distribution has
/// n=3 rather than refusing as insufficient evidence.
fn seed_task_tracker(state: &StateDir) {
    let tracker = state.path().join("tracker/beads.db");
    std::fs::create_dir_all(tracker.parent().unwrap()).unwrap();
    let output = Command::new("sqlite3")
        .arg(&tracker)
        .arg(
            "CREATE TABLE events (
                id INTEGER PRIMARY KEY,
                issue_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                actor TEXT,
                old_value TEXT,
                new_value TEXT,
                created_at TEXT NOT NULL
            );
            INSERT INTO events (id, issue_id, event_type, actor, old_value, new_value, created_at) VALUES
             (1, 'aub-1', 'status_changed', 'agent-1', 'open', 'in_progress', '2026-08-25T00:30:00Z'),
             (2, 'aub-1', 'status_changed', 'agent-1', 'in_progress', 'closed', '2026-08-25T02:00:00Z'),
             (3, 'aub-2', 'status_changed', 'agent-1', 'open', 'in_progress', '2026-08-25T02:30:00Z'),
             (4, 'aub-2', 'status_changed', 'agent-1', 'in_progress', 'closed', '2026-08-25T04:00:00Z'),
             (5, 'aub-3', 'status_changed', 'agent-1', 'open', 'in_progress', '2026-08-25T04:30:00Z'),
             (6, 'aub-3', 'status_changed', 'agent-1', 'in_progress', 'closed', '2026-08-25T06:00:00Z');",
        )
        .output()
        .expect("sqlite3 must be spawnable for the tracker seed");
    assert!(
        output.status.success(),
        "the tracker seed must apply: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Seeds the task kinds no CLI resolves yet, directly against
/// `task_identity`, matching this suite's convention for tables with no
/// ingestion path of their own.
fn seed_task_identity(state: &StateDir) {
    let path = state.path().join(connection::LEDGER_DATABASE_FILE);
    let conn = connection::open(
        &path,
        AccessMode::ReadWrite,
        &PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1_000),
        },
    )
    .expect("the ledger must already exist and open");
    conn.execute_batch(
        "INSERT INTO task_identity (
            task_source, task_native, state, kind, winner_origin, evidence,
            normalization_version, size_state, size, size_evidence,
            difficulty_state, difficulty, difficulty_evidence
        ) VALUES
         ('beads', 'aub-1', 'resolved', 'task', 'tracker_field:kind', '{}', 1, 'unknown', NULL, '{}', 'unknown', NULL, '{}'),
         ('beads', 'aub-2', 'resolved', 'task', 'tracker_field:kind', '{}', 1, 'unknown', NULL, '{}', 'unknown', NULL, '{}'),
         ('beads', 'aub-3', 'resolved', 'task', 'tracker_field:kind', '{}', 1, 'unknown', NULL, '{}', 'unknown', NULL, '{}');",
    )
    .expect("the task kinds must insert");
}

/// Criterion (`aub-cab.6`): one seeded calibration identifier appears in
/// `calibrate show`, calibrated spend-conversion provenance and can-run
/// advice alike; append-only supersession moves all three in the same run;
/// no source or configuration change happens between the two halves.
///
/// The spend half keeps the clean single-session shape (1,000,000 input
/// tokens on account `spend`: 3.00 credits, exactly 10.0000 points at 30
/// micros/point, 30.0000 at 10). The can-run history lives on account
/// `work-primary` with three completed tasks, so one seeded repository
/// drives all three consumers without the two workloads sharing a number.
/// Every binary step points its endpoint at an unreachable port and the
/// state directory carries no credential file, so the whole test runs
/// without network or credentials.
#[test]
fn can_run_supersession_moves_all_three_consumers_together() {
    let state = StateDir::new();

    let corpus = state.path().join("transcripts/claude-code/project");
    std::fs::create_dir_all(&corpus).unwrap();
    std::fs::create_dir_all(state.path().join("home")).unwrap();
    let mut body = String::new();
    body.push_str(
        "{\"type\":\"assistant\",\"timestamp\":\"2026-08-25T07:00:00.000Z\",\"sessionId\":\"s-spend\",\"message\":{\"id\":\"m-s-spend\",\"model\":\"claude-3-5-sonnet\",\"usage\":{\"input_tokens\":1000000,\"output_tokens\":0,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}\n",
    );
    for (session, input, output, at) in [
        ("s1", 1000, 500_000, "2026-08-25T01:00:00.000Z"),
        ("s2", 1000, 800_000, "2026-08-25T03:00:00.000Z"),
        ("s3", 1000, 1_100_000, "2026-08-25T05:00:00.000Z"),
    ] {
        body.push_str(&format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{at}\",\"sessionId\":\"{session}\",\"message\":{{\"id\":\"m-{session}\",\"usage\":{{\"input_tokens\":{input},\"output_tokens\":{output},\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}}}}\n",
        ));
    }
    std::fs::write(corpus.join("sessions.jsonl"), body).unwrap();

    // No credential file is created anywhere under this state directory:
    // every command below must succeed without reading one. The spend-only
    // account is named `spend` rather than `work` so its `account=spend`
    // marker cannot substring-match the `account=work-primary` lines.
    let config = format!(
        "state.dir = \"{}\"\n\n[[accounts]]\nname = \"spend\"\nprovider = \"anthropic\"\ncredential = {{ kind = \"file\", path = \"{}/creds/token.json\" }}\n\n[[accounts]]\nname = \"work-primary\"\nprovider = \"anthropic\"\ncredential = {{ kind = \"file\", path = \"{}/creds/token.json\" }}\n\n[task_distribution]\nmin_samples = 3\n\n[[transcripts]]\nname = \"claude-code\"\nroot = \"{}\"\npattern = \"**/*.jsonl\"\nformat = \"claude-code\"\n\n[tracker]\nkind = \"local\"\npath = \"{}/tracker\"\n",
        state.path().display(),
        state.path().display(),
        state.path().display(),
        state.path().join("transcripts/claude-code").display(),
        state.path().display(),
    );
    std::fs::write(state.path().join("aub.toml"), config).unwrap();
    seed_task_tracker(&state);

    run(&state, &["ingest", "transcripts"]);
    run(&state, &["task", "ingest"]);
    seed_task_identity(&state);
    // The spend session belongs to `spend`; the task sessions to
    // `work-primary`. Markers carry the logical account explicitly.
    for (session, account, observed) in [
        ("s-spend", "spend", "2026-08-25T06:30:00Z"),
        ("s1", "work-primary", "2026-08-25T00:30:00Z"),
        ("s2", "work-primary", "2026-08-25T00:30:00Z"),
        ("s3", "work-primary", "2026-08-25T00:30:00Z"),
    ] {
        let path = state.path().join(connection::LEDGER_DATABASE_FILE);
        let conn = connection::open(
            &path,
            AccessMode::ReadWrite,
            &PragmaPolicy {
                busy_timeout: MonotonicDuration::from_millis(1_000),
            },
        )
        .expect("the ingested ledger must already exist and open");
        insert_marker(
            &conn,
            &NewSessionAccountMarker {
                session_id: SessionId::new(
                    SourceNamespace::new("claude-code"),
                    NativeSessionId::new(session),
                ),
                observed_at: UtcTimestamp::parse_rfc3339(observed)
                    .expect("the marker timestamp must parse"),
                source_ordering_key: None,
                logical_account: account.to_string(),
                resolved_account_id: None,
                marker_source: MarkerSource::new("hook"),
                run_id: None,
                evidence_designation: EvidenceDesignation::ExplicitLauncherOrHook,
            },
        )
        .expect("the account marker must insert");
    }
    run(&state, &["__cost-model-fixture", "complete"]);
    seed_meter_five_hour(&state);

    // First half: the conspicuous 30 micros/point coefficient from the
    // two-consumer test, so the spend arithmetic stays exactly comparable.
    run(&state, &["__calibration-fixture", "five_hour", "30"]);

    // The endpoint is unreachable on purpose: any live fetch fails the
    // step, proving the cached path reads no network.
    const OFFLINE: [(&str, &str); 1] = [("AUB_ANTHROPIC_ENDPOINT", "http://127.0.0.1:9")];

    assert_library_reads(&state, "five_hour-fixture-calibration", 100_000, 11_400_000);

    let show_before = run(&state, &["calibrate", "show"]);
    assert!(
        show_before.contains("active window calibration five_hour-fixture-calibration\n"),
        "{show_before}"
    );
    assert!(
        show_before.contains("fitted:          30 micros/point"),
        "{show_before}"
    );

    let mut spend_before_args = vec!["spend"];
    spend_before_args.extend(SPEND_WINDOW_ARGS);
    spend_before_args.extend(["--window-equivalent", "five_hour", "--refresh", "never"]);
    let spend_before = run(&state, &spend_before_args);
    assert!(spend_before.contains("3.00 credits"), "{spend_before}");
    let spend_line_before = window_equivalent_line(&spend_before, "spend");
    assert!(
        spend_line_before.contains("[10.0000, 10.0000] percentage points"),
        "{spend_line_before}"
    );
    assert!(
        spend_line_before.contains("calibration five_hour-fixture-calibration)"),
        "{spend_line_before}"
    );

    let (can_run_before, can_run_before_stderr) = run_with_env(
        &state,
        &[
            "-v",
            "can-run",
            "--task-kind",
            "task",
            "--account",
            "work-primary",
            "--task-model",
            "sonnet",
            "--cached",
        ],
        &OFFLINE,
    );
    assert!(
        can_run_before.contains("calibration #five_hour-fixture-calibration  headroom"),
        "{can_run_before}"
    );
    assert!(
        can_run_before.contains("#five_hour-fixture-calibration, current"),
        "{can_run_before}"
    );
    assert!(
        can_run_before_stderr.contains("\"event\":\"run_started\""),
        "{can_run_before_stderr}"
    );
    assert!(
        can_run_before_stderr.contains("\"event\":\"report_rendered\""),
        "{can_run_before_stderr}"
    );
    let (can_run_json_before, _) = run_with_env(
        &state,
        &[
            "-v",
            "can-run",
            "--task-kind",
            "task",
            "--account",
            "work-primary",
            "--task-model",
            "sonnet",
            "--cached",
            "--format",
            "json",
        ],
        &OFFLINE,
    );
    let json_before: serde_json::Value =
        serde_json::from_str(&can_run_json_before).expect("can-run must emit valid JSON");
    assert_eq!(json_before["command"], "can-run");
    assert_eq!(
        json_before["outcome"]["windows"][0]["calibration_id"],
        "five_hour-fixture-calibration"
    );
    assert_eq!(
        json_before["outcome"]["windows"][0]["headroom"]["lower"],
        "11400000"
    );
    assert_eq!(
        json_before["outcome"]["windows"][0]["headroom"]["upper"],
        "11400000"
    );

    // Second half: append-only supersession, no source or configuration
    // edit. The coefficient is a third of the first, so the same credits
    // convert to three times the points and the headroom shrinks to a
    // third: any consumer holding a local copy keeps the old number.
    run(&state, &["__calibration-fixture", "five_hour", "10"]);

    assert_library_reads(
        &state,
        "five_hour-fixture-calibration-1",
        300_000,
        3_800_000,
    );

    let show_after = run(&state, &["calibrate", "show"]);
    assert!(
        show_after.contains("active window calibration five_hour-fixture-calibration-1"),
        "{show_after}"
    );
    assert!(
        show_after.contains("fitted:          10 micros/point"),
        "{show_after}"
    );
    assert!(
        !show_after.contains("active window calibration five_hour-fixture-calibration\n"),
        "the superseded record must no longer be the active one:\n{show_after}"
    );

    let mut spend_after_args = vec!["spend"];
    spend_after_args.extend(SPEND_WINDOW_ARGS);
    spend_after_args.extend(["--window-equivalent", "five_hour", "--refresh", "never"]);
    let spend_after = run(&state, &spend_after_args);
    let spend_line_after = window_equivalent_line(&spend_after, "spend");
    assert!(
        spend_line_after.contains("[30.0000, 30.0000] percentage points"),
        "{spend_line_after}"
    );
    assert!(
        spend_line_after.contains("calibration five_hour-fixture-calibration-1)"),
        "{spend_line_after}"
    );

    let (can_run_after, can_run_after_stderr) = run_with_env(
        &state,
        &[
            "-v",
            "can-run",
            "--task-kind",
            "task",
            "--account",
            "work-primary",
            "--task-model",
            "sonnet",
            "--cached",
        ],
        &OFFLINE,
    );
    assert!(
        can_run_after.contains("calibration #five_hour-fixture-calibration-1  headroom"),
        "{can_run_after}"
    );
    assert!(
        can_run_after.contains("#five_hour-fixture-calibration-1, current"),
        "{can_run_after}"
    );
    assert!(
        !can_run_after.contains("#five_hour-fixture-calibration, current"),
        "the superseded identifier must not survive in can-run output:\n{can_run_after}"
    );
    assert!(
        can_run_after_stderr.contains("\"event\":\"report_rendered\""),
        "{can_run_after_stderr}"
    );
    let (can_run_json_after, _) = run_with_env(
        &state,
        &[
            "-v",
            "can-run",
            "--task-kind",
            "task",
            "--account",
            "work-primary",
            "--task-model",
            "sonnet",
            "--cached",
            "--format",
            "json",
        ],
        &OFFLINE,
    );
    let json_after: serde_json::Value =
        serde_json::from_str(&can_run_json_after).expect("can-run must emit valid JSON");
    assert_eq!(
        json_after["outcome"]["windows"][0]["calibration_id"],
        "five_hour-fixture-calibration-1"
    );
    assert_eq!(
        json_after["outcome"]["windows"][0]["headroom"]["lower"],
        "3800000"
    );
    assert_eq!(
        json_after["outcome"]["windows"][0]["headroom"]["upper"],
        "3800000"
    );

    assert_ne!(spend_line_before, spend_line_after);
    assert_ne!(can_run_before, can_run_after);
}
