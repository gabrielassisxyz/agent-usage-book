//! The status JSON contract tests (aub-me5.6): every document validates
//! against the versioned envelope, exactly one freshness variant is present
//! per account, and the degraded and selector documents carry their facts.

use agent_usage_book::domain::attempt::AttemptId;
use agent_usage_book::domain::freshness::{Freshness, Observed};
use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaRemaining};
use agent_usage_book::domain::time::{MeasurementBasis, ReceivedAt, UtcTimestamp};
use agent_usage_book::domain::window::{ModelId, NominalWindowDuration, WindowScope};
use agent_usage_book::logging::{LogicalName, RunId};
use agent_usage_book::presentation::json::{status_json_with_explain, validate_status_report_json};
use agent_usage_book::presentation::render::ExplainMode;
use agent_usage_book::report::{
    LimitingWindow, MeterAccount, ProjectionReadState, ReportMetadata, StatusReport,
};

fn run() -> RunId {
    RunId::new(UtcTimestamp::from_unix_nanos(2_000))
}

fn metadata() -> ReportMetadata {
    let now = UtcTimestamp::from_unix_nanos(2_000);
    ReportMetadata::new(
        now,
        now,
        agent_usage_book::report::LedgerGeneration::new(12),
        None,
    )
}

fn observed(remaining_ppm: u32) -> Observed<QuotaRemaining> {
    Observed::new(
        QuotaRemaining::new(QuotaFractionPpm::new(remaining_ppm as i32).unwrap()),
        None,
        ReceivedAt::new(UtcTimestamp::from_unix_nanos(1_000)),
        MeasurementBasis::ProviderObserved,
    )
}

fn fresh_account(name: &str) -> MeterAccount {
    MeterAccount::from_projection(
        LogicalName::new(name),
        Freshness::Fresh {
            observed: observed(380_000),
            latest_attempt: AttemptId::new(1),
        },
        Some(LimitingWindow {
            scope: WindowScope::AccountWide,
            nominal_duration: NominalWindowDuration::from_nanos(18_000_000_000_000),
        }),
        vec![WindowScope::AccountWide],
        None,
    )
}

/// Every freshness variant serializes with exactly one variant marker, and the
/// document validates against the versioned contract.
#[test]
fn each_freshness_variant_appears_exactly_once_and_validates() {
    let variants: Vec<(MeterAccount, &str, Vec<&str>)> = vec![
        (
            fresh_account("primary"),
            "fresh",
            vec!["remaining", "latest_attempt"],
        ),
        (
            MeterAccount::new(
                LogicalName::new("secondary"),
                Freshness::Stale {
                    last_good: Some(observed(250_000)),
                    latest_attempt: AttemptId::new(2),
                    reason: agent_usage_book::domain::freshness::StaleReason::AgeExceeded,
                },
            ),
            "stale",
            vec!["reason", "last_good", "latest_attempt"],
        ),
        (
            MeterAccount::new(
                LogicalName::new("tertiary"),
                Freshness::<QuotaRemaining>::AuthRequired {
                    last_good: None,
                    latest_attempt: AttemptId::new(3),
                },
            ),
            "auth_required",
            vec!["last_good", "latest_attempt"],
        ),
    ];

    for (account, variant, required_fields) in variants {
        let report =
            StatusReport::new(metadata(), vec![account], vec![], ProjectionReadState::Read);
        let document = status_json_with_explain(&report, run(), ExplainMode::Off);
        validate_status_report_json(&document).expect("the contract document must validate");

        let parsed: serde_json::Value = serde_json::from_str(&document).unwrap();
        let account = &parsed["accounts"][0];
        let names: Vec<String> = account.as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            account["freshness"].as_str().unwrap(),
            variant,
            "exactly one freshness variant names itself: {names:?}"
        );
        assert!(
            !names.iter().any(|name| {
                name != "freshness" && ["fresh", "stale", "auth_required"].contains(&name.as_str())
            }),
            "no second freshness vocabulary may appear: {names:?}"
        );
        for field in required_fields {
            assert!(
                account.get(field).is_some(),
                "variant {variant} carries {field}: {names:?}"
            );
        }
    }
}

/// A projection the status path could not read is stated once, with the state
/// and the reason, and the account list is empty.
#[test]
fn the_unavailable_projection_is_stated_once_with_its_reason() {
    for (state, reason) in [
        ("missing", "projection not found"),
        ("unsupported_schema", "projection schema version 9 is newer"),
        ("malformed", "projection malformed: not valid JSON"),
        ("too_large", "projection exceeds the read bound"),
    ] {
        let report = StatusReport::new(
            metadata(),
            vec![],
            vec![],
            ProjectionReadState::Unavailable {
                state,
                reason: reason.to_string(),
            },
        );
        let document = status_json_with_explain(&report, run(), ExplainMode::Off);
        validate_status_report_json(&document)
            .expect("the degraded document must validate against the contract");

        let parsed: serde_json::Value = serde_json::from_str(&document).unwrap();
        let projection = &parsed["projection"];
        assert_eq!(projection["state"], state);
        assert_eq!(projection["reason"], reason);
        assert_eq!(parsed["accounts"].as_array().map(Vec::len), Some(0));
    }
}

/// The selector context: the chosen model and every included window scope are
/// identified, including the limiting window itself.
#[test]
fn the_selector_document_identifies_model_scopes_and_the_limit() {
    let report = StatusReport::new(
        metadata(),
        vec![MeterAccount::from_projection(
            LogicalName::new("work-primary"),
            Freshness::Fresh {
                observed: observed(700_000),
                latest_attempt: AttemptId::new(1),
            },
            Some(LimitingWindow {
                scope: WindowScope::ModelSpecific(ModelId::new("claude-model-x".to_string())),
                nominal_duration: NominalWindowDuration::from_nanos(7 * 86_400_000_000_000),
            }),
            vec![
                WindowScope::AccountWide,
                WindowScope::ModelSpecific(ModelId::new("claude-model-x".to_string())),
            ],
            Some(ModelId::new("claude-model-x".to_string())),
        )],
        vec![],
        ProjectionReadState::Read,
    );
    let document = status_json_with_explain(&report, run(), ExplainMode::Off);
    validate_status_report_json(&document).expect("the selector document must validate");

    let parsed: serde_json::Value = serde_json::from_str(&document).unwrap();
    let account = &parsed["accounts"][0];
    assert_eq!(account["selected_model"], "claude-model-x");
    let scopes: Vec<&str> = account["included_scopes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|scope| scope.as_str().unwrap())
        .collect();
    assert_eq!(scopes, vec!["account_wide", "model:claude-model-x"]);
    assert_eq!(account["limiting_window"]["scope"], "model");
    assert_eq!(account["limiting_window"]["model"], "claude-model-x");
    assert_eq!(
        account["limiting_window"]["nominal_duration_nanos"],
        7 * 86_400_000_000_000i64
    );
}

/// A reading with no window context carries the fact by the fields' absence:
/// the account object is exactly the pre-selector shape.
#[test]
fn a_reading_without_window_context_stays_the_plain_shape() {
    let report = StatusReport::new(
        metadata(),
        vec![MeterAccount::new(
            LogicalName::new("primary"),
            Freshness::Fresh {
                observed: observed(380_000),
                latest_attempt: AttemptId::new(1),
            },
        )],
        vec![],
        ProjectionReadState::Read,
    );
    let document = status_json_with_explain(&report, run(), ExplainMode::Off);
    validate_status_report_json(&document).expect("the plain document must validate");

    let parsed: serde_json::Value = serde_json::from_str(&document).unwrap();
    let account = &parsed["accounts"][0];
    let names: Vec<String> = account.as_object().unwrap().keys().cloned().collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(
        names, sorted,
        "the fields are exactly the plain shape, in the serializer's order: {names:?}"
    );
    assert_eq!(
        sorted,
        vec!["account", "freshness", "latest_attempt", "remaining"],
        "no selector context, no selector fields"
    );
}

/// The validator refuses a projection object outside the four unavailable
/// states, so a consumer can match the state exhaustively.
#[test]
fn the_validator_refuses_an_unknown_projection_state() {
    let report = StatusReport::new(
        metadata(),
        vec![],
        vec![],
        ProjectionReadState::Unavailable {
            state: "missing",
            reason: "projection not found".to_string(),
        },
    );
    let document = status_json_with_explain(&report, run(), ExplainMode::Off)
        .replace("\"state\":\"missing\"", "\"state\":\"all_fine\"");

    let error = validate_status_report_json(&document).expect_err("an unknown state must refuse");
    assert!(
        error.to_string().contains("projection.state"),
        "the refusal names the field: {error}"
    );
}
