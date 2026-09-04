//! Integration and unit tests for interval reconciliation with strict eligibility gating (aub-dpn.1).
//!
//! Tests the 6 strict eligibility conditions in isolation, property test for zero residual,
//! signed residual distinguishability (overprediction vs underexplanation), unexplained residual
//! naming across human and JSON surfaces, full provenance manifest tracking, diagnostic patterns,
//! and single-source calibration proof via the shared calibration repository.
//!
//! aub-dpn.2 extends this with residual-uncertainty propagation: quantization derived
//! per observation, calibration uncertainty carried through interval arithmetic, explicit
//! timing alignment, the residual as an interval in both renderers, a zero-containing
//! interval reported as reconciling, and the non-narrowing law as a property test.

use std::collections::BTreeSet;

use agent_usage_book::attribution::account_segment::AccountEvidenceClass;
use agent_usage_book::calibration::health::CalibrationHealth;
use agent_usage_book::domain::credits::Credits;
use agent_usage_book::domain::ids::{AdapterVersion, MeterSemanticsId, ProviderContractId};
use agent_usage_book::domain::provenance::{
    CostModelId, EvidenceId, WindowCalibrationId, WitnessId,
};
use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaUsed};
use agent_usage_book::domain::time::{
    FakeClock, MeasurementBasis, MonotonicDuration, UtcTimestamp,
};
use agent_usage_book::domain::window::{
    NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
    WindowSemanticKey,
};
use agent_usage_book::presentation::{reconciliation_json, render_reconciliation};
use agent_usage_book::reconciliation::{
    CandidateInterval, CandidateObservation, CandidateUsageEvent, EligibilityCondition,
    IntervalCoverage, IntervalSettlement, IntervalUsage, ReconciliationOutcome, ResidualPattern,
    TimingAlignmentUncertainty, classify_patterns, evaluate_eligibility, reconcile,
};
use agent_usage_book::store::account::{AccountId, observe_account};
use agent_usage_book::store::calibration::{WindowCalibration, activate, load_result};
use agent_usage_book::store::meter_attempt::{DueReason, NewMeterAttempt, start_meter_attempt};
use agent_usage_book::store::meter_evidence::{
    NewMeterObservation, NewMeterResponseEvidence, NewMeterWindow, insert_observation,
    insert_response_evidence, insert_window,
};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use agent_usage_book::store::reconciliation::reconcile_candidate_from_store;
use agent_usage_book::store::sample_run::{Trigger, start_sample_run};
use agent_usage_book::store::sampling_policy_snapshot::{
    ResolvedSamplingPolicy, resolve_policy_snapshot,
};
use proptest::prelude::*;

const POLICY: ResolvedSamplingPolicy = ResolvedSamplingPolicy {
    ordinary_cadence: MonotonicDuration::from_millis(300_000),
    freshness_horizon: MonotonicDuration::from_millis(900_000),
    reset_edge_policy: String::new(),
    retry_backoff_policy: String::new(),
    command_budget: MonotonicDuration::from_millis(60_000),
    policy_algorithm_version: String::new(),
};

fn fixture_db() -> rusqlite::Connection {
    let mut conn = rusqlite::Connection::open_in_memory().expect("open in memory db");
    run_migrations(
        &mut conn,
        &registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
    )
    .expect("run migrations");
    conn
}

fn insert_calibration(
    conn: &rusqlite::Connection,
    cal_id: &str,
    provider: &str,
    plan: &str,
    window: &str,
    fitted_micros: i64,
) {
    conn.execute(
        "INSERT INTO window_calibration_result (
            calibration_id, provider, plan_tier, window_semantic_key, meter_semantics_id,
            billing_semantics_id, cost_model_id, fitted_micros_per_point,
            equivalent_full_window_capacity_micros, fit_residual_micros, uncertainty_low_micros,
            uncertainty_high_micros, lag_estimate_nanos, lag_handling, sample_count,
            fit_timestamp, inputs_digest, inputs_count, fitting_evidence_digest,
            validation_evidence_digest, validation_method, validation_version,
            out_of_sample_residual_micros, statistical_method, statistical_parameters,
            condition_number_micros, observation_coverage_requirement, settling_policy,
            excluded_samples, activation_policy_version, aub_version, source_revision,
            valid_from, valid_until, knowledge_time
        ) VALUES (
            ?1, ?2, ?3, ?4, 'meter-v1', 'billing-v1', 'cm-1',
            ?5, 12000000, 4200, ?5 - 1000, ?5 + 1000, 90000000000, 'shifted-by-estimate', 40,
            1000, '0123456789abcdef', 3,
            '0123456789abcdef',
            '0123456789abcdef',
            'holdout', 'v2', 7000, 'ols', '{\"ridge\":0}',
            3500000, 'ninety-percent', 'plateau-3', '[]', 'ap-v1', '0.1.0', 'abc1234',
            0, 1000000000000, 1000
        )",
        rusqlite::params![cal_id, provider, plan, window, fitted_micros],
    )
    .expect("insert window_calibration_result");
}

fn load_fixture_calibration(conn: &rusqlite::Connection, cal_id: &str) -> WindowCalibration {
    load_result(conn, &WindowCalibrationId::new(cal_id))
        .expect("load_result ok")
        .expect("calibration exists")
}

fn base_eligible_candidate(cal: WindowCalibration) -> CandidateInterval {
    let account = AccountId::new(1);
    let window_key = WindowSemanticKey::new("five_hour");
    let resets_at = UtcTimestamp::from_unix_nanos(100_000_000);

    let start_obs = CandidateObservation {
        observation_id: EvidenceId::new("obs-start"),
        account_id: account,
        received_at: UtcTimestamp::from_unix_nanos(1_000),
        window_key: window_key.clone(),
        quota_used: QuotaUsed::new(QuotaFractionPpm::new(100_000).unwrap()),
        resets_at,
        reported_resolution: ReportedResolution::new(QuotaFractionPpm::new(1_000).unwrap())
            .unwrap(),
        quantization: QuantizationSemantics::RoundedToNearest,
    };

    let end_obs = CandidateObservation {
        observation_id: EvidenceId::new("obs-end"),
        account_id: account,
        received_at: UtcTimestamp::from_unix_nanos(2_000),
        window_key: window_key.clone(),
        quota_used: QuotaUsed::new(QuotaFractionPpm::new(200_000).unwrap()),
        resets_at,
        reported_resolution: ReportedResolution::new(QuotaFractionPpm::new(1_000).unwrap())
            .unwrap(),
        quantization: QuantizationSemantics::RoundedToNearest,
    };

    let usage_events = vec![CandidateUsageEvent {
        event_id: EvidenceId::new("usage-ev-1"),
        occurred_at: UtcTimestamp::from_unix_nanos(1_500),
        is_measured: true,
        attribution_class: AccountEvidenceClass::ExplicitLauncherOrHook,
        is_quarantined: false,
    }];

    CandidateInterval {
        account_id: account,
        window_key,
        start_observation: start_obs,
        end_observation: end_obs,
        resets_in_interval: Vec::new(),
        coverage: IntervalCoverage::acceptable(),
        active_calibration: Some(cal),
        calibration_health: Some(CalibrationHealth::Current),
        settlement: IntervalSettlement::settled(),
        local_usage: IntervalUsage::new(usage_events, Credits::from_micros(5_000_000)),
        timing_alignment: TimingAlignmentUncertainty::none(),
    }
}

// ---------------------------------------------------------------------------
// Unit tests: 6 eligibility conditions failing in isolation (aub-dpn.1)
// ---------------------------------------------------------------------------

#[test]
fn unit_eligibility_condition_same_account_and_window_fails_in_isolation() {
    let conn = fixture_db();
    insert_calibration(
        &conn,
        "cal-unit-1",
        "anthropic",
        "max",
        "five_hour",
        100_000,
    );
    let cal = load_fixture_calibration(&conn, "cal-unit-1");

    let mut cand = base_eligible_candidate(cal);
    // Account mismatch in start observation.
    cand.start_observation.account_id = AccountId::new(999);

    let assessment = evaluate_eligibility(&cand);
    assert!(!assessment.is_eligible());
    assert_eq!(
        assessment.failing_conditions(),
        &[EligibilityCondition::SameAccountAndWindow]
    );

    let outcome = reconcile(&cand);
    match outcome {
        ReconciliationOutcome::NotComputed { failing_conditions } => {
            assert_eq!(
                failing_conditions,
                vec![EligibilityCondition::SameAccountAndWindow]
            );
        }
        ReconciliationOutcome::Computed(_) => {
            panic!("expected NotComputed when SameAccountAndWindow fails");
        }
    }
}

#[test]
fn unit_eligibility_condition_no_reset_inside_fails_in_isolation() {
    let conn = fixture_db();
    insert_calibration(
        &conn,
        "cal-unit-2",
        "anthropic",
        "max",
        "five_hour",
        100_000,
    );
    let cal = load_fixture_calibration(&conn, "cal-unit-2");

    let mut cand = base_eligible_candidate(cal);
    // A reset occurred inside interval.
    cand.resets_in_interval = vec![UtcTimestamp::from_unix_nanos(1_500)];

    let assessment = evaluate_eligibility(&cand);
    assert!(!assessment.is_eligible());
    assert_eq!(
        assessment.failing_conditions(),
        &[EligibilityCondition::NoResetInside]
    );

    let outcome = reconcile(&cand);
    match outcome {
        ReconciliationOutcome::NotComputed { failing_conditions } => {
            assert_eq!(
                failing_conditions,
                vec![EligibilityCondition::NoResetInside]
            );
        }
        ReconciliationOutcome::Computed(_) => {
            panic!("expected NotComputed when NoResetInside fails");
        }
    }
}

#[test]
fn unit_eligibility_condition_acceptable_meter_coverage_fails_in_isolation() {
    let conn = fixture_db();
    insert_calibration(
        &conn,
        "cal-unit-3",
        "anthropic",
        "max",
        "five_hour",
        100_000,
    );
    let cal = load_fixture_calibration(&conn, "cal-unit-3");

    let mut cand = base_eligible_candidate(cal);
    // Coverage is unacceptable.
    cand.coverage = IntervalCoverage::unacceptable();

    let assessment = evaluate_eligibility(&cand);
    assert!(!assessment.is_eligible());
    assert_eq!(
        assessment.failing_conditions(),
        &[EligibilityCondition::AcceptableMeterCoverage]
    );

    let outcome = reconcile(&cand);
    match outcome {
        ReconciliationOutcome::NotComputed { failing_conditions } => {
            assert_eq!(
                failing_conditions,
                vec![EligibilityCondition::AcceptableMeterCoverage]
            );
        }
        ReconciliationOutcome::Computed(_) => {
            panic!("expected NotComputed when AcceptableMeterCoverage fails");
        }
    }
}

#[test]
fn unit_eligibility_condition_applicable_current_calibration_fails_in_isolation() {
    let conn = fixture_db();
    insert_calibration(
        &conn,
        "cal-unit-4",
        "anthropic",
        "max",
        "five_hour",
        100_000,
    );
    let cal = load_fixture_calibration(&conn, "cal-unit-4");

    let mut cand = base_eligible_candidate(cal);
    // Calibration health is Inapplicable instead of Current.
    cand.calibration_health = Some(CalibrationHealth::Inapplicable);

    let assessment = evaluate_eligibility(&cand);
    assert!(!assessment.is_eligible());
    assert_eq!(
        assessment.failing_conditions(),
        &[EligibilityCondition::ApplicableCurrentCalibration]
    );

    let outcome = reconcile(&cand);
    match outcome {
        ReconciliationOutcome::NotComputed { failing_conditions } => {
            assert_eq!(
                failing_conditions,
                vec![EligibilityCondition::ApplicableCurrentCalibration]
            );
        }
        ReconciliationOutcome::Computed(_) => {
            panic!("expected NotComputed when ApplicableCurrentCalibration fails");
        }
    }
}

#[test]
fn unit_eligibility_condition_sufficient_settlement_and_lag_handling_fails_in_isolation() {
    let conn = fixture_db();
    insert_calibration(
        &conn,
        "cal-unit-5",
        "anthropic",
        "max",
        "five_hour",
        100_000,
    );
    let cal = load_fixture_calibration(&conn, "cal-unit-5");

    let mut cand = base_eligible_candidate(cal);
    // Settlement condition not met: interval is unsettled.
    cand.settlement = IntervalSettlement::unsettled();

    let assessment = evaluate_eligibility(&cand);
    assert!(!assessment.is_eligible());
    assert_eq!(
        assessment.failing_conditions(),
        &[EligibilityCondition::SufficientSettlementAndLagHandling]
    );

    let outcome = reconcile(&cand);
    match outcome {
        ReconciliationOutcome::NotComputed { failing_conditions } => {
            assert_eq!(
                failing_conditions,
                vec![EligibilityCondition::SufficientSettlementAndLagHandling]
            );
        }
        ReconciliationOutcome::Computed(_) => {
            panic!("expected NotComputed when SufficientSettlementAndLagHandling fails");
        }
    }
}

#[test]
fn unit_eligibility_condition_exact_local_usage_where_required_fails_in_isolation() {
    let conn = fixture_db();
    insert_calibration(
        &conn,
        "cal-unit-6",
        "anthropic",
        "max",
        "five_hour",
        100_000,
    );
    let cal = load_fixture_calibration(&conn, "cal-unit-6");

    let mut cand = base_eligible_candidate(cal);
    // Usage contains an unmeasured / estimated event.
    cand.local_usage.events[0].is_measured = false;

    let assessment = evaluate_eligibility(&cand);
    assert!(!assessment.is_eligible());
    assert_eq!(
        assessment.failing_conditions(),
        &[EligibilityCondition::ExactLocalUsageWhereRequired]
    );

    let outcome = reconcile(&cand);
    match outcome {
        ReconciliationOutcome::NotComputed { failing_conditions } => {
            assert_eq!(
                failing_conditions,
                vec![EligibilityCondition::ExactLocalUsageWhereRequired]
            );
        }
        ReconciliationOutcome::Computed(_) => {
            panic!("expected NotComputed when ExactLocalUsageWhereRequired fails");
        }
    }
}

#[test]
fn unit_multiple_eligibility_conditions_failing_are_all_named() {
    let conn = fixture_db();
    insert_calibration(
        &conn,
        "cal-unit-multi",
        "anthropic",
        "max",
        "five_hour",
        100_000,
    );
    let cal = load_fixture_calibration(&conn, "cal-unit-multi");

    let mut cand = base_eligible_candidate(cal);
    cand.coverage = IntervalCoverage::unacceptable();
    cand.settlement = IntervalSettlement::unsettled();

    let assessment = evaluate_eligibility(&cand);
    assert!(!assessment.is_eligible());
    assert_eq!(
        assessment.failing_conditions(),
        &[
            EligibilityCondition::AcceptableMeterCoverage,
            EligibilityCondition::SufficientSettlementAndLagHandling,
        ]
    );

    let outcome = reconcile(&cand);
    match outcome {
        ReconciliationOutcome::NotComputed { failing_conditions } => {
            assert_eq!(
                failing_conditions,
                vec![
                    EligibilityCondition::AcceptableMeterCoverage,
                    EligibilityCondition::SufficientSettlementAndLagHandling,
                ]
            );
        }
        ReconciliationOutcome::Computed(_) => {
            panic!("expected NotComputed when multiple conditions fail");
        }
    }
}

// ---------------------------------------------------------------------------
// Property test: zero local usage and zero meter movement produces zero residual
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn property_zero_movement_and_zero_usage_produces_zero_residual(ppm in 0..=1_000_000i32) {
        let conn = fixture_db();
        insert_calibration(&conn, "cal-prop", "anthropic", "max", "five_hour", 100_000);
        let cal = load_fixture_calibration(&conn, "cal-prop");

        let mut cand = base_eligible_candidate(cal);
        cand.start_observation.quota_used = QuotaUsed::new(QuotaFractionPpm::new(ppm).unwrap());
        cand.end_observation.quota_used = QuotaUsed::new(QuotaFractionPpm::new(ppm).unwrap());
        cand.local_usage = IntervalUsage::empty();

        let outcome = reconcile(&cand);
        match outcome {
            ReconciliationOutcome::Computed(res) => {
                prop_assert_eq!(res.observed_meter_delta().get(), 0);
                prop_assert_eq!(res.observed_meter_credits().micros(), 0);
                prop_assert_eq!(res.locally_explained_credits().micros(), 0);
                prop_assert_eq!(res.explained_interval_change().get(), 0);
                prop_assert_eq!(res.unexplained_residual().micros(), 0);
                prop_assert_eq!(res.unexplained_residual_percentage_points().get(), 0);
            }
            ReconciliationOutcome::NotComputed { failing_conditions } => {
                panic!("expected Computed(0) for valid measurement of nothing, got NotComputed: {failing_conditions:?}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests: output term reading unexplained residual in human and JSON surfaces
// ---------------------------------------------------------------------------

#[test]
fn unit_output_term_unexplained_residual_on_human_and_json_surfaces() {
    let conn = fixture_db();
    insert_calibration(&conn, "cal-term", "anthropic", "max", "five_hour", 100_000);
    let cal = load_fixture_calibration(&conn, "cal-term");

    let cand = base_eligible_candidate(cal);
    let outcome = reconcile(&cand);

    // 1. Human rendered surface on Computed
    let rendered_human = render_reconciliation(&outcome);
    assert!(
        rendered_human.contains("unexplained residual:"),
        "human surface must name 'unexplained residual:' but got:\n{rendered_human}"
    );
    assert!(!rendered_human.to_lowercase().contains("web spend"));
    assert!(
        !rendered_human
            .to_lowercase()
            .contains("unattributed consumption")
    );
    assert!(!rendered_human.to_lowercase().contains("hidden token spend"));

    // 2. JSON rendered surface on Computed
    let json_val = reconciliation_json(&outcome);
    let json_str = serde_json::to_string_pretty(&json_val).expect("json serialize");
    assert!(
        json_val.get("unexplained_residual").is_some(),
        "json surface must contain key 'unexplained_residual'"
    );
    assert!(!json_str.to_lowercase().contains("web_spend"));
    assert!(!json_str.to_lowercase().contains("web spend"));
    assert!(!json_str.to_lowercase().contains("unattributed_consumption"));
    assert!(!json_str.to_lowercase().contains("unattributed consumption"));
    assert!(!json_str.to_lowercase().contains("hidden_token_spend"));
    assert!(!json_str.to_lowercase().contains("hidden token spend"));

    // 3. Human and JSON rendered surface on NotComputed
    let not_computed = ReconciliationOutcome::NotComputed {
        failing_conditions: vec![EligibilityCondition::NoResetInside],
    };
    let human_not_comp = render_reconciliation(&not_computed);
    assert!(
        human_not_comp.contains("unexplained residual: not computed"),
        "human surface for ineligible interval must state 'unexplained residual: not computed'"
    );
    assert!(!human_not_comp.to_lowercase().contains("web spend"));
    assert!(
        !human_not_comp
            .to_lowercase()
            .contains("unattributed consumption")
    );

    let json_not_comp = reconciliation_json(&not_computed);
    let json_not_comp_str = serde_json::to_string(&json_not_comp).unwrap();
    assert_eq!(
        json_not_comp.get("unexplained_residual").unwrap(),
        &serde_json::Value::Null
    );
    assert!(!json_not_comp_str.to_lowercase().contains("web spend"));
    assert!(
        !json_not_comp_str
            .to_lowercase()
            .contains("unattributed consumption")
    );
}

// ---------------------------------------------------------------------------
// Unit tests: residual provenance manifest tracks inputs and witnesses
// ---------------------------------------------------------------------------

#[test]
fn unit_residual_provenance_manifest_identifies_inputs_and_witnesses() {
    let conn = fixture_db();
    insert_calibration(&conn, "cal-prov", "anthropic", "max", "five_hour", 100_000);
    let cal = load_fixture_calibration(&conn, "cal-prov");

    let cand = base_eligible_candidate(cal);
    let outcome = reconcile(&cand);
    let res = outcome.as_computed().expect("must be computed");

    let manifest = res.provenance();
    assert_eq!(manifest.input_count(), 3); // 2 observations + 1 usage event
    assert_eq!(manifest.witnesses().len(), 1); // 1 calibration witness

    let expected_inputs: BTreeSet<EvidenceId> = vec![
        EvidenceId::new("obs-start"),
        EvidenceId::new("obs-end"),
        EvidenceId::new("usage-ev-1"),
    ]
    .into_iter()
    .collect();
    assert!(manifest.verify_expansion(&expected_inputs));

    let witnesses: Vec<WitnessId> = manifest.witnesses().iter().cloned().collect();
    assert_eq!(
        witnesses,
        vec![WitnessId::WindowCalibration(WindowCalibrationId::new(
            "cal-prov"
        ))]
    );
    assert_eq!(res.calibration_id().as_str(), "cal-prov");
}

// ---------------------------------------------------------------------------
// Unit tests: diagnostic patterns reported strictly as patterns, never causes
// ---------------------------------------------------------------------------

#[test]
fn unit_diagnostic_patterns_reported_as_patterns_not_causes() {
    for pattern in ResidualPattern::ALL {
        let desc = pattern.diagnostic_pattern();
        assert!(
            desc.starts_with("pattern: "),
            "pattern description must start with 'pattern: ', got: {desc}"
        );
        assert!(
            desc.contains("possible ") || desc.contains("likely "),
            "pattern description must remain diagnostic hypothesis rather than asserted cause: {desc}"
        );
        assert!(
            !desc.contains("caused by"),
            "must never state definitive causation: {desc}"
        );
    }

    let pos_seq = vec![
        Credits::from_micros(10_000),
        Credits::from_micros(20_000),
        Credits::from_micros(30_000),
    ];
    assert_eq!(
        classify_patterns(&pos_seq),
        vec![ResidualPattern::PersistentlyPositive]
    );

    let neg_seq = vec![
        Credits::from_micros(-10_000),
        Credits::from_micros(-20_000),
        Credits::from_micros(-30_000),
    ];
    assert_eq!(
        classify_patterns(&neg_seq),
        vec![ResidualPattern::PersistentlyNegative]
    );

    let alt_seq = vec![
        Credits::from_micros(10_000),
        Credits::from_micros(-10_000),
        Credits::from_micros(10_000),
        Credits::from_micros(-10_000),
    ];
    assert_eq!(
        classify_patterns(&alt_seq),
        vec![ResidualPattern::AlternatingNetZero]
    );

    let step_seq = vec![
        Credits::from_micros(-10_000),
        Credits::from_micros(-10_000),
        Credits::from_micros(100_000),
        Credits::from_micros(100_000),
    ];
    assert_eq!(
        classify_patterns(&step_seq),
        vec![ResidualPattern::StepChange]
    );
}

// ---------------------------------------------------------------------------
// Integration: known synthetic hidden traffic produces positive residual
// ---------------------------------------------------------------------------

#[test]
fn integration_synthetic_hidden_traffic_produces_positive_residual() {
    let conn = fixture_db();
    insert_calibration(
        &conn,
        "cal-hidden",
        "anthropic",
        "max",
        "five_hour",
        100_000,
    );
    let cal = load_fixture_calibration(&conn, "cal-hidden");

    let mut cand = base_eligible_candidate(cal);
    // Observed meter delta: 100_000 ppm (10 percentage points).
    // Calibration fitted: 100_000 micros per point.
    // Observed meter credits = 100_000 * 100_000 = 10_000_000_000 micros.
    cand.start_observation.quota_used = QuotaUsed::new(QuotaFractionPpm::new(100_000).unwrap());
    cand.end_observation.quota_used = QuotaUsed::new(QuotaFractionPpm::new(200_000).unwrap());

    // Local usage explains 4_000_000_000 micros (40_000 ppm).
    // Hidden traffic exists at provider for remaining 6_000_000_000 micros (60_000 ppm).
    cand.local_usage = IntervalUsage::new(
        vec![CandidateUsageEvent {
            event_id: EvidenceId::new("ev-local"),
            occurred_at: UtcTimestamp::from_unix_nanos(1_500),
            is_measured: true,
            attribution_class: AccountEvidenceClass::ExplicitLauncherOrHook,
            is_quarantined: false,
        }],
        Credits::from_micros(4_000_000_000),
    );

    let outcome = reconcile(&cand);
    let res = outcome.as_computed().expect("must compute residual");

    assert_eq!(res.observed_meter_credits().micros(), 10_000_000_000);
    assert_eq!(res.locally_explained_credits().micros(), 4_000_000_000);
    assert_eq!(res.unexplained_residual().micros(), 6_000_000_000);
    assert_eq!(res.unexplained_residual_percentage_points().get(), 60_000);
    assert!(
        res.unexplained_residual().micros() > 0,
        "residual must be positive for hidden traffic"
    );
}

// ---------------------------------------------------------------------------
// Integration: known calibration overprediction produces negative residual
// ---------------------------------------------------------------------------

#[test]
fn integration_calibration_overprediction_produces_negative_residual() {
    let conn = fixture_db();
    insert_calibration(
        &conn,
        "cal-overpred",
        "anthropic",
        "max",
        "five_hour",
        100_000,
    );
    let cal = load_fixture_calibration(&conn, "cal-overpred");

    let mut cand = base_eligible_candidate(cal);
    // Observed meter delta: 50_000 ppm (5 percentage points).
    // At 100_000 micros per point => 50_000 * 100_000 = 5_000_000_000 micros.
    cand.start_observation.quota_used = QuotaUsed::new(QuotaFractionPpm::new(100_000).unwrap());
    cand.end_observation.quota_used = QuotaUsed::new(QuotaFractionPpm::new(150_000).unwrap());

    // Local usage is measured at 8_000_000_000 micros (80_000 ppm).
    // Calibration overpredicted the points-per-credit, so explained movement exceeds observed.
    cand.local_usage = IntervalUsage::new(
        vec![CandidateUsageEvent {
            event_id: EvidenceId::new("ev-overpred"),
            occurred_at: UtcTimestamp::from_unix_nanos(1_500),
            is_measured: true,
            attribution_class: AccountEvidenceClass::ExplicitLauncherOrHook,
            is_quarantined: false,
        }],
        Credits::from_micros(8_000_000_000),
    );

    let outcome = reconcile(&cand);
    let res = outcome.as_computed().expect("must compute residual");

    assert_eq!(res.observed_meter_credits().micros(), 5_000_000_000);
    assert_eq!(res.locally_explained_credits().micros(), 8_000_000_000);
    assert_eq!(res.unexplained_residual().micros(), -3_000_000_000);
    assert_eq!(res.unexplained_residual_percentage_points().get(), -30_000);
    assert!(
        res.unexplained_residual().micros() < 0,
        "residual must be signed negative for overprediction"
    );
}

// ---------------------------------------------------------------------------
// Integration: single-source calibration proof via shared calibration repository
// ---------------------------------------------------------------------------

#[test]
fn integration_single_source_calibration_proof_updates_provenance_and_explained_change() {
    let mut conn = fixture_db();
    let window_key = WindowSemanticKey::new("five_hour");
    let cost_model_id = CostModelId::new("cm-1");

    // 0. Set up account, sample run, policy snapshot, and attempts to satisfy foreign keys
    let account = observe_account(
        &conn,
        "anthropic",
        "acct-proof",
        UtcTimestamp::from_unix_nanos(100),
    )
    .expect("observe account");

    let run = start_sample_run(
        &conn,
        Trigger::Manual,
        UtcTimestamp::from_unix_nanos(200),
        "reconciliation-test",
    )
    .expect("start sample run");

    let snapshot =
        resolve_policy_snapshot(&conn, account, UtcTimestamp::from_unix_nanos(300), &POLICY)
            .expect("resolve snapshot");

    let att1 = start_meter_attempt(
        &conn,
        &NewMeterAttempt {
            run_id: run,
            account_id: account,
            provider: "anthropic".into(),
            request_started_at: UtcTimestamp::from_unix_nanos(400),
            credential_context_id: Some("ctx-1".into()),
            policy_snapshot_id: snapshot,
            due_at: UtcTimestamp::from_unix_nanos(400),
            due_reason: DueReason::OrdinaryCadence,
            due_basis: None,
            provider_contract_id: "endpoint-v1".into(),
            meter_semantics_id: "meter-v1".into(),
        },
    )
    .expect("att1 inserts");

    let att2 = start_meter_attempt(
        &conn,
        &NewMeterAttempt {
            run_id: run,
            account_id: account,
            provider: "anthropic".into(),
            request_started_at: UtcTimestamp::from_unix_nanos(1_800),
            credential_context_id: Some("ctx-1".into()),
            policy_snapshot_id: snapshot,
            due_at: UtcTimestamp::from_unix_nanos(1_800),
            due_reason: DueReason::OrdinaryCadence,
            due_basis: None,
            provider_contract_id: "endpoint-v1".into(),
            meter_semantics_id: "meter-v1".into(),
        },
    )
    .expect("att2 inserts");

    // 1. Insert response evidence and observations in SQLite store
    let ev1 = insert_response_evidence(
        &conn,
        &NewMeterResponseEvidence {
            attempt_id: att1,
            response_classification: "success".into(),
            received_at: UtcTimestamp::from_unix_nanos(1_000),
            provider_observed_at_original: None,
            evidence_capsule: r#"{"windows":[]}"#.into(),
            capsule_schema_version: "capsule-v1".into(),
            sanitizer_version: "sanitizer-v1".into(),
            capture_truncated: false,
        },
    )
    .expect("ev1 inserts");

    let obs1_id = insert_observation(
        &conn,
        &NewMeterObservation {
            attempt_id: att1,
            evidence_id: ev1,
            account_id: account,
            provider: "anthropic".into(),
            provider_observed_at: None,
            received_at: UtcTimestamp::from_unix_nanos(1_000),
            measurement_basis: MeasurementBasis::LocallyReceived,
            observed_plan: Some("max".into()),
            observed_tier: None,
            adapter_version: AdapterVersion::new("adapter-v1"),
            provider_contract_id: ProviderContractId::new("endpoint-v1"),
            meter_semantics_id: MeterSemanticsId::new("meter-v1"),
            normalized_fingerprint: "fp-1".into(),
        },
    )
    .expect("obs1 inserts");

    insert_window(
        &conn,
        &NewMeterWindow {
            observation_id: obs1_id,
            semantic_key: window_key.clone(),
            scope: WindowScope::AccountWide,
            quota_used: QuotaUsed::new(QuotaFractionPpm::new(100_000).unwrap()),
            reported_resolution: ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap())
                .unwrap(),
            quantization: QuantizationSemantics::RoundedToNearest,
            resets_at: UtcTimestamp::from_unix_nanos(100_000_000),
            nominal_duration: NominalWindowDuration::from_nanos(18_000_000_000_000),
        },
    )
    .expect("win1 inserts");

    let ev2 = insert_response_evidence(
        &conn,
        &NewMeterResponseEvidence {
            attempt_id: att2,
            response_classification: "success".into(),
            received_at: UtcTimestamp::from_unix_nanos(2_000),
            provider_observed_at_original: None,
            evidence_capsule: r#"{"windows":[]}"#.into(),
            capsule_schema_version: "capsule-v1".into(),
            sanitizer_version: "sanitizer-v1".into(),
            capture_truncated: false,
        },
    )
    .expect("ev2 inserts");

    let obs2_id = insert_observation(
        &conn,
        &NewMeterObservation {
            attempt_id: att2,
            evidence_id: ev2,
            account_id: account,
            provider: "anthropic".into(),
            provider_observed_at: None,
            received_at: UtcTimestamp::from_unix_nanos(2_000),
            measurement_basis: MeasurementBasis::LocallyReceived,
            observed_plan: Some("max".into()),
            observed_tier: None,
            adapter_version: AdapterVersion::new("adapter-v1"),
            provider_contract_id: ProviderContractId::new("endpoint-v1"),
            meter_semantics_id: MeterSemanticsId::new("meter-v1"),
            normalized_fingerprint: "fp-2".into(),
        },
    )
    .expect("obs2 inserts");

    insert_window(
        &conn,
        &NewMeterWindow {
            observation_id: obs2_id,
            semantic_key: window_key.clone(),
            scope: WindowScope::AccountWide,
            quota_used: QuotaUsed::new(QuotaFractionPpm::new(200_000).unwrap()),
            reported_resolution: ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap())
                .unwrap(),
            quantization: QuantizationSemantics::RoundedToNearest,
            resets_at: UtcTimestamp::from_unix_nanos(100_000_000),
            nominal_duration: NominalWindowDuration::from_nanos(18_000_000_000_000),
        },
    )
    .expect("win2 inserts");

    // 2. Insert and activate calibration 1: fitted = 100_000 micros per point
    insert_calibration(
        &conn,
        "wcr-proof-1",
        "anthropic",
        "max",
        "five_hour",
        100_000,
    );
    activate(
        &mut conn,
        &WindowCalibrationId::new("wcr-proof-1"),
        UtcTimestamp::from_unix_nanos(500),
        None,
    )
    .expect("activate cal 1");

    // 3. Reconcile candidate from store with calibration 1 in force
    let outcome1 = reconcile_candidate_from_store(
        &conn,
        account,
        obs1_id,
        obs2_id,
        &window_key,
        &cost_model_id,
        UtcTimestamp::from_unix_nanos(1_500),
        UtcTimestamp::from_unix_nanos(2_500),
    )
    .expect("reconcile candidate 1");

    let res1 = outcome1.as_computed().expect("must compute residual 1");
    assert_eq!(res1.calibration_id().as_str(), "wcr-proof-1");
    // 100_000 ppm = 100_000 points. At 100_000 micros per point => 10_000_000_000 micros credits.
    assert_eq!(res1.observed_meter_credits().micros(), 10_000_000_000);

    let cal1_witness = WitnessId::WindowCalibration(WindowCalibrationId::new("wcr-proof-1"));
    assert!(res1.provenance().witnesses().contains(&cal1_witness));

    // 4. Append and activate superseding calibration 2: fitted = 200_000 micros per point
    insert_calibration(
        &conn,
        "wcr-proof-2",
        "anthropic",
        "max",
        "five_hour",
        200_000,
    );
    activate(
        &mut conn,
        &WindowCalibrationId::new("wcr-proof-2"),
        UtcTimestamp::from_unix_nanos(3_000),
        Some(&WindowCalibrationId::new("wcr-proof-1")),
    )
    .expect("activate cal 2 superseding cal 1");

    // 5. Recalculate without editing source or configuration (at knowledge time 3_500)
    let outcome2 = reconcile_candidate_from_store(
        &conn,
        account,
        obs1_id,
        obs2_id,
        &window_key,
        &cost_model_id,
        UtcTimestamp::from_unix_nanos(3_500),
        UtcTimestamp::from_unix_nanos(2_500),
    )
    .expect("reconcile candidate 2");

    let res2 = outcome2.as_computed().expect("must compute residual 2");
    // Provenance ID and explained interval change updated together!
    assert_eq!(res2.calibration_id().as_str(), "wcr-proof-2");
    assert_eq!(res2.observed_meter_credits().micros(), 20_000_000_000);

    let cal2_witness = WitnessId::WindowCalibration(WindowCalibrationId::new("wcr-proof-2"));
    assert!(res2.provenance().witnesses().contains(&cal2_witness));
    assert!(!res2.provenance().witnesses().contains(&cal1_witness));
}

// ---------------------------------------------------------------------------
// aub-dpn.2: residual uncertainty propagated from its three sources
// ---------------------------------------------------------------------------

/// A calibration row with an explicitly chosen coefficient uncertainty band, so a
/// test can widen calibration uncertainty in isolation.
fn insert_calibration_with_band(
    conn: &rusqlite::Connection,
    cal_id: &str,
    fitted_micros: i64,
    uncertainty_low_micros: i64,
    uncertainty_high_micros: i64,
) {
    conn.execute(
        "INSERT INTO window_calibration_result (
            calibration_id, provider, plan_tier, window_semantic_key, meter_semantics_id,
            billing_semantics_id, cost_model_id, fitted_micros_per_point,
            equivalent_full_window_capacity_micros, fit_residual_micros, uncertainty_low_micros,
            uncertainty_high_micros, lag_estimate_nanos, lag_handling, sample_count,
            fit_timestamp, inputs_digest, inputs_count, fitting_evidence_digest,
            validation_evidence_digest, validation_method, validation_version,
            out_of_sample_residual_micros, statistical_method, statistical_parameters,
            condition_number_micros, observation_coverage_requirement, settling_policy,
            excluded_samples, activation_policy_version, aub_version, source_revision,
            valid_from, valid_until, knowledge_time
        ) VALUES (
            ?1, 'anthropic', 'max', 'five_hour', 'meter-v1', 'billing-v1', 'cm-1',
            ?2, 12000000, 4200, ?3, ?4, 90000000000, 'shifted-by-estimate', 40,
            1000, '0123456789abcdef', 3,
            '0123456789abcdef',
            '0123456789abcdef',
            'holdout', 'v2', 7000, 'ols', '{\"ridge\":0}',
            3500000, 'ninety-percent', 'plateau-3', '[]', 'ap-v1', '0.1.0', 'abc1234',
            0, 1000000000000, 1000
        )",
        rusqlite::params![
            cal_id,
            fitted_micros,
            uncertainty_low_micros,
            uncertainty_high_micros
        ],
    )
    .expect("insert window_calibration_result with band");
}

/// A base-eligible candidate with per-observation reported resolution and an
/// explicit timing-alignment uncertainty. The interval carries movement of
/// 100_000 ppm and 5_000_000 micro-credits of locally explained usage.
fn eligible_with(
    cal: WindowCalibration,
    start_resolution_ppm: i32,
    end_resolution_ppm: i32,
    timing: TimingAlignmentUncertainty,
) -> CandidateInterval {
    let mut cand = base_eligible_candidate(cal);
    cand.start_observation.reported_resolution =
        ReportedResolution::new(QuotaFractionPpm::new(start_resolution_ppm).unwrap()).unwrap();
    cand.end_observation.reported_resolution =
        ReportedResolution::new(QuotaFractionPpm::new(end_resolution_ppm).unwrap()).unwrap();
    cand.timing_alignment = timing;
    cand
}

fn computed(
    outcome: ReconciliationOutcome,
) -> agent_usage_book::reconciliation::ReconciledResidual {
    match outcome {
        ReconciliationOutcome::Computed(res) => *res,
        ReconciliationOutcome::NotComputed { failing_conditions } => {
            panic!("expected Computed, got NotComputed: {failing_conditions:?}")
        }
    }
}

// Criterion: quantization uncertainty is derived per observation from its persisted
// resolution, never from one global tolerance.
#[test]
fn unit_quantization_bounds_are_per_observation_not_a_global_tolerance() {
    let conn = fixture_db();
    insert_calibration(&conn, "cal-q", "anthropic", "max", "five_hour", 100_000);
    let cal = load_fixture_calibration(&conn, "cal-q");

    // start reported at 2_000 ppm, end reported at 10_000 ppm, both round-to-nearest.
    let asymmetric = computed(reconcile(&eligible_with(
        cal.clone(),
        2_000,
        10_000,
        TimingAlignmentUncertainty::none(),
    )));
    // Interval subtraction of the two bands: width is the sum of the two resolutions.
    assert_eq!(
        asymmetric.observed_meter_delta_bounds().width_ppm(),
        12_000,
        "delta bounds must widen by each observation's own resolution"
    );

    // Both observations at 2_000 ppm: a single global tolerance could not produce
    // both this width and the one above.
    let symmetric = computed(reconcile(&eligible_with(
        cal,
        2_000,
        2_000,
        TimingAlignmentUncertainty::none(),
    )));
    assert_eq!(symmetric.observed_meter_delta_bounds().width_ppm(), 4_000);
}

// Criterion / Done when: the same scenario at a coarse vs a fine provider
// resolution produces a wider interval for the coarse one.
#[test]
fn unit_coarser_provider_resolution_produces_a_wider_residual_interval() {
    let conn = fixture_db();
    insert_calibration(&conn, "cal-r", "anthropic", "max", "five_hour", 100_000);
    let cal = load_fixture_calibration(&conn, "cal-r");

    let coarse = computed(reconcile(&eligible_with(
        cal.clone(),
        50_000,
        50_000,
        TimingAlignmentUncertainty::none(),
    )));
    let fine = computed(reconcile(&eligible_with(
        cal,
        1_000,
        1_000,
        TimingAlignmentUncertainty::none(),
    )));

    let coarse_width = coarse.unexplained_residual_interval().upper().micros()
        - coarse.unexplained_residual_interval().lower().micros();
    let fine_width = fine.unexplained_residual_interval().upper().micros()
        - fine.unexplained_residual_interval().lower().micros();
    assert!(
        coarse_width > fine_width,
        "coarse interval width {coarse_width} must exceed fine width {fine_width}"
    );
}

// Criterion: calibration uncertainty propagates through the credits conversion with
// interval arithmetic. Quantization is held to Exact so the only width in the
// residual interval comes from the coefficient's stated band.
#[test]
fn unit_calibration_uncertainty_propagates_through_the_credits_conversion() {
    let conn = fixture_db();
    insert_calibration_with_band(&conn, "cal-narrow", 100_000, 99_000, 101_000);
    insert_calibration_with_band(&conn, "cal-wide", 100_000, 90_000, 110_000);

    let exact = |cal| {
        let mut cand = eligible_with(cal, 1_000, 1_000, TimingAlignmentUncertainty::none());
        cand.start_observation.quantization = QuantizationSemantics::Exact;
        cand.end_observation.quantization = QuantizationSemantics::Exact;
        cand
    };

    let narrow = computed(reconcile(&exact(load_fixture_calibration(
        &conn,
        "cal-narrow",
    ))));
    let wide = computed(reconcile(&exact(load_fixture_calibration(
        &conn, "cal-wide",
    ))));

    // Movement is a fixed 100_000 ppm point; the interval width is
    // (coefficient_upper - coefficient_lower) * 100_000 micro-credits.
    let width = |r: &agent_usage_book::reconciliation::ReconciledResidual| {
        r.unexplained_residual_interval().upper().micros()
            - r.unexplained_residual_interval().lower().micros()
    };
    assert_eq!(width(&narrow), 2_000 * 100_000);
    assert_eq!(width(&wide), 20_000 * 100_000);
}

// Criterion: a residual interval containing zero reconciles within uncertainty and
// is not reported as a finding.
#[test]
fn unit_residual_interval_containing_zero_reconciles_within_uncertainty() {
    let conn = fixture_db();
    insert_calibration(&conn, "cal-z", "anthropic", "max", "five_hour", 100_000);
    let cal = load_fixture_calibration(&conn, "cal-z");

    // Observed movement is 100_000 ppm at 100_000 micros/point => 10_000_000_000
    // micro-credits. Locally explaining exactly that puts the point residual at zero
    // and the interval straddling it.
    let mut cand = eligible_with(
        cal.clone(),
        1_000,
        1_000,
        TimingAlignmentUncertainty::none(),
    );
    cand.local_usage = IntervalUsage::new(
        cand.local_usage.events.clone(),
        Credits::from_micros(10_000_000_000),
    );
    let res = computed(reconcile(&cand));

    assert!(res.reconciles_within_uncertainty());
    assert!(res.unexplained_residual_interval().lower().micros() < 0);
    assert!(res.unexplained_residual_interval().upper().micros() > 0);

    let human = render_reconciliation(&ReconciliationOutcome::Computed(Box::new(res.clone())));
    assert!(human.contains("reconciles within uncertainty"));
    assert!(!human.contains("residual interval excludes zero"));

    let json = reconciliation_json(&ReconciliationOutcome::Computed(Box::new(res)));
    assert_eq!(
        json.get("reconciles_within_uncertainty").unwrap(),
        &serde_json::json!(true)
    );

    // Mutation control: a residual interval that excludes zero is not called reconciling.
    let mut finding = eligible_with(cal, 1_000, 1_000, TimingAlignmentUncertainty::none());
    finding.local_usage = IntervalUsage::new(
        finding.local_usage.events.clone(),
        Credits::from_micros(5_000_000),
    );
    let finding_res = computed(reconcile(&finding));
    assert!(!finding_res.reconciles_within_uncertainty());
    assert!(
        render_reconciliation(&ReconciliationOutcome::Computed(Box::new(finding_res)))
            .contains("residual interval excludes zero")
    );
}

// Contract: both endpoints of the residual interval survive into human and JSON output.
#[test]
fn contract_both_residual_interval_endpoints_reach_human_and_json() {
    let conn = fixture_db();
    insert_calibration(&conn, "cal-c", "anthropic", "max", "five_hour", 100_000);
    let cal = load_fixture_calibration(&conn, "cal-c");

    let outcome = reconcile(&eligible_with(
        cal,
        2_000,
        4_000,
        TimingAlignmentUncertainty::from_credit_half_width(Credits::from_micros(1_000_000)),
    ));
    let res = computed(outcome.clone());
    let lower = res.unexplained_residual_interval().lower().micros();
    let upper = res.unexplained_residual_interval().upper().micros();
    assert!(lower < upper, "a non-degenerate interval is expected here");

    let human = render_reconciliation(&outcome);
    assert!(human.contains(&format!(
        "unexplained residual interval: [{lower} .. {upper}] credits"
    )));

    let json = reconciliation_json(&outcome);
    let interval = json
        .get("unexplained_residual_interval")
        .expect("interval key present");
    assert_eq!(
        interval.get("lower").unwrap(),
        &serde_json::json!(lower.to_string())
    );
    assert_eq!(
        interval.get("upper").unwrap(),
        &serde_json::json!(upper.to_string())
    );
    assert_eq!(interval.get("unit").unwrap(), &serde_json::json!("credits"));
    assert!(json.get("observed_meter_credits_interval").is_some());
    assert!(json.get("observed_meter_delta_bounds_ppm").is_some());
}

// Isolated timing-alignment coverage (bead's dedicated section): hold meter
// quantization and calibration uncertainty fixed, widen only timing alignment, and
// assert the residual interval cannot narrow. Then assert timing-alignment
// provenance reaches both renderers.
#[test]
fn unit_widening_only_timing_alignment_never_narrows_and_is_rendered() {
    let conn = fixture_db();
    insert_calibration(&conn, "cal-t", "anthropic", "max", "five_hour", 100_000);
    let cal = load_fixture_calibration(&conn, "cal-t");

    let narrow = computed(reconcile(&eligible_with(
        cal.clone(),
        1_000,
        1_000,
        TimingAlignmentUncertainty::none(),
    )));
    let wide_outcome = reconcile(&eligible_with(
        cal,
        1_000,
        1_000,
        TimingAlignmentUncertainty::from_credit_half_width(Credits::from_micros(7_000_000)),
    ));
    let wide = computed(wide_outcome.clone());

    assert!(
        wide.unexplained_residual_interval().lower().micros()
            <= narrow.unexplained_residual_interval().lower().micros()
    );
    assert!(
        wide.unexplained_residual_interval().upper().micros()
            >= narrow.unexplained_residual_interval().upper().micros()
    );
    let narrow_width = narrow.unexplained_residual_interval().upper().micros()
        - narrow.unexplained_residual_interval().lower().micros();
    let wide_width = wide.unexplained_residual_interval().upper().micros()
        - wide.unexplained_residual_interval().lower().micros();
    assert_eq!(
        wide_width - narrow_width,
        14_000_000,
        "a +/- band widens by twice its half width"
    );

    let human = render_reconciliation(&wide_outcome);
    assert!(human.contains("timing alignment uncertainty: +/-7000000 credits"));
    let json = reconciliation_json(&wide_outcome);
    assert_eq!(
        json.get("timing_alignment_uncertainty")
            .and_then(|t| t.get("credits_micros_half_width"))
            .unwrap(),
        &serde_json::json!(7_000_000)
    );
}

proptest! {
    // The non-narrowing law across all three uncertainty sources: widening any one
    // input can never narrow the residual interval.
    #[test]
    fn prop_widening_any_uncertainty_source_never_narrows_residual_interval(
        base_resolution in 1_000i32..=90_000i32,
        resolution_widen in 1i32..=90_000i32,
        band in 0i64..=40_000i64,
        band_widen in 1i64..=40_000i64,
        base_timing in 0i64..=3_000_000i64,
        timing_widen in 1i64..=5_000_000i64,
        which in 0u8..3u8,
    ) {
        let conn = fixture_db();
        insert_calibration_with_band(&conn, "cal-a", 100_000, 100_000 - band, 100_000 + band);
        let cal_a = load_fixture_calibration(&conn, "cal-a");

        let base_timing_unc =
            TimingAlignmentUncertainty::from_credit_half_width(Credits::from_micros(base_timing));
        let base = eligible_with(cal_a.clone(), base_resolution, base_resolution, base_timing_unc);

        let widened = match which {
            0 => eligible_with(
                cal_a.clone(),
                base_resolution + resolution_widen,
                base_resolution + resolution_widen,
                base_timing_unc,
            ),
            1 => {
                insert_calibration_with_band(
                    &conn,
                    "cal-b",
                    100_000,
                    100_000 - band - band_widen,
                    100_000 + band + band_widen,
                );
                let cal_b = load_fixture_calibration(&conn, "cal-b");
                eligible_with(cal_b, base_resolution, base_resolution, base_timing_unc)
            }
            _ => eligible_with(
                cal_a.clone(),
                base_resolution,
                base_resolution,
                TimingAlignmentUncertainty::from_credit_half_width(Credits::from_micros(
                    base_timing + timing_widen,
                )),
            ),
        };

        let base_res = computed(reconcile(&base));
        let widened_res = computed(reconcile(&widened));
        let base_interval = base_res.unexplained_residual_interval();
        let widened_interval = widened_res.unexplained_residual_interval();
        prop_assert!(widened_interval.lower().micros() <= base_interval.lower().micros());
        prop_assert!(widened_interval.upper().micros() >= base_interval.upper().micros());
    }
}
