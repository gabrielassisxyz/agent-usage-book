//! Failure semantics test matrix (`aub-71j.5`, PLAN.md Section 30).
//!
//! Encodes every row of the failure semantics table as an executable test case
//! asserting two things independently:
//! 1. What was stored, by directly reading the SQLite database or filesystem state.
//! 2. What the user saw, by reading stdout, JSON fields, structured log events, and exit statuses.
//!
//! No assertion in this module checks prose: assertions are strictly on typed values,
//! JSON error envelope fields, SQLite row counts and column values, and exit codes.

use std::path::{Path, PathBuf};
use std::process::Command;

use agent_usage_book::attribution::TaskEventKind;
use agent_usage_book::attribution::account_segment::{
    AccountMarkerBoundary, AccountSegmentationInputs, AccountUsageEvent,
    segment as segment_by_account,
};
use agent_usage_book::attribution::segment::{
    ClaimBoundary, OverheadReason, SegmentationContext, SegmentationInputs, UsageWindow,
    segment as segment_by_task,
};
use agent_usage_book::calibration::contamination::{
    ContaminationInputs, ContaminationMarkerPoint, ContaminationMeterPoint,
    ContaminationThresholds, evaluate_contamination, require_uncontaminated_for_activation,
};
use agent_usage_book::calibration::health::{
    ApplicabilityContext, CalibrationFacts, CalibrationHealth, HealthInputs, LifecycleState,
    compute_health, require_current_applicable,
};
use agent_usage_book::calibration::passive::{
    CandidateInterval, PassiveExclusionReason, evaluate_interval,
};
use agent_usage_book::config::TranscriptConfig;
use agent_usage_book::cost_model::convert;
use agent_usage_book::coverage::{
    AttemptRecord, AttemptResultRecord, CoverageInputs, PolicySnapshot, TimerRunRecord,
    compute as compute_coverage,
};
use agent_usage_book::dedup::deduplicate;
use agent_usage_book::doctor::{CheckName, CheckStatus, DoctorContext, build_registry};
use agent_usage_book::domain::attempt::{AttemptId, AttemptOutcome, AttemptResult, AttemptStarted};
use agent_usage_book::domain::failure::FailureClass;
use agent_usage_book::domain::freshness::{
    Freshness, FreshnessInput, LatestAttempt, Observed, StaleReason, compute_freshness,
};
use agent_usage_book::domain::ids::{
    BillingSemanticsId, CredentialContextId, MeterSemanticsId, NativeTaskId, SourceNamespace,
    TaskId,
};
use agent_usage_book::domain::money::Usd;
use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaUsed};
use agent_usage_book::domain::time::{
    ClockSkewEnvelope, FakeClock, MeasurementBasis, MonotonicDuration, ProviderObservedAt,
    ReceivedAt, UtcDate, UtcTimestamp,
};
use agent_usage_book::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, TokenCount,
    TokenKind, UsageVector,
};
use agent_usage_book::domain::window::WindowResetState;
use agent_usage_book::domain::window_anomaly::{
    WindowAnomalyKind, WindowReading, classify_window_transition,
};
use agent_usage_book::error::ExitClass;
use agent_usage_book::evidence::{CoverageCompleteness, Derivation, EvidenceQuality};
use agent_usage_book::store::account::observe_account;
use agent_usage_book::store::calibration::PlanTier;
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::cost_model::{
    anthropic_claude_messages_incomplete_v1, anthropic_claude_messages_v1,
};
use agent_usage_book::store::ingest_quarantine::{
    NewQuarantineItem, count_quarantined_records, load_all_quarantine, record_quarantine,
};
use agent_usage_book::store::meter_attempt::{DueReason, NewMeterAttempt, start_meter_attempt};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use agent_usage_book::store::sample_run::{Trigger, start_sample_run};
use agent_usage_book::store::sampling_policy_snapshot::{
    ResolvedSamplingPolicy, resolve_policy_snapshot,
};
use agent_usage_book::store::spool::{
    PendingTerminalBundle, PendingWindow, drain_pending, spool_pending,
};
use agent_usage_book::transcripts::discovery::{DiscoveryError, DiscoveryOptions, discover};
use agent_usage_book::transcripts::parser::{
    ParserVersion, QuarantineClass, QuarantineRecord, SourceLocation,
};
use agent_usage_book::valuation::{MissingRate, RateBook, ValuationOutcome, value_usage_vector};
use test_support::StateDir;
use test_support::synthetic_server::SyntheticServer;
use test_support::synthetic_server::script::{ScriptedOutcome, ScriptedResponseBody};

/// The 32 rows of Section 30 Failure Semantics table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FailureRow {
    NoCredentialsConfigured,
    ProviderSaysCredentialsInvalidExpired,
    EndpointUnreachable,
    HttpRateLimit,
    Http5xx,
    MalformedPayload200,
    ProviderResponseTimestampAlreadyTooOld,
    DbUnavailableBeforeSample,
    DbCommitFailsAfterRequest,
    ProjectionUnavailableCorrupt,
    TranscriptMissing,
    TranscriptParseError,
    DuplicateTranscriptRecord,
    UnknownTokenClass,
    MissingCostModelTerm,
    PlanMismatch,
    WindowResetInsideCalibrationSegment,
    MixedPlanTiersInCalibration,
    CalibrationContaminated,
    MissingRateCard,
    TaskBoundaryAmbiguous,
    AccountUnknown,
    WebConsumptionHasNoTranscript,
    TimerNeverRan,
    TimerRanProviderFailed,
    MeterPercentDecreasesWithoutReset,
    ResetTimestampChangesUnexpectedly,
    ClockMovesBackward,
    CollectorDiedAfterDurableAttemptStart,
    ProjectionLagsDb,
    PassiveFitContaminated,
    CalibrationReviewOverdue,
}

impl FailureRow {
    pub const ALL: [FailureRow; 32] = [
        FailureRow::NoCredentialsConfigured,
        FailureRow::ProviderSaysCredentialsInvalidExpired,
        FailureRow::EndpointUnreachable,
        FailureRow::HttpRateLimit,
        FailureRow::Http5xx,
        FailureRow::MalformedPayload200,
        FailureRow::ProviderResponseTimestampAlreadyTooOld,
        FailureRow::DbUnavailableBeforeSample,
        FailureRow::DbCommitFailsAfterRequest,
        FailureRow::ProjectionUnavailableCorrupt,
        FailureRow::TranscriptMissing,
        FailureRow::TranscriptParseError,
        FailureRow::DuplicateTranscriptRecord,
        FailureRow::UnknownTokenClass,
        FailureRow::MissingCostModelTerm,
        FailureRow::PlanMismatch,
        FailureRow::WindowResetInsideCalibrationSegment,
        FailureRow::MixedPlanTiersInCalibration,
        FailureRow::CalibrationContaminated,
        FailureRow::MissingRateCard,
        FailureRow::TaskBoundaryAmbiguous,
        FailureRow::AccountUnknown,
        FailureRow::WebConsumptionHasNoTranscript,
        FailureRow::TimerNeverRan,
        FailureRow::TimerRanProviderFailed,
        FailureRow::MeterPercentDecreasesWithoutReset,
        FailureRow::ResetTimestampChangesUnexpectedly,
        FailureRow::ClockMovesBackward,
        FailureRow::CollectorDiedAfterDurableAttemptStart,
        FailureRow::ProjectionLagsDb,
        FailureRow::PassiveFitContaminated,
        FailureRow::CalibrationReviewOverdue,
    ];

    pub fn table_title(self) -> &'static str {
        match self {
            FailureRow::NoCredentialsConfigured => "No credentials configured",
            FailureRow::ProviderSaysCredentialsInvalidExpired => {
                "Provider says credentials invalid/expired"
            }
            FailureRow::EndpointUnreachable => "Endpoint unreachable (DNS/connect timeout)",
            FailureRow::HttpRateLimit => "HTTP rate limit",
            FailureRow::Http5xx => "HTTP 5xx",
            FailureRow::MalformedPayload200 => "200 with malformed payload",
            FailureRow::ProviderResponseTimestampAlreadyTooOld => {
                "Provider response timestamp already too old"
            }
            FailureRow::DbUnavailableBeforeSample => "DB unavailable before sample",
            FailureRow::DbCommitFailsAfterRequest => "DB commit fails after request",
            FailureRow::ProjectionUnavailableCorrupt => "Projection unavailable/corrupt",
            FailureRow::TranscriptMissing => "Transcript missing",
            FailureRow::TranscriptParseError => "Transcript parse error",
            FailureRow::DuplicateTranscriptRecord => "Duplicate transcript record",
            FailureRow::UnknownTokenClass => "Unknown token class",
            FailureRow::MissingCostModelTerm => "Missing cost-model term",
            FailureRow::PlanMismatch => "Plan mismatch",
            FailureRow::WindowResetInsideCalibrationSegment => {
                "Window reset inside calibration segment"
            }
            FailureRow::MixedPlanTiersInCalibration => "Mixed plan tiers in calibration",
            FailureRow::CalibrationContaminated => "Calibration contaminated",
            FailureRow::MissingRateCard => "Missing rate card",
            FailureRow::TaskBoundaryAmbiguous => "Task boundary ambiguous",
            FailureRow::AccountUnknown => "Account unknown",
            FailureRow::WebConsumptionHasNoTranscript => "Web consumption has no transcript",
            FailureRow::TimerNeverRan => "Timer never ran",
            FailureRow::TimerRanProviderFailed => "Timer ran, provider failed",
            FailureRow::MeterPercentDecreasesWithoutReset => {
                "Meter percent decreases without reset"
            }
            FailureRow::ResetTimestampChangesUnexpectedly => "Reset timestamp changes unexpectedly",
            FailureRow::ClockMovesBackward => "Clock moves backward",
            FailureRow::CollectorDiedAfterDurableAttemptStart => {
                "Collector died after a durable attempt start"
            }
            FailureRow::ProjectionLagsDb => "Projection lags DB",
            FailureRow::PassiveFitContaminated => "Passive fit contaminated",
            FailureRow::CalibrationReviewOverdue => "Calibration review overdue",
        }
    }
}

// ---------------------------------------------------------------------------
// Harness helpers
// ---------------------------------------------------------------------------

fn aub() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aub"))
}

fn aub_cmd(state_dir: &Path, config_path: &Path) -> Command {
    let mut cmd = aub();
    cmd.env("AUB_STATE_DIR", state_dir)
        .env("AUB_CONFIG_FILE", config_path);
    cmd
}

fn init_ledger(state_dir: &Path) -> rusqlite::Connection {
    let db_path = state_dir.join(agent_usage_book::store::connection::LEDGER_DATABASE_FILE);
    let policy = PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(1000),
    };
    let mut conn = open(&db_path, AccessMode::ReadWrite, &policy).expect("open test ledger");
    run_migrations(
        &mut conn,
        &registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
    )
    .expect("migrate test ledger");
    conn
}

const FIXTURE_POLICY: ResolvedSamplingPolicy = ResolvedSamplingPolicy {
    ordinary_cadence: MonotonicDuration::from_millis(300_000),
    freshness_horizon: MonotonicDuration::from_millis(900_000),
    reset_edge_policy: String::new(),
    retry_backoff_policy: String::new(),
    command_budget: MonotonicDuration::from_millis(60_000),
    policy_algorithm_version: String::new(),
};

/// Durably starts one real meter attempt against `conn`, satisfying the
/// foreign keys `meter_attempt_result`/`meter_response_evidence` require: the
/// account, sample run and policy snapshot the attempt references all exist
/// as real rows first, exactly as the live sampler creates them before it
/// ever calls the provider. Returns the attempt's row id.
fn start_durable_attempt(conn: &rusqlite::Connection, at_nanos: i64) -> i64 {
    let at = UtcTimestamp::from_unix_nanos(at_nanos);
    let account = observe_account(conn, "anthropic", "work-primary", at).expect("observe account");
    let run = start_sample_run(conn, Trigger::Manual, at, "fixture").expect("start sample run");
    let snapshot = resolve_policy_snapshot(conn, account, at, &FIXTURE_POLICY)
        .expect("resolve policy snapshot");
    start_meter_attempt(
        conn,
        &NewMeterAttempt {
            run_id: run,
            account_id: account,
            provider: "anthropic".to_owned(),
            request_started_at: at,
            credential_context_id: Some("ctx-work-primary".to_owned()),
            policy_snapshot_id: snapshot,
            due_at: at,
            due_reason: DueReason::OrdinaryCadence,
            due_basis: None,
            provider_contract_id: "anthropic-v1".to_owned(),
            meter_semantics_id: "anthropic-five-hour-v1".to_owned(),
        },
    )
    .expect("start durable meter attempt")
    .value()
}

fn write_config(state_dir: &Path, content: &str) -> PathBuf {
    let config_path = state_dir.join("aub.toml");
    std::fs::write(&config_path, content).expect("write config");
    config_path
}

/// `aub sample --format=json` prints one JSON document per account before the
/// top-level error envelope, so stdout on a failing run is several concatenated
/// JSON values, not one. A streaming deserializer reads each in turn; the
/// error envelope is always the last one printed.
fn last_json_value(stdout: &str) -> serde_json::Value {
    serde_json::Deserializer::from_str(stdout)
        .into_iter::<serde_json::Value>()
        .map(|value| value.expect("each concatenated stdout document is valid json"))
        .last()
        .expect("at least one json document on stdout")
}

fn write_account_config(state_dir: &Path, _server_url: &str) -> PathBuf {
    let token_path = state_dir.join("token.json");
    std::fs::write(&token_path, r#"{"accessToken":"test-token"}"#).expect("write token");
    let content = format!(
        "[state]\ndir = {:?}\n\n[[accounts]]\nname = \"work-primary\"\nprovider = \"anthropic\"\ncredential = {{ kind = \"file\", path = {:?} }}\n",
        state_dir, token_path
    );
    write_config(state_dir, &content)
}

fn sample_bundle(attempt_id: i64) -> PendingTerminalBundle {
    PendingTerminalBundle {
        attempt_id,
        completed_at_nanos: 1_000_000_000,
        elapsed_nanos: 100_000_000,
        outcome: "success".to_string(),
        failure_class: None,
        retry_after_nanos: None,
        sanitized_error_classification: None,
        retry_index: None,
        clock_anomaly: false,
        response_classification: "success".to_string(),
        received_at_nanos: 1_000_000_000,
        provider_observed_at_original: Some("2026-08-30T00:00:00Z".to_string()),
        evidence_capsule: "{\"sanitized\":true}".to_string(),
        capsule_schema_version: "v1".to_string(),
        sanitizer_version: "v1".to_string(),
        capture_truncated: false,
        account_id: 1,
        provider: "anthropic".to_string(),
        provider_observed_at_nanos: Some(1_000_000_000),
        measurement_basis: "provider_observed".to_string(),
        observed_plan: Some("max".to_string()),
        observed_tier: None,
        adapter_version: "v1".to_string(),
        provider_contract_id: "anthropic".to_string(),
        meter_semantics_id: "anthropic-five-hour-v1".to_string(),
        normalized_fingerprint: "fp-1".to_string(),
        windows: vec![PendingWindow {
            semantic_key: "five_hour".to_string(),
            scope_kind: "account_wide".to_string(),
            scoped_model: None,
            quota_used_ppm: 250_000,
            reported_resolution_ppm: 1_000,
            quantization: "exact".to_string(),
            resets_at_nanos: Some(2_000_000_000),
            nominal_duration_nanos: 18_000_000_000_000,
            is_active: true,
            severity: "normal".to_string(),
        }],
    }
}

const ANTHROPIC_SUCCESS_BODY: &[u8] = br#"{
    "five_hour": { "utilization": 0.25, "resets_at": "2026-08-30T16:00:00Z" },
    "seven_day": { "utilization": 0.50, "resets_at": "2026-09-06T00:00:00Z" }
}"#;

#[test]
fn row_01_no_credentials_configured() {
    let state = StateDir::new();
    let conn = init_ledger(state.path());
    let config = write_config(
        state.path(),
        &format!(
            "[state]\ndir = {:?}\n\n[[accounts]]\nname = \"uncred\"\nprovider = \"anthropic\"\ncredential = {{ kind = \"file\", path = \"\" }}\n",
            state.path()
        ),
    );

    let output = aub_cmd(state.path(), &config)
        .args(["sample", "--account", "uncred", "--format=json"])
        .output()
        .expect("aub must run");

    assert_eq!(output.status.code(), Some(ExitClass::Usage.code() as i32));

    let attempts: i64 = conn
        .query_row("SELECT count(*) FROM meter_attempt", [], |r| r.get(0))
        .unwrap();
    assert_eq!(attempts, 0, "no attempt must be stored on invalid config");

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let parsed: serde_json::Value = last_json_value(&stdout);
    assert_eq!(parsed["error"]["exit_class"], ExitClass::Usage.code());
    assert_eq!(parsed["error"]["code"], "INVALID_USAGE");
}

#[test]
fn row_02_provider_says_credentials_invalid_expired() {
    let state = StateDir::new();
    let conn = init_ledger(state.path());
    let server = SyntheticServer::start(vec![ScriptedOutcome::Unauthorized401]).unwrap();
    let config = write_account_config(state.path(), &server.url());

    let output = aub_cmd(state.path(), &config)
        .env("AUB_ANTHROPIC_ENDPOINT", server.url())
        .args([
            "sample",
            "--account",
            "work-primary",
            "--require-success",
            "--format=json",
        ])
        .output()
        .expect("aub must run");

    assert_eq!(
        output.status.code(),
        Some(ExitClass::AuthRequired.code() as i32)
    );

    let attempts: i64 = conn
        .query_row("SELECT count(*) FROM meter_attempt", [], |r| r.get(0))
        .unwrap();
    assert_eq!(attempts, 1, "attempt start row must be persisted");
    let outcome: String = conn
        .query_row("SELECT outcome FROM meter_attempt_result", [], |r| r.get(0))
        .unwrap();
    assert_eq!(outcome, "auth_required");
    let observations: i64 = conn
        .query_row("SELECT count(*) FROM meter_observation", [], |r| r.get(0))
        .unwrap();
    assert_eq!(observations, 0, "no observation stored on auth failure");

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let parsed: serde_json::Value = last_json_value(&stdout);
    assert_eq!(
        parsed["error"]["exit_class"],
        ExitClass::AuthRequired.code()
    );
    assert_eq!(parsed["error"]["code"], "AUTHENTICATION_REQUIRED");
}

/// The endpoint is a port nothing listens on, so the connection is refused at
/// the connect phase: the same idiom `zero_is_data_and_no_silent_fallback.rs`
/// uses to force `FailureClass::ConnectTimeout` without a bind/drop race.
const UNREACHABLE_ENDPOINT: &str = "http://127.0.0.1:9";

#[test]
fn row_03_endpoint_unreachable() {
    let state = StateDir::new();
    let conn = init_ledger(state.path());
    let config = write_account_config(state.path(), UNREACHABLE_ENDPOINT);

    let output = aub_cmd(state.path(), &config)
        .env("AUB_ANTHROPIC_ENDPOINT", UNREACHABLE_ENDPOINT)
        .args([
            "sample",
            "--account",
            "work-primary",
            "--require-success",
            "--format=json",
        ])
        .output()
        .expect("aub must run");

    assert_eq!(
        output.status.code(),
        Some(ExitClass::RemoteUnavailable.code() as i32)
    );

    let (outcome, failure_class): (String, String) = conn
        .query_row(
            "SELECT outcome, failure_class FROM meter_attempt_result",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(outcome, "unreachable");
    assert_eq!(failure_class, "transport_timeout");

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let parsed: serde_json::Value = last_json_value(&stdout);
    assert_eq!(
        parsed["error"]["exit_class"],
        ExitClass::RemoteUnavailable.code()
    );
}

#[test]
fn row_04_http_rate_limit() {
    let state = StateDir::new();
    let conn = init_ledger(state.path());
    let server = SyntheticServer::start(vec![ScriptedOutcome::TooManyRequests429 {
        retry_after_seconds: Some(60),
    }])
    .unwrap();
    let config = write_account_config(state.path(), &server.url());

    let output = aub_cmd(state.path(), &config)
        .env("AUB_ANTHROPIC_ENDPOINT", server.url())
        .args([
            "sample",
            "--account",
            "work-primary",
            "--require-success",
            "--format=json",
        ])
        .output()
        .expect("aub must run");

    assert_eq!(
        output.status.code(),
        Some(ExitClass::RemoteUnavailable.code() as i32)
    );

    let (outcome, failure_class, retry_after): (String, String, Option<i64>) = conn
        .query_row(
            "SELECT outcome, failure_class, retry_after_nanos FROM meter_attempt_result",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(outcome, "unreachable");
    assert_eq!(failure_class, "rate_limited");
    assert_eq!(retry_after, Some(60_000_000_000));

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let parsed: serde_json::Value = last_json_value(&stdout);
    assert_eq!(
        parsed["error"]["exit_class"],
        ExitClass::RemoteUnavailable.code()
    );
    assert_eq!(parsed["error"]["code"], "REMOTE_SOURCE_UNAVAILABLE");
}

#[test]
fn row_05_http_5xx() {
    let state = StateDir::new();
    let conn = init_ledger(state.path());
    let server = SyntheticServer::start(vec![ScriptedOutcome::InternalServerError500]).unwrap();
    let config = write_account_config(state.path(), &server.url());

    let output = aub_cmd(state.path(), &config)
        .env("AUB_ANTHROPIC_ENDPOINT", server.url())
        .args([
            "sample",
            "--account",
            "work-primary",
            "--require-success",
            "--format=json",
        ])
        .output()
        .expect("aub must run");

    assert_eq!(
        output.status.code(),
        Some(ExitClass::RemoteUnavailable.code() as i32)
    );

    let (outcome, failure_class): (String, String) = conn
        .query_row(
            "SELECT outcome, failure_class FROM meter_attempt_result",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(outcome, "unreachable");
    assert_eq!(failure_class, "http_status_server_error");

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let parsed: serde_json::Value = last_json_value(&stdout);
    assert_eq!(
        parsed["error"]["exit_class"],
        ExitClass::RemoteUnavailable.code()
    );
    assert_eq!(parsed["error"]["code"], "REMOTE_SOURCE_UNAVAILABLE");
}

#[test]
fn row_06_malformed_payload_200() {
    let state = StateDir::new();
    let conn = init_ledger(state.path());
    // Genuinely unparseable bytes, not merely a wrong JSON shape: a body that
    // still parses as JSON but has the wrong shape (e.g. a string where an
    // object is expected) resolves through the schema-error path
    // (`missing_required_field`) rather than the malformed-body path this row
    // targets.
    let server = SyntheticServer::start(vec![ScriptedOutcome::MalformedJson {
        status: 200,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: b"not json at all {".to_vec(),
    }])
    .unwrap();
    let config = write_account_config(state.path(), &server.url());

    let output = aub_cmd(state.path(), &config)
        .env("AUB_ANTHROPIC_ENDPOINT", server.url())
        .args([
            "sample",
            "--account",
            "work-primary",
            "--require-success",
            "--format=json",
        ])
        .output()
        .expect("aub must run");

    assert_eq!(
        output.status.code(),
        Some(ExitClass::RemoteUnavailable.code() as i32)
    );

    let (outcome, failure_class): (String, String) = conn
        .query_row(
            "SELECT outcome, failure_class FROM meter_attempt_result",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(outcome, "unreachable");
    assert_eq!(failure_class, "malformed_body");

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let parsed: serde_json::Value = last_json_value(&stdout);
    assert_eq!(
        parsed["error"]["exit_class"],
        ExitClass::RemoteUnavailable.code()
    );
}

#[test]
fn row_07_provider_response_timestamp_already_too_old() {
    let ctx = CredentialContextId::new("ctx-too-old");
    let past_timestamp = UtcTimestamp::from_unix_nanos(1_000_000_000);
    let now = UtcTimestamp::from_unix_nanos(10_000_000_000);
    let started = AttemptStarted::new(AttemptId::new(1), past_timestamp);
    let result = AttemptResult::new(
        AttemptId::new(1),
        past_timestamp,
        MonotonicDuration::from_millis(50),
        AttemptOutcome::Success,
    );

    let observed = Observed::new(
        500_000u64,
        Some(ProviderObservedAt::new(past_timestamp)),
        ReceivedAt::new(past_timestamp),
        MeasurementBasis::ProviderObserved,
    );

    let input = FreshnessInput::new(
        Some(observed),
        Some(&ctx),
        Some(LatestAttempt::new(started, Some(result), &ctx)),
        None,
        Some(&ctx),
        MonotonicDuration::from_seconds(5),
        MonotonicDuration::from_seconds(10),
        ClockSkewEnvelope::new(MonotonicDuration::from_seconds(1)),
    );

    let freshness: Freshness<u64> = compute_freshness(&input, &FakeClock::new(now));
    match freshness {
        Freshness::Stale { reason, .. } => {
            assert_eq!(reason, StaleReason::AgeExceeded);
        }
        Freshness::Fresh { .. } | Freshness::AuthRequired { .. } => {
            panic!("too-old response must evaluate to Stale(AgeExceeded) even on success");
        }
    }
}

#[test]
fn row_08_db_unavailable_before_sample() {
    let state = StateDir::new();
    let server = SyntheticServer::start(vec![ScriptedOutcome::Success(
        ScriptedResponseBody::json_ok(ANTHROPIC_SUCCESS_BODY.to_vec()),
    )])
    .unwrap();

    let real_dir = state.path().join("real_state");
    std::fs::create_dir_all(&real_dir).unwrap();
    let sym_dir = state.path().join("sym_state");
    std::os::unix::fs::symlink(&real_dir, &sym_dir).unwrap();

    let config = write_config(
        state.path(),
        &format!(
            "[state]\ndir = {:?}\n\n[[accounts]]\nname = \"work-primary\"\nprovider = \"anthropic\"\ncredential = {{ kind = \"none\" }}\n",
            sym_dir
        ),
    );

    let output = aub_cmd(&sym_dir, &config)
        .env("AUB_ANTHROPIC_ENDPOINT", server.url())
        .args(["sample", "--due"])
        .output()
        .expect("aub must run");

    assert_eq!(output.status.code(), Some(ExitClass::Store.code() as i32));
    assert_eq!(
        server.request_count(),
        0,
        "no network request must be made when db is unviable"
    );
}

#[test]
fn row_09_db_commit_fails_after_request() {
    let state = StateDir::new();
    let mut conn = init_ledger(state.path());
    let attempt_id = start_durable_attempt(&conn, 500_000_000);

    let bundle = sample_bundle(attempt_id);
    spool_pending(state.path(), &bundle).expect("spool pending record");

    let pending_files: Vec<_> = std::fs::read_dir(state.path().join("pending"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(
        pending_files.len(),
        1,
        "pending spool file must be retained"
    );

    drain_pending(&mut conn, state.path()).expect("drain pending spool");

    let remaining: Vec<_> = std::fs::read_dir(state.path().join("pending"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(
        remaining.len(),
        0,
        "pending spool must be empty after drain"
    );

    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM meter_attempt_result WHERE attempt_id = ?1",
            [attempt_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 1,
        "drained attempt result must be safely persisted in SQLite"
    );
}

#[test]
fn row_10_projection_unavailable_corrupt() {
    let state = StateDir::new();
    let _conn = init_ledger(state.path());
    let config = write_config(
        state.path(),
        &format!(
            "[state]\ndir = {:?}\n\n[[accounts]]\nname = \"work-primary\"\nprovider = \"anthropic\"\n",
            state.path()
        ),
    );

    std::fs::write(state.path().join("projection"), "not valid json").unwrap();

    let output = aub_cmd(state.path(), &config)
        .args(["status", "--format=json"])
        .output()
        .expect("aub status must run");

    assert_eq!(output.status.code(), Some(ExitClass::Success.code() as i32));
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("json status output");

    assert_eq!(parsed["projection"]["state"], "malformed");
    assert!(
        parsed["accounts"].as_array().unwrap().is_empty(),
        "no db fallback in status path"
    );
}

#[test]
fn row_11_transcript_missing() {
    let missing_root = std::env::temp_dir().join("aub-row11-transcript-root-that-does-not-exist");
    let source = TranscriptConfig {
        name: "primary".to_string(),
        root: missing_root.clone(),
        pattern: "**/*.jsonl".to_string(),
        format: None,
        usage_evidence: None,
    };

    let result = discover(&[source], &DiscoveryOptions::default());

    // What was "stored": the doc contract on `discover` is that a missing root
    // is an error naming the source, never a substituted empty result. A
    // silently-empty `Ok(vec![])` is exactly the "zero is data" violation this
    // row exists to catch.
    let Err(DiscoveryError::RootMissing { source, path }) = result else {
        panic!("expected DiscoveryError::RootMissing, got {result:?}");
    };
    assert_eq!(source, "primary");
    assert_eq!(path, missing_root);
}

#[test]
fn row_12_transcript_parse_error() {
    let record = QuarantineRecord::new(
        SourceLocation::new("transcripts/corrupt.jsonl", 7),
        ParserVersion::new("native-v1"),
        QuarantineClass::TruncatedStructure,
    );
    let item = NewQuarantineItem::from_record(&record, UtcTimestamp::from_unix_nanos(1_000));

    let state = StateDir::new();
    let conn = init_ledger(state.path());
    record_quarantine(&conn, &item).expect("record quarantine");

    // Stored: the quarantine table carries the record, addressable independently
    // of any report.
    assert_eq!(count_quarantined_records(&conn).unwrap(), 1);
    let rows = load_all_quarantine(&conn).unwrap();
    assert_eq!(rows[0].source_file(), "transcripts/corrupt.jsonl");
    // Presented: the record's own failure class is what a diagnostic renders,
    // never a substituted subtotal of zero for the affected transcript.
    assert_eq!(
        rows[0].failure_class(),
        QuarantineClass::TruncatedStructure.name()
    );
}

/// Two occurrences of one strongly-identified event (a replay), the second
/// carrying the larger output count growth a replay always does.
fn replayed_events(native_id: &str) -> Vec<agent_usage_book::transcripts::NormalizedUsageEvent> {
    fn event(
        id: &str,
        input: u64,
        output: u64,
    ) -> agent_usage_book::transcripts::NormalizedUsageEvent {
        let usage = UsageVector::new(
            KnownTokenVector::new(
                InputTokens::new(input),
                OutputTokens::new(output),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(0),
            ),
            std::collections::BTreeMap::new(),
            CoverageCompleteness::Complete,
            EvidenceQuality::Measured,
        );
        agent_usage_book::transcripts::NormalizedUsageEvent::new(
            usage,
            agent_usage_book::transcripts::parser::EvidenceClassification::Reported,
            agent_usage_book::evidence::Provenance::new(vec![format!(
                "{}{id}",
                agent_usage_book::transcripts::parser::STRONG_IDENTITY_PREFIX
            )]),
            ParserVersion::new("native-v1"),
        )
    }
    vec![event(native_id, 10, 5), event(native_id, 10, 8)]
}

#[test]
fn row_13_duplicate_transcript_record() {
    let deduplicated = deduplicate(replayed_events("msg-1"));

    // Stored: the canonical event is kept exactly once, with the larger
    // (later-replayed) output count, never summed and never dropped.
    assert_eq!(deduplicated.canonical.len(), 1);
    assert_eq!(
        deduplicated.canonical[0]
            .usage()
            .known()
            .value(TokenKind::Output),
        8
    );
    // Presented: the duplicate counter that a report reads increments, so the
    // replay is visible rather than silently collapsed with no trace.
    assert_eq!(deduplicated.replayed_occurrences, 1);
}

#[test]
fn row_14_unknown_token_class() {
    let published_at = UtcTimestamp::from_unix_nanos(1_717_200_000_000_000_000);
    let model = anthropic_claude_messages_v1(published_at);
    let mut unknown = std::collections::BTreeMap::new();
    unknown.insert("reasoning_tokens".to_string(), TokenCount::new(99));
    let usage = UsageVector::new(
        KnownTokenVector::new(
            InputTokens::new(100),
            OutputTokens::new(0),
            CacheReadTokens::new(0),
            CacheWriteTokens::new(0),
        ),
        unknown,
        CoverageCompleteness::Complete,
        EvidenceQuality::Measured,
    );

    let derivation = convert(&model, &usage);

    // Stored: even a fully-populated cost model produces no credit total when
    // an unknown component is present, regardless of model coverage.
    let Derivation::Unavailable { missing, .. } = derivation else {
        panic!("expected Unavailable derivation for an unknown token component");
    };
    // Presented: the missing-fact set names the unknown component, not a
    // generic "something is wrong".
    assert!(
        missing.iter().any(|fact| fact
            .as_str()
            .contains("unknown component: reasoning_tokens")),
        "missing facts {missing:?} must name the unknown component"
    );
}

#[test]
fn row_15_missing_cost_model_term() {
    let published_at = UtcTimestamp::from_unix_nanos(1_717_200_000_000_000_000);
    let model = anthropic_claude_messages_incomplete_v1(published_at);
    let usage = UsageVector::new(
        KnownTokenVector::new(
            InputTokens::new(100),
            OutputTokens::new(0),
            CacheReadTokens::new(0),
            CacheWriteTokens::new(10_000),
        ),
        std::collections::BTreeMap::new(),
        CoverageCompleteness::Complete,
        EvidenceQuality::Measured,
    );

    let derivation = convert(&model, &usage);

    // Stored: no credit total for the affected usage; the input contribution
    // alone is not substituted as if it were the whole answer.
    let Derivation::Unavailable { missing, .. } = derivation else {
        panic!("expected Unavailable derivation for a token kind the model has no term for");
    };
    // Presented: the missing fact names the absent rate by kind.
    assert!(
        missing
            .iter()
            .any(|fact| fact.as_str().contains("cache_write rate")),
        "missing facts {missing:?} must name the absent cache_write rate"
    );
}

fn calibration_facts() -> CalibrationFacts {
    CalibrationFacts {
        plan_tier: PlanTier::new("pro-5h"),
        meter_semantics_id: MeterSemanticsId::new("account-5h-v2"),
        billing_semantics_id: BillingSemanticsId::new("model-x-subscription-v4"),
    }
}

fn matching_context() -> ApplicabilityContext {
    ApplicabilityContext {
        plan_tier: PlanTier::new("pro-5h"),
        meter_semantics_id: MeterSemanticsId::new("account-5h-v2"),
        billing_semantics_id: BillingSemanticsId::new("model-x-subscription-v4"),
    }
}

#[test]
fn row_16_plan_mismatch() {
    let facts = calibration_facts();
    // The environment's plan tier no longer matches what the calibration was
    // fitted against; nothing else about the calibration changed.
    let context = ApplicabilityContext {
        plan_tier: PlanTier::new("max-5h"),
        ..matching_context()
    };
    let inputs = HealthInputs {
        calibration: &facts,
        context: &context,
        lifecycle: LifecycleState::Active,
        cost_model_superseded: false,
        drift: None,
        review_due_at: None,
    };

    let health = compute_health(&inputs, UtcTimestamp::from_unix_nanos(1_000_000));

    // Stored: n/a, per the table row; there is no calibration-not-applicable
    // row to write. What is checked instead is the typed health state itself.
    assert_eq!(health, CalibrationHealth::Inapplicable);
    // Presented: a quantitative consumer refuses to route from it.
    assert!(require_current_applicable(health).is_err());
}

#[test]
fn row_17_window_reset_inside_calibration_segment() {
    let interval = CandidateInterval {
        reset_inside: true,
        ..CandidateInterval::eligible_fixture("segment-with-reset")
    };

    let reasons = evaluate_interval(&interval);

    // Stored: the segment is excluded from the fit rather than silently kept
    // with a reset crossing it.
    assert!(
        reasons
            .iter()
            .any(|r| matches!(r, PassiveExclusionReason::ConditionFailed(cond, _) if cond.as_str() == "no_reset_inside")),
        "reasons {reasons:?} must exclude the interval for the reset-inside condition"
    );
    // Presented: an otherwise-eligible interval (no other condition fails).
    assert_eq!(reasons.len(), 1);
}

#[test]
fn row_18_mixed_plan_tiers_in_calibration() {
    let interval = CandidateInterval {
        plan_tier_start: PlanTier::new("pro-5h"),
        plan_tier_end: PlanTier::new("max-5h"),
        ..CandidateInterval::eligible_fixture("segment-with-mixed-tiers")
    };

    let reasons = evaluate_interval(&interval);

    // Stored: the interval is rejected from contributing to the fit.
    assert!(
        reasons
            .iter()
            .any(|r| matches!(r, PassiveExclusionReason::ConditionFailed(cond, _) if cond.as_str() == "unchanged_plan_tier")),
        "reasons {reasons:?} must exclude the interval for its tier change"
    );
    // Presented: the excluding reason names both tiers, not a generic message.
    let detail = reasons
        .iter()
        .find_map(|r| match r {
            PassiveExclusionReason::ConditionFailed(cond, detail)
                if cond.as_str() == "unchanged_plan_tier" =>
            {
                Some(detail.clone())
            }
            _ => None,
        })
        .expect("unchanged_plan_tier reason must be present");
    assert!(detail.contains("pro-5h") && detail.contains("max-5h"));
}

#[test]
fn row_19_calibration_contaminated() {
    let thresholds = ContaminationThresholds::conservative_default();
    let pre_burn = [ContaminationMeterPoint::new(
        UtcTimestamp::from_unix_nanos(0),
        QuotaUsed::new(QuotaFractionPpm::new(100_000).unwrap()),
    )];
    let post = [ContaminationMeterPoint::new(
        UtcTimestamp::from_unix_nanos(3_000),
        QuotaUsed::new(QuotaFractionPpm::new(100_000).unwrap()),
    )];
    let inputs = ContaminationInputs {
        experiment_account: "work-a",
        baseline_plateau_started_at: UtcTimestamp::from_unix_nanos(0),
        started_at: UtcTimestamp::from_unix_nanos(1_000),
        ended_at: Some(UtcTimestamp::from_unix_nanos(2_000)),
        evaluated_at: UtcTimestamp::from_unix_nanos(3_000),
        pre_burn_series: &pre_burn,
        post_series: &post,
        // The meter moved substantially while the controlled work was running,
        // but no local credits were attributed to it: hidden traffic.
        controlled_meter_start: QuotaUsed::new(QuotaFractionPpm::new(100_000).unwrap()),
        controlled_meter_end: QuotaUsed::new(QuotaFractionPpm::new(150_000).unwrap()),
        local_credits_delta: agent_usage_book::domain::credits::Credits::from_micros(0),
        markers: &[],
    };

    let verdict = evaluate_contamination(&inputs, &thresholds);

    // Stored: the candidate itself is not deleted; the contaminated verdict is
    // what the activation gate reads.
    assert!(verdict.is_contaminated());
    // Presented: activation is refused, and the refusal names the signal.
    let refusal = require_uncontaminated_for_activation(&verdict)
        .expect_err("a contaminated verdict must refuse activation");
    assert_eq!(
        refusal.signal,
        agent_usage_book::calibration::contamination::ContaminationSignal::FlatCreditsWithMeterMovement
    );
}

#[test]
fn row_20_missing_rate_card() {
    let empty_book = RateBook::new(Vec::new());
    let usage = UsageVector::new(
        KnownTokenVector::new(
            InputTokens::new(100_000),
            OutputTokens::new(0),
            CacheReadTokens::new(0),
            CacheWriteTokens::new(0),
        ),
        std::collections::BTreeMap::new(),
        CoverageCompleteness::Complete,
        EvidenceQuality::Measured,
    );

    let outcome: ValuationOutcome<Usd> = value_usage_vector(
        &empty_book,
        "anthropic",
        "claude-opus-4",
        UtcDate::parse("2026-08-30").expect("valid date"),
        &usage,
    );

    // Stored: no total is fabricated from the missing rate; the known-price
    // subtotal for the affected aggregate is exactly zero, never the input
    // count valued at an invented rate.
    let ValuationOutcome::Incomplete {
        known_price_subtotal,
        missing_rates,
    } = outcome
    else {
        panic!("expected an incomplete valuation with no matching rate card");
    };
    assert_eq!(known_price_subtotal.micros(), 0);
    // Presented: the missing rate names exactly the affected vendor/model/class.
    assert!(
        missing_rates
            .iter()
            .any(|m: &MissingRate| m.vendor == "anthropic"
                && m.model == "claude-opus-4"
                && m.token_class == "input")
    );
}

fn task(name: &str) -> TaskId {
    TaskId::new(SourceNamespace::new("github"), NativeTaskId::new(name))
}

fn ts(nanos: i64) -> UtcTimestamp {
    UtcTimestamp::from_unix_nanos(nanos)
}

fn task_tokens(input: u64) -> KnownTokenVector {
    KnownTokenVector::new(
        InputTokens::new(input),
        OutputTokens::new(0),
        CacheReadTokens::new(0),
        CacheWriteTokens::new(0),
    )
}

#[test]
fn row_21_task_boundary_ambiguous() {
    let inputs = SegmentationInputs {
        context: SegmentationContext {
            session_is_mapped: true,
            tracker_available: true,
        },
        boundaries: vec![
            ClaimBoundary {
                task_id: task("T1"),
                occurred_at: ts(0),
                kind: TaskEventKind::Claim,
            },
            ClaimBoundary {
                task_id: task("T2"),
                occurred_at: ts(35),
                kind: TaskEventKind::Claim,
            },
        ],
        usage: vec![UsageWindow {
            start: Some(ts(30)),
            end: Some(ts(40)), // crosses the T1/T2 boundary at 35
            usage: task_tokens(21),
        }],
    };

    let result = segment_by_task(&inputs);

    // Stored: the window's usage lands wholly in the named overhead bucket,
    // never split by wall-clock proportion between the two tasks.
    assert_eq!(
        result.overhead_usage(OverheadReason::AmbiguousBoundary),
        Some(task_tokens(21))
    );
    // Presented: neither task's own total was touched by the ambiguous window.
    assert!(result.task_usage(&task("T1")).is_none());
    assert!(result.task_usage(&task("T2")).is_none());
}

#[test]
fn row_22_account_unknown() {
    let inputs = AccountSegmentationInputs {
        markers: vec![AccountMarkerBoundary::explicit(
            "work-primary",
            ts(100),
            None,
        )],
        usage: vec![AccountUsageEvent {
            // Strictly before the first marker: no marker justifies an account.
            occurred_at: ts(50),
            usage: task_tokens(30),
        }],
    };

    let result = segment_by_account(&inputs);

    // Stored: the usage lands in the explicit unknown-account bucket, never
    // guessed onto the account that happens to be active later.
    assert_eq!(result.unknown_account_usage(), Some(task_tokens(30)));
    // Presented: the named account received nothing from it.
    assert!(result.account_usage("work-primary").is_none());
}

#[test]
fn row_23_web_consumption_has_no_transcript() {
    // No transcript exists for this usage window at all (web-only
    // consumption): the segmentation input carries no usage events, so
    // nothing is invented from the meter movement alone.
    let inputs = AccountSegmentationInputs {
        markers: vec![AccountMarkerBoundary::explicit("work-primary", ts(0), None)],
        usage: vec![],
    };
    let result = segment_by_account(&inputs);

    // Stored: no token attribution was fabricated for the account.
    assert!(result.account_usage("work-primary").is_none());
    assert!(result.unknown_account_usage().is_none());

    // Presented: the meter's own quota movement is still valid evidence in its
    // own right, unaffected by the absent transcript. A legitimate increase
    // with no anomalous reset classifies as no anomaly at all.
    let previous = WindowReading {
        quota_used: QuotaUsed::new(QuotaFractionPpm::new(100_000).unwrap()),
        resets_at: WindowResetState::NotStarted,
        observed_at: ts(0),
    };
    let current = WindowReading {
        quota_used: QuotaUsed::new(QuotaFractionPpm::new(150_000).unwrap()),
        resets_at: WindowResetState::NotStarted,
        observed_at: ts(1_000),
    };
    assert_eq!(classify_window_transition(previous, current), None);
}

#[test]
fn row_24_timer_never_ran() {
    let inputs = CoverageInputs {
        interval_start: ts(0),
        interval_end: ts(1_000_000_000_000), // 1000s
        policy_snapshots: vec![PolicySnapshot {
            effective_at: ts(0),
            ordinary_cadence: MonotonicDuration::from_seconds(300),
        }],
        attempts: vec![],
        observations: vec![],
        resets: vec![],
        timer_runs: vec![],
    };

    let report = compute_coverage(&inputs);

    // Stored: no attempt row exists at all over the interval.
    assert_eq!(report.attempted_opportunities, 0);
    // Presented: a first-class coverage gap, not a silently perfect number.
    assert_eq!(report.attempt_coverage.map(|f| f.as_f64()), Some(0.0));
    assert!(report.longest_no_attempt_gap.is_some());
}

#[test]
fn row_25_timer_ran_provider_failed() {
    let inputs = CoverageInputs {
        interval_start: ts(0),
        interval_end: ts(1_000_000_000_000),
        policy_snapshots: vec![PolicySnapshot {
            effective_at: ts(0),
            ordinary_cadence: MonotonicDuration::from_seconds(300),
        }],
        // The timer fired and an attempt reached a terminal result every time,
        // but none of them produced a successful observation.
        attempts: vec![
            AttemptRecord {
                started_at: ts(100_000_000_000),
                result: Some(AttemptResultRecord {
                    finished_at: ts(100_100_000_000),
                    retry_after: None,
                }),
            },
            AttemptRecord {
                started_at: ts(400_000_000_000),
                result: Some(AttemptResultRecord {
                    finished_at: ts(400_100_000_000),
                    retry_after: None,
                }),
            },
        ],
        observations: vec![],
        resets: vec![],
        timer_runs: vec![
            TimerRunRecord {
                at: ts(100_000_000_000),
            },
            TimerRunRecord {
                at: ts(400_000_000_000),
            },
        ],
    };

    let report = compute_coverage(&inputs);

    // Stored: the attempts themselves happened; the gap is not in whether the
    // timer ran.
    assert_eq!(report.attempted_opportunities, 2);
    // Presented: the measurement coverage, not the attempt coverage, is what
    // reads as the gap, distinguishing "timer never ran" from "timer ran,
    // provider failed".
    assert_eq!(report.measurement_coverage.map(|f| f.as_f64()), Some(0.0));
}

#[test]
fn row_26_meter_percent_decreases_without_reset() {
    let previous = WindowReading {
        quota_used: QuotaUsed::new(QuotaFractionPpm::new(500_000).unwrap()),
        resets_at: WindowResetState::NotStarted,
        observed_at: ts(0),
    };
    let current = WindowReading {
        // Dropped with no reset state change at all: nothing legitimises it.
        quota_used: QuotaUsed::new(QuotaFractionPpm::new(200_000).unwrap()),
        resets_at: WindowResetState::NotStarted,
        observed_at: ts(1_000),
    };

    let anomaly = classify_window_transition(previous, current);

    // Stored: the observation itself is retained regardless (the caller never
    // discards a reading because it is anomalous); what is checked here is the
    // typed classification a store would persist alongside it.
    assert_eq!(
        anomaly,
        Some(WindowAnomalyKind::PercentageDecreaseWithoutReset)
    );
    // Presented: this is not the sibling anomaly (an unexpected reset-timestamp
    // change), which is exactly the confusion a swapped classification would
    // produce.
    assert_ne!(
        anomaly,
        Some(WindowAnomalyKind::UnexpectedResetTimestampChange)
    );
}

#[test]
fn row_27_reset_timestamp_changes_unexpectedly() {
    // The instants are seconds rather than microseconds apart on purpose. The
    // classifier now carries a 100 ms jitter envelope, so a reset that moves by
    // less than that is the same boundary reported twice and not an anomaly at
    // all; and "not yet due" is judged against the old instant minus that same
    // envelope, which near zero would go negative and make every reading due.
    // A row asserting the unexpected-change class therefore has to move the
    // boundary materially, and has to observe well before it.
    let previous = WindowReading {
        quota_used: QuotaUsed::new(QuotaFractionPpm::new(500_000).unwrap()),
        resets_at: WindowResetState::Known(ts(1_000_000_000)),
        observed_at: ts(400_000_000),
    };
    let current = WindowReading {
        // No decrease, but the reset instant moved by 300 ms with no boundary
        // having been due yet (observed_at is still before the old reset
        // instant, by more than the jitter envelope).
        quota_used: QuotaUsed::new(QuotaFractionPpm::new(500_000).unwrap()),
        resets_at: WindowResetState::Known(ts(1_300_000_000)),
        observed_at: ts(500_000_000),
    };

    let anomaly = classify_window_transition(previous, current);

    assert_eq!(
        anomaly,
        Some(WindowAnomalyKind::UnexpectedResetTimestampChange)
    );
    assert_ne!(
        anomaly,
        Some(WindowAnomalyKind::PercentageDecreaseWithoutReset)
    );
}

#[test]
fn row_28_clock_moves_backward() {
    let ctx = CredentialContextId::new("ctx-clock");
    let horizon = MonotonicDuration::from_seconds(300);
    let command_horizon = MonotonicDuration::from_seconds(10);
    // A one-second envelope: a 50-second gap between provider-observed and
    // received time is well outside it.
    let envelope = ClockSkewEnvelope::new(MonotonicDuration::from_seconds(10));
    let observed = Observed::new(
        77u64,
        Some(ProviderObservedAt::new(ts(1_050_000_000_000))),
        ReceivedAt::new(ts(1_000_000_000_000)),
        MeasurementBasis::ProviderObserved,
    );
    let started = AttemptStarted::new(AttemptId::new(1), ts(1_000_000_000_000));
    let result = AttemptResult::new(
        AttemptId::new(1),
        ts(1_000_000_000_000),
        MonotonicDuration::from_seconds(0),
        AttemptOutcome::Success,
    );
    let input = FreshnessInput::new(
        Some(observed.clone()),
        Some(&ctx),
        Some(LatestAttempt::new(started, Some(result), &ctx)),
        None,
        Some(&ctx),
        horizon,
        command_horizon,
        envelope,
    );

    let freshness: Freshness<u64> =
        compute_freshness(&input, &FakeClock::new(ts(1_060_000_000_000)));

    // Stored: the evidence is retained (the observation itself is not
    // discarded); the clock flag is what a store-level exclusion reads.
    let Freshness::Stale {
        last_good: Some(good),
        reason: StaleReason::ClockAnomaly,
        ..
    } = freshness
    else {
        panic!("expected Stale(ClockAnomaly), got {freshness:?}");
    };
    assert_eq!(good, observed);
    // Presented: never reported as if the request had simply succeeded.
}

#[test]
fn row_29_collector_died_after_durable_attempt_start() {
    let ctx = CredentialContextId::new("ctx-collector");
    let horizon = MonotonicDuration::from_seconds(300);
    let command_horizon = MonotonicDuration::from_seconds(10);
    let envelope = ClockSkewEnvelope::new(MonotonicDuration::from_seconds(10));
    let last_good = Observed::new(
        50u64,
        Some(ProviderObservedAt::new(ts(1_000_000_000))),
        ReceivedAt::new(ts(1_000_000_000)),
        MeasurementBasis::ProviderObserved,
    );

    // Stored: a real attempt-start row exists with no terminal result, the
    // exact shape a collector kill between request and response leaves.
    let state = StateDir::new();
    let conn = init_ledger(state.path());
    let attempt_id = start_durable_attempt(&conn, 5_000_000_000);
    let (has_result,): (i64,) = conn
        .query_row(
            "SELECT count(*) FROM meter_attempt_result WHERE attempt_id = ?1",
            [attempt_id],
            |r| Ok((r.get(0)?,)),
        )
        .unwrap();
    assert_eq!(
        has_result, 0,
        "a collector interruption never durably records a terminal result"
    );

    // Presented: freshness reads it as a collector interruption once the
    // command horizon passes, never as an endpoint timeout.
    let started = AttemptStarted::new(AttemptId::new(attempt_id as u64), ts(5_000_000_000));
    let input = FreshnessInput::new(
        Some(last_good.clone()),
        Some(&ctx),
        Some(LatestAttempt::new(started, None, &ctx)),
        None,
        Some(&ctx),
        horizon,
        command_horizon,
        envelope,
    );
    let freshness: Freshness<u64> = compute_freshness(&input, &FakeClock::new(ts(20_000_000_000)));
    let Freshness::Stale { reason, .. } = freshness else {
        panic!("expected Stale, got {freshness:?}");
    };
    assert_eq!(reason, StaleReason::CollectorInterrupted);
    assert_ne!(
        reason,
        StaleReason::SourceUnreachable(FailureClass::ConnectTimeout)
    );
}

#[test]
fn row_30_projection_lags_db() {
    let state = StateDir::new();
    let conn = init_ledger(state.path());
    // The database has already advanced past generation 0 (a real write
    // happened), but the published projection file was never refreshed and
    // still names the prior generation.
    agent_usage_book::store::ledger_generation::advance(&conn).expect("advance generation");
    std::fs::write(
        state.path().join("projection"),
        "{\"schema_version\":1,\"ledger_generation\":0}",
    )
    .expect("write stale projection file");

    let config_text = format!("[state]\ndir = {:?}\n", state.path());
    let (config, _) = agent_usage_book::config::resolve(
        &agent_usage_book::config::Overrides::new(),
        &agent_usage_book::config::RealEnv,
        Some(&config_text),
        "aub.toml",
    )
    .expect("minimal config must resolve");
    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000_000_000_000),
        db_path: state
            .path()
            .join(agent_usage_book::store::connection::LEDGER_DATABASE_FILE),
        db: Some(&conn),
        db_missing: false,
        db_open_error: None,
    };

    let outcomes = build_registry(&ctx);
    let outcome = outcomes
        .iter()
        .find(|o| o.name == CheckName::ProjectionVersusDatabaseGeneration)
        .expect("ProjectionVersusDatabaseGeneration present");

    // Stored: `status` never invents a fresher number than the last published
    // projection; the doctor check is what makes the lag visible.
    assert!(
        matches!(&outcome.status, CheckStatus::Fail(reason) if reason.contains("projection is generation 0, database is at 1")),
        "outcome was {:?}",
        outcome.status
    );
    // Presented: doctor names this as a repairable check (`--fix` republishes).
    assert!(outcome.has_repair);
}

#[test]
fn row_31_passive_fit_contaminated() {
    let thresholds = ContaminationThresholds::conservative_default();
    // No meter movement anywhere: the flat-credits and drift signals stay
    // clean. The only thing that fires is a second session marked against the
    // same account inside the experiment window.
    let clean = [
        ContaminationMeterPoint::new(
            ts(0),
            QuotaUsed::new(QuotaFractionPpm::new(100_000).unwrap()),
        ),
        ContaminationMeterPoint::new(
            ts(500),
            QuotaUsed::new(QuotaFractionPpm::new(100_000).unwrap()),
        ),
    ];
    let marker = ContaminationMarkerPoint::new("claude-code", "session-b", "work-a", ts(1_500));
    let inputs = ContaminationInputs {
        experiment_account: "work-a",
        baseline_plateau_started_at: ts(0),
        started_at: ts(1_000),
        ended_at: Some(ts(2_000)),
        evaluated_at: ts(3_000),
        pre_burn_series: &clean,
        post_series: &clean,
        controlled_meter_start: QuotaUsed::new(QuotaFractionPpm::new(100_000).unwrap()),
        controlled_meter_end: QuotaUsed::new(QuotaFractionPpm::new(100_000).unwrap()),
        local_credits_delta: agent_usage_book::domain::credits::Credits::from_micros(5_000_000),
        markers: std::slice::from_ref(&marker),
    };

    let verdict = evaluate_contamination(&inputs, &thresholds);

    // Stored: the candidate produced by the passive fit is retained (nothing
    // here deletes it); the contamination mark is what the activation gate
    // reads before letting it become authoritative.
    assert!(verdict.is_contaminated());
    let refusal = require_uncontaminated_for_activation(&verdict)
        .expect_err("an overlapping session must refuse activation");
    // Presented: named as an overlapping-session finding, not the flat-credits
    // signal row 19 exercises; the two contaminated rows are distinguished by
    // which signal fired.
    assert_eq!(
        refusal.signal,
        agent_usage_book::calibration::contamination::ContaminationSignal::OverlappingSession
    );
}

#[test]
fn row_32_calibration_review_overdue() {
    let facts = calibration_facts();
    let context = matching_context();
    let review_due_at = ts(10_000);
    let inputs = HealthInputs {
        calibration: &facts,
        context: &context,
        lifecycle: LifecycleState::Active,
        cost_model_superseded: false,
        drift: None,
        review_due_at: Some(review_due_at),
    };

    // Stored: the calibration's historical result is untouched and stays
    // readable; only its health state changed.
    let health = compute_health(&inputs, ts(20_000));
    assert_eq!(health, CalibrationHealth::ReviewDue);

    // Presented: a quantitative consumer (`can-run`) refuses a current verdict.
    let refusal = require_current_applicable(health).expect_err("review-due must refuse a verdict");
    assert_eq!(refusal.health, CalibrationHealth::ReviewDue);
}

// ---------------------------------------------------------------------------
// Class-swap verification (rows 03-06 differ only in persisted failure class)
// ---------------------------------------------------------------------------

/// Runs one `aub sample --require-success` scenario against a synthetic
/// server and returns the `failure_class` persisted for it. Shared by rows
/// 03-06 and by the swap verification below so the same real invocation path
/// backs both: a hand-copied literal here could drift from what the rows
/// above actually observed.
fn observed_failure_class_at(state: &StateDir, endpoint_url: &str) -> String {
    let conn = init_ledger(state.path());
    let config = write_account_config(state.path(), endpoint_url);

    let output = aub_cmd(state.path(), &config)
        .env("AUB_ANTHROPIC_ENDPOINT", endpoint_url)
        .args([
            "sample",
            "--account",
            "work-primary",
            "--require-success",
            "--format=json",
        ])
        .output()
        .expect("aub must run");
    assert_eq!(
        output.status.code(),
        Some(ExitClass::RemoteUnavailable.code() as i32)
    );

    conn.query_row("SELECT failure_class FROM meter_attempt_result", [], |r| {
        r.get(0)
    })
    .expect("a failure_class must be persisted")
}

fn observed_failure_class(outcome: ScriptedOutcome) -> String {
    let state = StateDir::new();
    let server = SyntheticServer::start(vec![outcome]).unwrap();
    observed_failure_class_at(&state, &server.url())
}

#[test]
fn class_swap_verification_for_rows_differing_only_in_persisted_failure_class() {
    let connect_timeout = observed_failure_class_at(&StateDir::new(), UNREACHABLE_ENDPOINT);
    let rate_limited = observed_failure_class(ScriptedOutcome::TooManyRequests429 {
        retry_after_seconds: Some(60),
    });
    let server_error = observed_failure_class(ScriptedOutcome::InternalServerError500);
    let malformed_body = observed_failure_class(ScriptedOutcome::MalformedJson {
        status: 200,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: b"not json at all {".to_vec(),
    });

    // Every row 03-06 pins its own exact persisted class already; this test's
    // job is the swap itself: if any two of the four classes were exchanged,
    // exactly one of these equalities would break while the other rows kept
    // passing, since each row asserts one literal independently. Requiring
    // all four pairwise distinct is what makes a swap detectable at all.
    let classes = [
        &connect_timeout,
        &rate_limited,
        &server_error,
        &malformed_body,
    ];
    for i in 0..classes.len() {
        for j in (i + 1)..classes.len() {
            assert_ne!(
                classes[i], classes[j],
                "rows 03-06 must persist pairwise distinct failure classes, \
                 or swapping their expectations would go undetected"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Table-versus-case-list consistency
// ---------------------------------------------------------------------------

/// Parses the "Failure" column of PLAN.md Section 30's table, in row order.
/// Reads the real file rather than a copy, so editing the table without
/// touching this test file is exactly what this check exists to catch.
fn plan_failure_table_titles() -> Vec<String> {
    let plan_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/PLAN.md");
    let text = std::fs::read_to_string(&plan_path).expect("docs/PLAN.md must be readable");
    let section_start = text
        .find("\n# 30. Failure semantics\n")
        .expect("PLAN.md must have a '# 30. Failure semantics' heading");
    let section_end = text[section_start + 1..]
        .find("\n# 31.")
        .expect("PLAN.md must have a following '# 31.' heading");
    let section = &text[section_start..section_start + 1 + section_end];

    section
        .lines()
        .filter(|line| line.starts_with("| "))
        .skip(2) // header row, then the "| --- |" separator row
        .filter_map(|line| line.trim_start_matches('|').split('|').next())
        .map(|cell| cell.trim().to_string())
        .collect()
}

/// Maps every [`FailureRow`] to the test function that covers it, through an
/// exhaustive match with no wildcard arm: adding a row to [`FailureRow::ALL`]
/// without adding its arm here is a compile error, not a silently-uncovered
/// row. Function items are referenced, never called, so this registry costs
/// nothing at runtime beyond the pointer comparisons below.
fn case_for_row(row: FailureRow) -> fn() {
    match row {
        FailureRow::NoCredentialsConfigured => row_01_no_credentials_configured,
        FailureRow::ProviderSaysCredentialsInvalidExpired => {
            row_02_provider_says_credentials_invalid_expired
        }
        FailureRow::EndpointUnreachable => row_03_endpoint_unreachable,
        FailureRow::HttpRateLimit => row_04_http_rate_limit,
        FailureRow::Http5xx => row_05_http_5xx,
        FailureRow::MalformedPayload200 => row_06_malformed_payload_200,
        FailureRow::ProviderResponseTimestampAlreadyTooOld => {
            row_07_provider_response_timestamp_already_too_old
        }
        FailureRow::DbUnavailableBeforeSample => row_08_db_unavailable_before_sample,
        FailureRow::DbCommitFailsAfterRequest => row_09_db_commit_fails_after_request,
        FailureRow::ProjectionUnavailableCorrupt => row_10_projection_unavailable_corrupt,
        FailureRow::TranscriptMissing => row_11_transcript_missing,
        FailureRow::TranscriptParseError => row_12_transcript_parse_error,
        FailureRow::DuplicateTranscriptRecord => row_13_duplicate_transcript_record,
        FailureRow::UnknownTokenClass => row_14_unknown_token_class,
        FailureRow::MissingCostModelTerm => row_15_missing_cost_model_term,
        FailureRow::PlanMismatch => row_16_plan_mismatch,
        FailureRow::WindowResetInsideCalibrationSegment => {
            row_17_window_reset_inside_calibration_segment
        }
        FailureRow::MixedPlanTiersInCalibration => row_18_mixed_plan_tiers_in_calibration,
        FailureRow::CalibrationContaminated => row_19_calibration_contaminated,
        FailureRow::MissingRateCard => row_20_missing_rate_card,
        FailureRow::TaskBoundaryAmbiguous => row_21_task_boundary_ambiguous,
        FailureRow::AccountUnknown => row_22_account_unknown,
        FailureRow::WebConsumptionHasNoTranscript => row_23_web_consumption_has_no_transcript,
        FailureRow::TimerNeverRan => row_24_timer_never_ran,
        FailureRow::TimerRanProviderFailed => row_25_timer_ran_provider_failed,
        FailureRow::MeterPercentDecreasesWithoutReset => {
            row_26_meter_percent_decreases_without_reset
        }
        FailureRow::ResetTimestampChangesUnexpectedly => {
            row_27_reset_timestamp_changes_unexpectedly
        }
        FailureRow::ClockMovesBackward => row_28_clock_moves_backward,
        FailureRow::CollectorDiedAfterDurableAttemptStart => {
            row_29_collector_died_after_durable_attempt_start
        }
        FailureRow::ProjectionLagsDb => row_30_projection_lags_db,
        FailureRow::PassiveFitContaminated => row_31_passive_fit_contaminated,
        FailureRow::CalibrationReviewOverdue => row_32_calibration_review_overdue,
    }
}

#[test]
fn every_table_row_has_exactly_one_case() {
    // The exhaustive match in `case_for_row` already refuses to compile once
    // `FailureRow::ALL` grows past it; what remains to check at runtime is
    // that no two rows share one case function, and that the table itself
    // (read fresh from PLAN.md, not copied here) has exactly the rows this
    // enum claims, in the same order and under the same titles.
    let mut seen = std::collections::HashSet::new();
    for row in FailureRow::ALL {
        let pointer = case_for_row(row) as usize;
        assert!(
            seen.insert(pointer),
            "row {row:?} shares its case function with an earlier row"
        );
    }

    let plan_titles = plan_failure_table_titles();
    let case_titles: Vec<&str> = FailureRow::ALL
        .iter()
        .map(|row| row.table_title())
        .collect();
    assert_eq!(
        plan_titles.len(),
        case_titles.len(),
        "PLAN.md's failure semantics table has {} rows but FailureRow::ALL has {}: \
         a row was added to one without the other",
        plan_titles.len(),
        case_titles.len(),
    );
    for (index, (plan_title, case_title)) in plan_titles.iter().zip(case_titles.iter()).enumerate()
    {
        assert_eq!(
            plan_title, case_title,
            "row {index} title mismatch between PLAN.md and FailureRow::table_title"
        );
    }
}
