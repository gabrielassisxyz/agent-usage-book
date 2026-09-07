//! The status JSON contract tests (aub-me5.6): every document validates
//! against the versioned envelope, exactly one freshness variant is present
//! per account, and the degraded and selector documents carry their facts.

use agent_usage_book::domain::attempt::AttemptId;
use agent_usage_book::domain::burn_rate::BurnRate;
use agent_usage_book::domain::freshness::{Freshness, Observed};
use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaRemaining};
use agent_usage_book::domain::time::{MeasurementBasis, ReceivedAt, UtcTimestamp};
use agent_usage_book::domain::window::{
    ModelId, NominalWindowDuration, WindowResetState, WindowScope,
};
use agent_usage_book::logging::{LogicalName, RunId};
use agent_usage_book::presentation::json::{status_json_with_explain, validate_status_report_json};
use agent_usage_book::presentation::render::ExplainMode;
use agent_usage_book::report::{
    LimitingWindow, MeterAccount, ProjectionReadState, ReportMetadata, StatusReport, WindowBurnRate,
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
            reset_state: WindowResetState::Known(UtcTimestamp::from_unix_nanos(2_000)),
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
                reset_state: WindowResetState::Known(UtcTimestamp::from_unix_nanos(2_000)),
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

/// The limiting window carries the derived burn rate as a decimal string. The
/// status path has no observation series to find a freeze instant in, so
/// `capped_at` is null and the rate is the live one.
#[test]
fn the_limiting_window_carries_the_burn_rate_and_a_null_cap() {
    let account = MeterAccount::from_projection(
        LogicalName::new("primary"),
        Freshness::Fresh {
            observed: observed(600_000),
            latest_attempt: AttemptId::new(1),
        },
        Some(LimitingWindow {
            scope: WindowScope::AccountWide,
            nominal_duration: NominalWindowDuration::from_nanos(18_000_000_000_000),
            reset_state: WindowResetState::Known(UtcTimestamp::from_unix_nanos(2_000)),
        }),
        vec![WindowScope::AccountWide],
        None,
    )
    .with_burn_rate(WindowBurnRate {
        rate: BurnRate::from_window(400_000, Some(0.2)),
        capped_at: None,
        derived_from: UtcTimestamp::from_unix_nanos(1_000),
    });
    let report = StatusReport::new(metadata(), vec![account], vec![], ProjectionReadState::Read);
    let document = status_json_with_explain(&report, run(), ExplainMode::Off);
    validate_status_report_json(&document).expect("the document must validate");

    let parsed: serde_json::Value = serde_json::from_str(&document).unwrap();
    let limiting = &parsed["accounts"][0]["limiting_window"];
    assert_eq!(limiting["burn_rate"], "2.000000");
    assert!(
        limiting["capped_at"].is_null(),
        "the status path reports no freeze instant"
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

/// `aub status --format json` lists every window under `accounts[].windows[]`
/// (schema v3), each carrying the full field set, and the limiting window
/// derived from the list is the active window with the highest used ppm. The
/// planted negative: dropping one window field fails the contract, and a
/// limiting-window pick that took the first window rather than the most used
/// would disagree with `windows[]`.
#[test]
fn the_status_document_lists_every_window_with_its_full_field_set() {
    use agent_usage_book::domain::burn_rate::BurnRate;
    use agent_usage_book::domain::quota::QuotaUsed;
    use agent_usage_book::report::StatusWindow;

    let fresh = |ppm: u32| Freshness::Fresh {
        observed: observed(1_000_000 - ppm),
        latest_attempt: AttemptId::new(1),
    };
    let window =
        |key: &str, scope: WindowScope, used_ppm: i32, reset: WindowResetState| StatusWindow {
            semantic_key: key.to_string(),
            scope,
            quota_used: QuotaUsed::new(QuotaFractionPpm::new(used_ppm).unwrap()),
            reset_state: reset,
            nominal_duration: NominalWindowDuration::from_nanos(18_000_000_000_000),
            rate: BurnRate::from_window(used_ppm.max(0) as u32, Some(0.5)),
            capped_at: None,
            observation: fresh(500_000),
        };
    let account = MeterAccount::from_projection(
        LogicalName::new("primary"),
        fresh(380_000),
        None,
        vec![],
        None,
    )
    .with_windows(vec![
        window(
            "five_hour",
            WindowScope::AccountWide,
            300_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(9_000)),
        ),
        window(
            "weekly_scoped_fable",
            WindowScope::ModelSpecific(ModelId::new("fable".to_string())),
            910_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(9_000)),
        ),
        StatusWindow {
            rate: None,
            ..window(
                "weekly_all",
                WindowScope::AccountWide,
                0,
                WindowResetState::NotStarted,
            )
        },
    ]);

    // The limiting window is derived from the list, not stored: the most used
    // started window.
    assert_eq!(
        account
            .limiting_status_window()
            .map(|w| w.semantic_key.as_str()),
        Some("weekly_scoped_fable"),
    );

    let report = StatusReport::new(metadata(), vec![account], vec![], ProjectionReadState::Read);
    let document = status_json_with_explain(&report, run(), ExplainMode::Off);
    validate_status_report_json(&document).expect("the v3 windows document must validate");

    let parsed: serde_json::Value = serde_json::from_str(&document).unwrap();
    assert_eq!(parsed["schema"], 3);
    let windows = parsed["accounts"][0]["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 3);
    let fable = windows
        .iter()
        .find(|w| w["semantic_key"] == "weekly_scoped_fable")
        .unwrap();
    assert_eq!(fable["scope"], "model");
    assert_eq!(fable["model"], "fable");
    assert_eq!(fable["quota_used_ppm"], 910_000);
    assert_eq!(fable["resets_at_nanos"], 9_000);
    assert_eq!(fable["nominal_duration_nanos"], 18_000_000_000_000i64);
    assert!(fable["burn_rate"].is_string());
    assert!(fable["capped_at"].is_null());
    assert_eq!(fable["observation_freshness"], "fresh");

    let idle = windows
        .iter()
        .find(|w| w["semantic_key"] == "weekly_all")
        .unwrap();
    assert!(
        idle["resets_at_nanos"].is_null(),
        "a not-started window has no reset instant"
    );
    assert!(idle["burn_rate"].is_null());

    // The planted negative: a window missing a field is refused.
    let broken = document.replacen(",\"observation_freshness\":\"fresh\"", "", 1);
    assert!(
        validate_status_report_json(&broken).is_err(),
        "a window missing observation_freshness must be refused"
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

/// A provider quota group's window serializes its scope as an object
/// (`aub-n8yx`): `{"kind":"model_group","group":"<display name>"}`, the two
/// facts a consumer needs to read the sub-block the renderer prints beneath
/// the account. The limiting window, when it is a group window, carries the
/// same object, and `included_scopes` names the group with the flat
/// `group:<name>` label the model scopes' `model:<name>` parallels. The
/// planted negative: a naive reuse of the model scope would put the group
/// name in `model`, which a consumer reads as one model's own budget.
#[test]
fn a_model_group_window_serializes_its_scope_object() {
    use agent_usage_book::domain::burn_rate::BurnRate;
    use agent_usage_book::domain::quota::QuotaUsed;
    use agent_usage_book::domain::window::GroupName;
    use agent_usage_book::report::StatusWindow;

    let fresh = Freshness::Fresh {
        observed: observed(380_000),
        latest_attempt: AttemptId::new(1),
    };
    let group_window = StatusWindow {
        semantic_key: "5h".to_string(),
        scope: WindowScope::ModelGroup(GroupName::new("Gemini Models".to_string())),
        quota_used: QuotaUsed::new(QuotaFractionPpm::new(85_528).unwrap()),
        reset_state: WindowResetState::Known(UtcTimestamp::from_unix_nanos(9_000)),
        nominal_duration: NominalWindowDuration::from_nanos(18_000_000_000_000),
        rate: BurnRate::from_window(85_528, Some(0.5)),
        capped_at: None,
        observation: fresh.clone(),
    };
    let account = MeterAccount::from_projection(
        LogicalName::new("agy"),
        fresh,
        Some(LimitingWindow {
            scope: WindowScope::ModelGroup(GroupName::new("Gemini Models".to_string())),
            nominal_duration: NominalWindowDuration::from_nanos(18_000_000_000_000),
            reset_state: WindowResetState::Known(UtcTimestamp::from_unix_nanos(9_000)),
        }),
        vec![
            WindowScope::AccountWide,
            WindowScope::ModelGroup(GroupName::new("Gemini Models".to_string())),
        ],
        None,
    )
    .with_windows(vec![group_window]);

    let report = StatusReport::new(metadata(), vec![account], vec![], ProjectionReadState::Read);
    let document = status_json_with_explain(&report, run(), ExplainMode::Off);
    validate_status_report_json(&document).expect("the group-scope document must validate");

    let parsed: serde_json::Value = serde_json::from_str(&document).unwrap();
    let window = &parsed["accounts"][0]["windows"][0];
    assert_eq!(window["scope"]["kind"], "model_group");
    assert_eq!(window["scope"]["group"], "Gemini Models");
    assert_eq!(window["quota_used_ppm"], 85_528);

    let limiting = &parsed["accounts"][0]["limiting_window"];
    assert_eq!(limiting["scope"]["kind"], "model_group");
    assert_eq!(limiting["scope"]["group"], "Gemini Models");

    let scopes: Vec<&str> = parsed["accounts"][0]["included_scopes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|scope| scope.as_str().unwrap())
        .collect();
    assert_eq!(scopes, vec!["account_wide", "group:Gemini Models"]);
}
