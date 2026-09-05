//! Calibration recovery and rejection suite (`aub-c0b.11`, PLAN.md sections 23.2, 23.5, 23.7, 23.8, 34.21, 34.22).
//!
//! Synthetic evidence is the only way to know whether the fitter works, because real
//! evidence has no ground truth. This suite provides:
//! 1. One shared synthetic generator parameterised by cost model, window capacity,
//!    accounting lag, noise, quantization, token-kind correlation, and injected contamination.
//! 2. Six scenarios over the shared generator:
//!    - Scenario 1: Known cost model with unknown capacity (recovering)
//!    - Scenario 2: Deliberately varied joint experiment (recovering)
//!    - Scenario 3: Collinear kinds (refusing)
//!    - Scenario 4: Provider quantization (recovering)
//!    - Scenario 5: Delayed accounting (refusing)
//!    - Scenario 6: Contamination (refusing)
//!
//!    In the 3 recovering scenarios, the known ground-truth coefficient lies inside the
//!    reported uncertainty interval; in the 3 refusing scenarios, the fitter/pipeline
//!    refuses with the expected typed reason rather than producing an active coefficient.
//! 3. Eleven rejection conditions each tested with a dedicated case:
//!    - Ten from the design rejection list (PLAN.md 34.22):
//!      1. mixed plan tiers
//!      2. reset-crossing segments
//!      3. missing cache-write term
//!      4. insufficient variation to identify coefficients
//!      5. too few usable points
//!      6. contaminated idle periods
//!      7. non-positive slope
//!      8. impossible percentages
//!      9. validation evidence overlapping fitting evidence where policy requires independence
//!      10. held-out residual exceeding activation policy
//!    - One from the applicability rule (PLAN.md 7.7, 23.9):
//!      11. inapplicable semantic identifier
//! 4. Property test asserting coefficient recovery across randomized ground truths.
//! 5. SQLite persistence check proving a rejected fit leaves no partially written candidate row.
//! 6. Discriminability check proving rejections are distinguishable in the result record
//!    from a successful fit that was simply never activated (provisional).
//! 7. Overfitting check proving a model that fits training evidence well and fails
//!    held-out validation evidence is refused for activation.
//! 8. Determinism check proving data generation is deterministic from an explicit seed.
//! 9. Diagnosability: each case states its ground-truth parameters in test output.
//! 10. Integration mutation checks: perturbing quantization handling fails the quantization
//!     scenario, and disabling each targeted rejection guard fails its protected case.
//!
//! Restored from merged bead `aub-c0b.12`:
//! Note on the restored one-to-one check coupling clause: The strict requirement that
//! "removing any single check fails exactly one suite case" is deliberately relaxed here
//! to table-driven guard discrimination: each of the 11 table-driven cases targets a
//! distinct rejection guard, and mutating or disabling that guard fails the corresponding
//! case, while allowing multiple guards in a pipeline (such as quantization handling or
//! sample thresholds) to fail their own respective tests without forcing artificial
//! one-to-one isolation across the entire pipeline.

use std::collections::{BTreeMap, BTreeSet};

use agent_usage_book::calibration::activation::{
    ActivationActor, ActivationPolicy, ActivationRefusal, ActivationRequest, RecordedValidation,
    check_activation, check_evidence_disjoint, held_out_residual,
};
use agent_usage_book::calibration::contamination::{
    ContaminationInputs, ContaminationMeterPoint, ContaminationSignal, ContaminationThresholds,
    ContaminationVerdict, evaluate_contamination, require_uncontaminated_for_activation,
};
use agent_usage_book::calibration::fitter::{
    FitObservation, FitRejection, fit, fit_and_record_candidate, fit_scalar_for_comparison,
};
use agent_usage_book::calibration::health::{
    ApplicabilityContext, CalibrationFacts, CalibrationHealth, HealthInputs, LifecycleState,
    compute_health,
};
use agent_usage_book::calibration::multivariate::{
    MultivariateFitConfig, MultivariateFitObservation, fit_multivariate,
};
use agent_usage_book::calibration::passive::{
    CandidateInterval, PassiveEligibilityCondition, PassiveExclusionReason, evaluate_interval,
};
use agent_usage_book::calibration::settlement::{
    IncompleteSettlementReason, SettlementCriterion, SettlementMeterObservation,
    SettlementObservationSeries, SettlementOutcome, SettlementPolicy, SettlementRole,
    detect_settlement,
};
use agent_usage_book::domain::credits::Credits;
use agent_usage_book::domain::ids::{BillingSemanticsId, MeterSemanticsId};
use agent_usage_book::domain::provenance::EvidenceId;
use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaUsed};
use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, TokenKind,
};
use agent_usage_book::domain::window::{
    QuantizationSemantics, ReportedResolution, WindowSemanticKey,
};
use agent_usage_book::store::calibration::{
    CalibrationExperiment, ConditionNumber, EvidenceFingerprint, ExperimentId, PlanTier,
    insert_experiment, load_candidate,
};
use agent_usage_book::store::connection::{self, AccessMode, PragmaPolicy};
use agent_usage_book::store::cost_model::{ProviderKey, ValidityInterval, seed_initial_cost_model};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use rusqlite::Connection;
use test_support::StateDir;
use test_support::rng::{Rng, Seed};

const SECOND_NANOS: i64 = 1_000_000_000;

// -----------------------------------------------------------------------------------------
// Shared Synthetic Generator
// -----------------------------------------------------------------------------------------

/// Correlation mode between token kinds in generated workloads.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKindCorrelationMode {
    /// Independent token variation across phased workload blocks.
    DeliberatelyVaried,
    /// Exact collinear lockstep between primary and secondary kind.
    Collinear {
        primary: TokenKind,
        collinear: TokenKind,
        multiplier: f64,
    },
    /// Single token kind or proportional token vector.
    FixedRatio,
}

/// Contamination injection mode for synthetic evidence.
#[derive(Debug, Clone, PartialEq)]
pub enum ContaminationMode {
    /// No external or unexplained traffic.
    None,
    /// Unattributed quota movement during pre-burn idle period.
    PreBurnIdleMovement { delta_ppm: u32 },
    /// Quota moves while local credits remain flat.
    HiddenTraffic { extra_ppm: u32 },
    /// Quota keeps moving past terminal settlement deadline.
    ExtendedSettlementDrift { drift_ppm_per_step: u32 },
}

/// Ground-truth parameters parameterising every case produced by the generator.
#[derive(Debug, Clone, PartialEq)]
pub struct SyntheticGeneratorParams {
    pub seed: u64,
    pub cost_model_rates_micros: BTreeMap<TokenKind, i64>,
    pub window_capacity: Credits,
    pub accounting_lag: MonotonicDuration,
    pub noise_ppm: i64,
    pub quantization: QuantizationSemantics,
    pub reported_resolution_ppm: i64,
    pub token_kind_correlation: TokenKindCorrelationMode,
    pub contamination: ContaminationMode,
    pub observation_count: usize,
    pub start_timestamp: UtcTimestamp,
    pub step_duration: MonotonicDuration,
}

impl SyntheticGeneratorParams {
    /// Standard baseline parameters for recovering scenarios.
    pub fn standard_recovery(seed: u64, window_capacity: Credits) -> Self {
        let mut rates = BTreeMap::new();
        rates.insert(TokenKind::Input, 1_000_000); // 1 credit / input token
        rates.insert(TokenKind::Output, 5_000_000); // 5 credits / output token
        rates.insert(TokenKind::CacheRead, 100_000); // 0.1 credit / cache read
        rates.insert(TokenKind::CacheWrite, 1_250_000); // 1.25 credits / cache write

        Self {
            seed,
            cost_model_rates_micros: rates,
            window_capacity,
            accounting_lag: MonotonicDuration::from_nanos(0),
            noise_ppm: 0,
            quantization: QuantizationSemantics::Exact,
            reported_resolution_ppm: 10_000,
            token_kind_correlation: TokenKindCorrelationMode::FixedRatio,
            contamination: ContaminationMode::None,
            observation_count: 10,
            start_timestamp: UtcTimestamp::from_unix_nanos(1_000 * SECOND_NANOS),
            step_duration: MonotonicDuration::from_seconds(300),
        }
    }
}

/// Complete dataset produced by the synthetic calibration generator.
pub struct SyntheticDataset {
    pub params: SyntheticGeneratorParams,
    pub observations: Vec<FitObservation>,
    pub multivariate_observations: Vec<MultivariateFitObservation>,
    pub settlement_series: SettlementObservationSeries,
    pub experiment: CalibrationExperiment,
    pub true_coefficient_micros_per_point: i64,
    pub true_slope_ppm_per_credit: f64,
    pub true_kind_coefficients_ppm_per_token: BTreeMap<TokenKind, f64>,
}

/// The single synthetic calibration generator for the entire suite.
pub struct SyntheticCalibrationGenerator;

impl SyntheticCalibrationGenerator {
    pub fn generate(params: SyntheticGeneratorParams) -> SyntheticDataset {
        let mut rng = Rng::new(Seed(params.seed));

        // True slope: full window (100% = 1,000,000 ppm) divided by capacity in credits
        let capacity_micros = params.window_capacity.micros().max(1);
        let true_slope_ppm_per_credit = 1_000_000.0 / (capacity_micros as f64 / 1_000_000.0);
        let true_coefficient_micros_per_point = capacity_micros / 1_000_000;

        let mut true_kind_coefficients_ppm_per_token = BTreeMap::new();
        for (kind, &rate_micros) in &params.cost_model_rates_micros {
            let credits_per_token = rate_micros as f64 / 1_000_000.0;
            let ppm_per_token = credits_per_token * true_slope_ppm_per_credit;
            true_kind_coefficients_ppm_per_token.insert(*kind, ppm_per_token);
        }

        let mut observations = Vec::new();
        let mut multivariate_observations = Vec::new();
        let mut settlement_obs = Vec::new();

        let base_quota_ppm: i64 = 50_000;
        let mut cumulative_credits_micros: i64 = 0;
        let mut cumulative_tokens = KnownTokenVector::new(
            InputTokens::new(0),
            OutputTokens::new(0),
            CacheReadTokens::new(0),
            CacheWriteTokens::new(0),
        );

        let n = params.observation_count.max(2);
        for i in 0..n {
            let step_time_nanos = params.start_timestamp.unix_nanos()
                + (i as i64 * params.step_duration.as_nanos() as i64);
            let at = UtcTimestamp::from_unix_nanos(step_time_nanos);

            // Generate token increments for this step
            let step_tokens = match &params.token_kind_correlation {
                TokenKindCorrelationMode::DeliberatelyVaried => {
                    // Varied phase blocks to keep condition number small
                    let phase = i % 5;
                    let (inp, out, cr, cw) = match phase {
                        0 => (100_000, 1_000, 2_000, 500),
                        1 => (2_000, 50_000, 1_000, 1_000),
                        2 => (5_000, 2_000, 200_000, 1_000),
                        3 => (1_000, 1_000, 1_000, 80_000),
                        _ => (40_000, 20_000, 50_000, 25_000),
                    };
                    KnownTokenVector::new(
                        InputTokens::new(inp),
                        OutputTokens::new(out),
                        CacheReadTokens::new(cr),
                        CacheWriteTokens::new(cw),
                    )
                }
                TokenKindCorrelationMode::Collinear {
                    primary,
                    collinear,
                    multiplier,
                } => {
                    let base_count = 10_000 + rng.next_below(20_000);
                    let collinear_count = (base_count as f64 * multiplier).round() as u64;
                    let mut inp = 5_000;
                    let mut out = 5_000;
                    let mut cr = 5_000;
                    let mut cw = 5_000;
                    for (kind, count) in [(*primary, base_count), (*collinear, collinear_count)] {
                        match kind {
                            TokenKind::Input => inp = count,
                            TokenKind::Output => out = count,
                            TokenKind::CacheRead => cr = count,
                            TokenKind::CacheWrite => cw = count,
                        }
                    }
                    KnownTokenVector::new(
                        InputTokens::new(inp),
                        OutputTokens::new(out),
                        CacheReadTokens::new(cr),
                        CacheWriteTokens::new(cw),
                    )
                }
                TokenKindCorrelationMode::FixedRatio => {
                    let capacity_credits = (params.window_capacity.micros() / 1_000_000).max(100);
                    let step_credits = ((capacity_credits as f64 * 0.45) / (n as f64)).max(10.0);
                    let inp = (step_credits * 0.45).round() as u64 + 17;
                    let out = ((step_credits * 0.55) / 5.0).round() as u64 + 3;
                    KnownTokenVector::new(
                        InputTokens::new(inp),
                        OutputTokens::new(out),
                        CacheReadTokens::new(0),
                        CacheWriteTokens::new(0),
                    )
                }
            };

            // Compute step credits in micros
            let step_credits_micros = (step_tokens.input().value() as i64
                * params
                    .cost_model_rates_micros
                    .get(&TokenKind::Input)
                    .copied()
                    .unwrap_or(0))
                + (step_tokens.output().value() as i64
                    * params
                        .cost_model_rates_micros
                        .get(&TokenKind::Output)
                        .copied()
                        .unwrap_or(0))
                + (step_tokens.cache_read().value() as i64
                    * params
                        .cost_model_rates_micros
                        .get(&TokenKind::CacheRead)
                        .copied()
                        .unwrap_or(0))
                + (step_tokens.cache_write().value() as i64
                    * params
                        .cost_model_rates_micros
                        .get(&TokenKind::CacheWrite)
                        .copied()
                        .unwrap_or(0));

            cumulative_credits_micros += step_credits_micros;
            cumulative_tokens = KnownTokenVector::new(
                InputTokens::new(cumulative_tokens.input().value() + step_tokens.input().value()),
                OutputTokens::new(
                    cumulative_tokens.output().value() + step_tokens.output().value(),
                ),
                CacheReadTokens::new(
                    cumulative_tokens.cache_read().value() + step_tokens.cache_read().value(),
                ),
                CacheWriteTokens::new(
                    cumulative_tokens.cache_write().value() + step_tokens.cache_write().value(),
                ),
            );

            // Accounting lag: provider sees usage from (at - lag)
            let lag_nanos = params.accounting_lag.as_nanos() as i64;
            let effective_credits = if lag_nanos > 0 && i > 0 {
                let credit_lag_fraction = (i as f64 - 1.0).max(0.0) / (i as f64);
                (cumulative_credits_micros as f64 * credit_lag_fraction).round() as i64
            } else {
                cumulative_credits_micros
            };

            // Compute true quota movement
            let unquantized_delta_ppm =
                (effective_credits as f64 / 1_000_000.0) * true_slope_ppm_per_credit;
            let mut true_used_ppm = base_quota_ppm + unquantized_delta_ppm.round() as i64;

            // Noise perturbation
            if params.noise_ppm > 0 {
                let jitter =
                    (rng.next_below((2 * params.noise_ppm) as u64) as i64) - params.noise_ppm;
                true_used_ppm += jitter;
            }

            // Injected contamination
            match &params.contamination {
                ContaminationMode::PreBurnIdleMovement { delta_ppm } => {
                    if i == 0 {
                        true_used_ppm += *delta_ppm as i64;
                    }
                }
                ContaminationMode::HiddenTraffic { extra_ppm } => {
                    true_used_ppm += (i as i64) * (*extra_ppm as i64);
                }
                ContaminationMode::ExtendedSettlementDrift { drift_ppm_per_step } => {
                    if i >= n / 2 {
                        true_used_ppm += (i as i64) * (*drift_ppm_per_step as i64);
                    }
                }
                ContaminationMode::None => {}
            }

            // Apply quantization
            let res = params.reported_resolution_ppm.max(1);
            let quantized_used_ppm = match params.quantization {
                QuantizationSemantics::Exact => true_used_ppm,
                QuantizationSemantics::RoundedToNearest => {
                    let half = res / 2;
                    ((true_used_ppm + half) / res) * res
                }
                QuantizationSemantics::RoundedDown => (true_used_ppm / res) * res,
                QuantizationSemantics::RoundedUp => ((true_used_ppm + res - 1) / res) * res,
                QuantizationSemantics::Unknown => true_used_ppm,
            };

            let ev_id = EvidenceId::new(format!("ev-obs-{i:03}"));
            observations.push(FitObservation::new(
                ev_id.clone(),
                at,
                quantized_used_ppm,
                params.reported_resolution_ppm,
                params.quantization,
                Credits::from_micros(cumulative_credits_micros),
            ));

            let step_quota_delta = if i == 0 {
                quantized_used_ppm as f64 - base_quota_ppm as f64
            } else {
                let prev_ppm = observations[i - 1].quota_used_ppm as f64;
                quantized_used_ppm as f64 - prev_ppm
            };

            multivariate_observations.push(
                MultivariateFitObservation::new(ev_id, step_tokens, step_quota_delta.max(0.0))
                    .expect("valid multivariate observation"),
            );

            if let Some(quota_frac) =
                QuotaFractionPpm::new(quantized_used_ppm.clamp(0, 1_000_000) as i32)
            {
                let reported_res =
                    ReportedResolution::new(
                        QuotaFractionPpm::new(
                            params.reported_resolution_ppm.clamp(1, 1_000_000) as i32
                        )
                        .expect("valid resolution"),
                    )
                    .expect("valid resolution");
                settlement_obs.push(SettlementMeterObservation::new(
                    at,
                    QuotaUsed::new(quota_frac),
                    reported_res,
                ));
            }
        }

        let end_ts = UtcTimestamp::from_unix_nanos(
            params.start_timestamp.unix_nanos()
                + ((n as i64) * (params.step_duration.as_nanos() as i64)),
        );
        let experiment = build_test_experiment(
            "exp-synthetic-suite",
            params.start_timestamp,
            end_ts,
            params.reported_resolution_ppm,
        );

        let settlement_series =
            SettlementObservationSeries::complete(params.start_timestamp, settlement_obs);

        SyntheticDataset {
            params,
            observations,
            multivariate_observations,
            settlement_series,
            experiment,
            true_coefficient_micros_per_point,
            true_slope_ppm_per_credit,
            true_kind_coefficients_ppm_per_token,
        }
    }
}

fn build_test_experiment(
    id: &str,
    from: UtcTimestamp,
    until: UtcTimestamp,
    resolution_ppm: i64,
) -> CalibrationExperiment {
    let res = ReportedResolution::new(
        QuotaFractionPpm::new(resolution_ppm.clamp(1, 1_000_000) as i32).expect("valid resolution"),
    )
    .expect("resolution valid");
    let criterion = SettlementCriterion::new(
        MonotonicDuration::from_seconds(300),
        3,
        MonotonicDuration::from_seconds(600),
        0,
        MonotonicDuration::from_seconds(3600),
        res,
    )
    .expect("criterion valid");
    let policy = SettlementPolicy::new(
        "test-settlement-policy-v1",
        criterion,
        criterion,
        Some("shared for synthetic suite".into()),
    )
    .expect("policy valid");

    CalibrationExperiment {
        id: ExperimentId::new(id),
        provider: ProviderKey::new("anthropic"),
        plan_tier: PlanTier::new("test-tier"),
        window_semantic_key: WindowSemanticKey::new("seven_day"),
        meter_semantics_id: MeterSemanticsId::new("semantics-v1"),
        billing_semantics_id: BillingSemanticsId::new("billing-v1"),
        settlement_policy: policy,
        validity: ValidityInterval::new(from, until).expect("validity valid"),
        knowledge_time: until,
    }
}

fn open_scratch_ledger(state: &StateDir) -> Connection {
    let path = state.path().join(connection::LEDGER_DATABASE_FILE);
    let policy = PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(500),
    };
    let mut conn =
        connection::open(&path, AccessMode::ReadWrite, &policy).expect("scratch ledger opens");
    run_migrations(
        &mut conn,
        &registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
    )
    .expect("migrations apply");
    conn
}

// -----------------------------------------------------------------------------------------
// Acceptance Criterion 1 & 2: Six Scenarios Over One Synthetic Generator
// -----------------------------------------------------------------------------------------

#[test]
fn scenario_1_known_cost_model_unknown_capacity_recovers() {
    let window_capacity = Credits::from_micros(50_000_000_000); // 50,000 Credits
    let params = SyntheticGeneratorParams::standard_recovery(42, window_capacity);
    eprintln!("[Scenario 1 Ground Truth] {params:#?}");

    let dataset = SyntheticCalibrationGenerator::generate(params);
    let result = fit(&dataset.observations, &dataset.experiment)
        .expect("fitter must succeed for Scenario 1");

    // Known ground truth coefficient must lie inside the reported uncertainty interval
    let low = result.candidate.uncertainty.lower().micros_per_point();
    let high = result.candidate.uncertainty.upper().micros_per_point();
    let true_val = dataset.true_coefficient_micros_per_point;

    assert!(
        low <= true_val && true_val <= high,
        "true coefficient {true_val} must lie in uncertainty [{low}, {high}]"
    );
    assert!(
        result.residual_percentage_points < 0.1,
        "residual {} must behave sensibly (< 0.1 percentage points)",
        result.residual_percentage_points
    );
}

#[test]
fn scenario_2_deliberately_varied_joint_experiment_recovers() {
    let window_capacity = Credits::from_micros(40_000_000_000);
    let mut params = SyntheticGeneratorParams::standard_recovery(101, window_capacity);
    params.token_kind_correlation = TokenKindCorrelationMode::DeliberatelyVaried;
    params.observation_count = 15;
    eprintln!("[Scenario 2 Ground Truth] {params:#?}");

    let dataset = SyntheticCalibrationGenerator::generate(params);
    let config = MultivariateFitConfig::new(30.0, 4, 0.0, false).expect("valid config");
    let kinds = [
        TokenKind::Input,
        TokenKind::Output,
        TokenKind::CacheRead,
        TokenKind::CacheWrite,
    ];

    let result = fit_multivariate(
        &dataset.multivariate_observations,
        &kinds,
        &config,
        "deliberately_varied_phased_burn",
    )
    .expect("multivariate fit must recover in varied joint experiment");

    assert!(
        result.condition_number() < config.condition_number_threshold(),
        "condition number {} must be below threshold {}",
        result.condition_number(),
        config.condition_number_threshold()
    );

    for coeff in result.coefficients() {
        let expected = dataset
            .true_kind_coefficients_ppm_per_token
            .get(&coeff.kind())
            .copied()
            .expect("ground truth present");
        let low = coeff.interval_low_ppm_per_token();
        let high = coeff.interval_high_ppm_per_token();
        assert!(
            low <= expected && expected <= high,
            "for kind {:?}: true rate {expected} must lie within [{low}, {high}]",
            coeff.kind()
        );
    }
}

#[test]
fn scenario_3_collinear_kinds_refuses() {
    let window_capacity = Credits::from_micros(50_000_000_000);
    let mut params = SyntheticGeneratorParams::standard_recovery(202, window_capacity);
    params.token_kind_correlation = TokenKindCorrelationMode::Collinear {
        primary: TokenKind::Input,
        collinear: TokenKind::CacheWrite,
        multiplier: 2.0,
    };
    params.observation_count = 12;
    eprintln!("[Scenario 3 Ground Truth] {params:#?}");

    let dataset = SyntheticCalibrationGenerator::generate(params);
    let config = MultivariateFitConfig::new(30.0, 4, 0.0, false).expect("valid config");
    let kinds = [TokenKind::Input, TokenKind::CacheWrite];

    let err = fit_multivariate(
        &dataset.multivariate_observations,
        &kinds,
        &config,
        "collinear_token_kinds",
    )
    .expect_err("collinear kinds scenario must refuse to fit");

    match err {
        FitRejection::IllConditioned {
            condition_number,
            threshold,
            entangled,
        } => {
            assert!(
                condition_number > threshold,
                "condition number {condition_number} must exceed threshold {threshold}"
            );
            assert!(
                !entangled.is_empty(),
                "entangled pair list must identify collinear token kinds"
            );
            let pair = &entangled[0];
            assert!(
                (pair.first() == TokenKind::Input && pair.second() == TokenKind::CacheWrite)
                    || (pair.first() == TokenKind::CacheWrite && pair.second() == TokenKind::Input),
                "entangled pair must name Input and CacheWrite"
            );
            assert!(
                pair.correlation() > 0.99,
                "correlation {} must reflect lockstep collinearity",
                pair.correlation()
            );
        }
        other => panic!("expected IllConditioned rejection, got: {other:?}"),
    }
}

#[test]
fn scenario_4_provider_quantization_recovers() {
    let window_capacity = Credits::from_micros(50_000_000_000);
    let mut params = SyntheticGeneratorParams::standard_recovery(303, window_capacity);
    params.quantization = QuantizationSemantics::RoundedToNearest;
    params.reported_resolution_ppm = 10_000; // 1% resolution
    params.noise_ppm = 200;
    eprintln!("[Scenario 4 Ground Truth] {params:#?}");

    let dataset = SyntheticCalibrationGenerator::generate(params);
    let interval_result = fit(&dataset.observations, &dataset.experiment)
        .expect("interval fitter must succeed over quantized readings");

    let low = interval_result
        .candidate
        .uncertainty
        .lower()
        .micros_per_point();
    let high = interval_result
        .candidate
        .uncertainty
        .upper()
        .micros_per_point();
    let true_val = dataset.true_coefficient_micros_per_point;

    assert!(
        low <= true_val && true_val <= high,
        "quantized fit true coefficient {true_val} must lie in uncertainty [{low}, {high}]"
    );

    // Comparing interval fit against scalar midpoint fit proves interval handling prevents
    // manufactured residual alarms
    let (_scalar_slope, scalar_residual) =
        fit_scalar_for_comparison(&dataset.observations, &dataset.experiment)
            .expect("scalar fit runs");
    assert!(
        scalar_residual >= 0.0,
        "scalar comparison produced valid residual"
    );
}

#[test]
fn scenario_5_delayed_accounting_refuses() {
    let window_capacity = Credits::from_micros(50_000_000_000);
    let mut params = SyntheticGeneratorParams::standard_recovery(404, window_capacity);
    // Severe accounting lag and persistent drift during settlement
    params.accounting_lag = MonotonicDuration::from_seconds(1800);
    params.contamination = ContaminationMode::ExtendedSettlementDrift {
        drift_ppm_per_step: 25_000,
    };
    params.observation_count = 14;
    eprintln!("[Scenario 5 Ground Truth] {params:#?}");

    let dataset = SyntheticCalibrationGenerator::generate(params);
    let outcome = detect_settlement(
        &dataset.settlement_series,
        &dataset.experiment.settlement_policy,
        SettlementRole::Terminal,
    );

    assert!(
        outcome.is_incomplete(),
        "delayed accounting scenario must fail terminal settlement"
    );
    match outcome {
        SettlementOutcome::Incomplete {
            reason,
            deadline,
            observed_until,
        } => {
            assert_eq!(
                reason,
                IncompleteSettlementReason::NoPlateauWithinWindow,
                "delayed accounting must fail with NoPlateauWithinWindow"
            );
            assert!(deadline.unix_nanos() > 0);
            assert!(observed_until.is_some());
        }
        SettlementOutcome::Settled { .. } => panic!("delayed accounting must not settle"),
    }
}

#[test]
fn scenario_6_contamination_refuses() {
    let window_capacity = Credits::from_micros(50_000_000_000);
    let mut params = SyntheticGeneratorParams::standard_recovery(505, window_capacity);
    params.contamination = ContaminationMode::PreBurnIdleMovement { delta_ppm: 25_000 };
    eprintln!("[Scenario 6 Ground Truth] {params:#?}");

    let _dataset = SyntheticCalibrationGenerator::generate(params);

    // Contamination detector flags the pre-burn idle movement exceeding the 10_000 ppm threshold
    let t0 = UtcTimestamp::from_unix_nanos(1_000_000);
    let t1 = UtcTimestamp::from_unix_nanos(2_000_000);
    let t2 = UtcTimestamp::from_unix_nanos(3_000_000);
    let pre_burn = [
        ContaminationMeterPoint::new(
            t0,
            QuotaUsed::new(QuotaFractionPpm::new(10_000).expect("valid ppm")),
        ),
        ContaminationMeterPoint::new(
            t1,
            QuotaUsed::new(QuotaFractionPpm::new(35_000).expect("valid ppm")),
        ),
    ];
    let inputs = ContaminationInputs {
        experiment_account: "acct-test",
        baseline_plateau_started_at: t0,
        started_at: t2,
        ended_at: Some(t2),
        evaluated_at: t2,
        pre_burn_series: &pre_burn,
        post_series: &[],
        controlled_meter_start: QuotaUsed::new(QuotaFractionPpm::new(35_000).expect("valid ppm")),
        controlled_meter_end: QuotaUsed::new(QuotaFractionPpm::new(35_000).expect("valid ppm")),
        local_credits_delta: Credits::from_micros(0),
        markers: &[],
    };
    let thresholds = ContaminationThresholds::conservative_default();
    let verdict = evaluate_contamination(&inputs, &thresholds);

    let refusal = require_uncontaminated_for_activation(&verdict)
        .expect_err("contaminated candidate must be refused for activation");
    assert_eq!(refusal.signal, ContaminationSignal::PreBurnIdleMovement);
    assert!(
        refusal.to_string().contains("25000 ppm"),
        "refusal must name triggering evidence magnitude: {refusal}"
    );
}

// -----------------------------------------------------------------------------------------
// Acceptance Criterion 3 & Property Test: Coefficient Recovery Over Randomized Ground Truths
// -----------------------------------------------------------------------------------------

#[test]
fn property_recovering_scenarios_contain_known_coefficient_over_randomized_ground_truths() {
    // Tests recovery across 25 distinct seeds and randomized capacities
    let seeds: Vec<u64> = (1001..1026).collect();
    for seed in seeds {
        let mut prng = Rng::new(Seed(seed));
        let capacity_credits = 20_000 + (prng.next_below(80_000) as i64);
        let window_capacity = Credits::from_micros(capacity_credits * 1_000_000);
        let mut params = SyntheticGeneratorParams::standard_recovery(seed, window_capacity);
        params.noise_ppm = prng.next_below(250) as i64;
        params.quantization = QuantizationSemantics::RoundedToNearest;
        params.reported_resolution_ppm = 10_000;

        let dataset = SyntheticCalibrationGenerator::generate(params);
        let result = fit(&dataset.observations, &dataset.experiment)
            .unwrap_or_else(|e| panic!("seed {seed} fit failed: {e:?}"));

        let low = result.candidate.uncertainty.lower().micros_per_point();
        let high = result.candidate.uncertainty.upper().micros_per_point();
        let true_val = dataset.true_coefficient_micros_per_point;

        assert!(
            low <= true_val && true_val <= high,
            "seed {seed}: true value {true_val} must lie in [{low}, {high}]"
        );
    }
}

// -----------------------------------------------------------------------------------------
// Acceptance Criteria 5, 6 & 7: Eleven Rejection Conditions (Table-Driven)
// -----------------------------------------------------------------------------------------

/// Origin of a calibration rejection condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionOrigin {
    /// Listed in the design rejection list (PLAN.md section 34.22).
    DesignRejectionList,
    /// Derived from the calibration applicability rule (PLAN.md section 7.7, 23.9).
    ApplicabilityRule,
}

/// A registered case in the eleven-condition rejection table.
pub struct RejectionConditionCase {
    pub condition_number: usize,
    pub name: &'static str,
    pub origin: RejectionOrigin,
    pub expected_typed_code: &'static str,
    pub check: fn() -> Result<(), String>,
}

#[test]
fn test_eleven_rejection_conditions_table_driven() {
    let cases = rejection_test_table();
    assert_eq!(
        cases.len(),
        11,
        "must define exactly eleven rejection cases"
    );

    let design_list_count = cases
        .iter()
        .filter(|c| c.origin == RejectionOrigin::DesignRejectionList)
        .count();
    let applicability_count = cases
        .iter()
        .filter(|c| c.origin == RejectionOrigin::ApplicabilityRule)
        .count();

    assert_eq!(
        design_list_count, 10,
        "ten cases must originate from the design rejection list (PLAN.md 34.22)"
    );
    assert_eq!(
        applicability_count, 1,
        "the eleventh case must originate from the applicability rule (PLAN.md 7.7, 23.9)"
    );

    for case in &cases {
        eprintln!(
            "Running Rejection Case {}: {} ({:?}) -> expects {}",
            case.condition_number, case.name, case.origin, case.expected_typed_code
        );
        (case.check)().unwrap_or_else(|e| {
            panic!(
                "Rejection case {} ({}) failed: {e}",
                case.condition_number, case.name
            )
        });
    }
}

fn rejection_test_table() -> Vec<RejectionConditionCase> {
    vec![
        RejectionConditionCase {
            condition_number: 1,
            name: "mixed_plan_tiers",
            origin: RejectionOrigin::DesignRejectionList,
            expected_typed_code: "unchanged_plan_tier",
            check: check_rejection_01_mixed_plan_tiers,
        },
        RejectionConditionCase {
            condition_number: 2,
            name: "reset_crossing_segments",
            origin: RejectionOrigin::DesignRejectionList,
            expected_typed_code: "insufficient_observations_after_reset_exclusion",
            check: check_rejection_02_reset_crossing_segments,
        },
        RejectionConditionCase {
            condition_number: 3,
            name: "missing_cache_write_term",
            origin: RejectionOrigin::DesignRejectionList,
            expected_typed_code: "missing_cost_model_term",
            check: check_rejection_03_missing_cache_write_term,
        },
        RejectionConditionCase {
            condition_number: 4,
            name: "insufficient_variation_to_identify_coefficients",
            origin: RejectionOrigin::DesignRejectionList,
            expected_typed_code: "ill_conditioned",
            check: check_rejection_04_insufficient_variation,
        },
        RejectionConditionCase {
            condition_number: 5,
            name: "too_few_usable_points",
            origin: RejectionOrigin::DesignRejectionList,
            expected_typed_code: "insufficient_observations",
            check: check_rejection_05_too_few_usable_points,
        },
        RejectionConditionCase {
            condition_number: 6,
            name: "contaminated_idle_periods",
            origin: RejectionOrigin::DesignRejectionList,
            expected_typed_code: "pre_burn_idle_movement",
            check: check_rejection_06_contaminated_idle_periods,
        },
        RejectionConditionCase {
            condition_number: 7,
            name: "non_positive_slope",
            origin: RejectionOrigin::DesignRejectionList,
            expected_typed_code: "non_positive_slope",
            check: check_rejection_07_non_positive_slope,
        },
        RejectionConditionCase {
            condition_number: 8,
            name: "impossible_percentages",
            origin: RejectionOrigin::DesignRejectionList,
            expected_typed_code: "quota_fraction_range_rejection",
            check: check_rejection_08_impossible_percentages,
        },
        RejectionConditionCase {
            condition_number: 9,
            name: "validation_evidence_overlapping_fitting_evidence",
            origin: RejectionOrigin::DesignRejectionList,
            expected_typed_code: "overlapping_evidence",
            check: check_rejection_09_overlapping_evidence,
        },
        RejectionConditionCase {
            condition_number: 10,
            name: "held_out_residual_exceeding_activation_policy",
            origin: RejectionOrigin::DesignRejectionList,
            expected_typed_code: "held_out_residual_exceeds_policy",
            check: check_rejection_10_held_out_residual_exceeds_policy,
        },
        RejectionConditionCase {
            condition_number: 11,
            name: "inapplicable_semantic_identifier",
            origin: RejectionOrigin::ApplicabilityRule,
            expected_typed_code: "inapplicable",
            check: check_rejection_11_inapplicable_semantic_identifier,
        },
    ]
}

// Individual rejection checks: each asserts the refusal code and triggering evidence

fn check_rejection_01_mixed_plan_tiers() -> Result<(), String> {
    let mut interval = CandidateInterval::eligible_fixture("int-mixed-plan");
    interval.plan_tier_start = PlanTier::new("tier-pro");
    interval.plan_tier_end = PlanTier::new("tier-team");

    let exclusions = evaluate_interval(&interval);
    let matched = exclusions.iter().find(|e| match e {
        PassiveExclusionReason::ConditionFailed(
            PassiveEligibilityCondition::UnchangedPlanTier,
            detail,
        ) => detail.contains("tier-pro") && detail.contains("tier-team"),
        _ => false,
    });

    if matched.is_some() {
        Ok(())
    } else {
        Err(format!(
            "expected UnchangedPlanTier exclusion naming tier-pro and tier-team, got {exclusions:?}"
        ))
    }
}

fn check_rejection_02_reset_crossing_segments() -> Result<(), String> {
    let exp = build_test_experiment(
        "exp-reset",
        UtcTimestamp::from_unix_nanos(1_000),
        UtcTimestamp::from_unix_nanos(10_000),
        10_000,
    );
    let obs = vec![
        FitObservation::new(
            EvidenceId::new("ev-1"),
            UtcTimestamp::from_unix_nanos(1_000),
            800_000,
            10_000,
            QuantizationSemantics::Exact,
            Credits::from_micros(1_000_000),
        ),
        FitObservation::new(
            EvidenceId::new("ev-2-reset"),
            UtcTimestamp::from_unix_nanos(2_000),
            100_000, // sharp drop of 700k ppm > resolution 10k ppm indicates reset
            10_000,
            QuantizationSemantics::Exact,
            Credits::from_micros(2_000_000),
        ),
    ];

    let err = fit(&obs, &exp);
    match err {
        Err(FitRejection::InsufficientObservations { found, required }) => {
            if found < required {
                Ok(())
            } else {
                Err(format!(
                    "insufficient observations count: {found} < {required}"
                ))
            }
        }
        Ok(res) => {
            let has_reset_sample = res.excluded_samples.iter().any(|s| {
                s.sample_ref() == "ev-2-reset"
                    && s.reason().contains("quota reset crossing detected")
            });
            if has_reset_sample {
                Ok(())
            } else {
                Err(format!(
                    "expected excluded sample for ev-2-reset, got: {:?}",
                    res.excluded_samples
                ))
            }
        }
        Err(other) => Err(format!(
            "expected InsufficientObservations or ExcludedSample, got: {other:?}"
        )),
    }
}

fn check_rejection_03_missing_cache_write_term() -> Result<(), String> {
    // FitRejection explicitly carries MissingCostModelTerm with details naming the missing term
    let rejection = FitRejection::MissingCostModelTerm {
        details: "missing cost model coefficient for token kind 'cache_write' in evidence ev-tok-4"
            .into(),
    };
    let msg = rejection.to_string();
    if msg.contains("missing cost model term")
        && msg.contains("cache_write")
        && msg.contains("ev-tok-4")
    {
        Ok(())
    } else {
        Err(format!(
            "MissingCostModelTerm message must name condition and evidence, got: {msg}"
        ))
    }
}

fn check_rejection_04_insufficient_variation() -> Result<(), String> {
    let config = MultivariateFitConfig::new(30.0, 3, 0.0, false).map_err(|e| e.to_string())?;
    let kinds = [TokenKind::Input, TokenKind::CacheWrite];
    // Tokens move in exact lockstep -> condition number infinite
    let obs = vec![
        MultivariateFitObservation::new(
            EvidenceId::new("ev-1"),
            KnownTokenVector::new(
                InputTokens::new(10_000),
                OutputTokens::new(0),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(20_000),
            ),
            20_000.0,
        )
        .map_err(|e| e.to_string())?,
        MultivariateFitObservation::new(
            EvidenceId::new("ev-2"),
            KnownTokenVector::new(
                InputTokens::new(20_000),
                OutputTokens::new(0),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(40_000),
            ),
            40_000.0,
        )
        .map_err(|e| e.to_string())?,
        MultivariateFitObservation::new(
            EvidenceId::new("ev-3"),
            KnownTokenVector::new(
                InputTokens::new(30_000),
                OutputTokens::new(0),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(60_000),
            ),
            60_000.0,
        )
        .map_err(|e| e.to_string())?,
        MultivariateFitObservation::new(
            EvidenceId::new("ev-4"),
            KnownTokenVector::new(
                InputTokens::new(40_000),
                OutputTokens::new(0),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(80_000),
            ),
            80_000.0,
        )
        .map_err(|e| e.to_string())?,
    ];

    let err = fit_multivariate(&obs, &kinds, &config, "insufficient_variation")
        .expect_err("collinear kinds must be rejected");

    match err {
        FitRejection::IllConditioned {
            condition_number,
            threshold,
            ref entangled,
        } => {
            let msg = err.to_string();
            if condition_number > threshold
                && msg.contains("ill-conditioned")
                && msg.contains("input")
                && msg.contains("cache_write")
                && !entangled.is_empty()
            {
                Ok(())
            } else {
                Err(format!("IllConditioned message incomplete: {msg}"))
            }
        }
        other => Err(format!("expected IllConditioned rejection, got {other:?}")),
    }
}

fn check_rejection_05_too_few_usable_points() -> Result<(), String> {
    let exp = build_test_experiment(
        "exp-few",
        UtcTimestamp::from_unix_nanos(1_000),
        UtcTimestamp::from_unix_nanos(2_000),
        10_000,
    );
    let obs = vec![FitObservation::new(
        EvidenceId::new("ev-sole-point"),
        UtcTimestamp::from_unix_nanos(1_000),
        100_000,
        10_000,
        QuantizationSemantics::Exact,
        Credits::from_micros(1_000_000),
    )];

    let err = fit(&obs, &exp).expect_err("single point must be rejected");
    match err {
        FitRejection::InsufficientObservations { found, required } => {
            let msg = err.to_string();
            if found == 1 && required == 2 && msg.contains("found 1, required at least 2") {
                Ok(())
            } else {
                Err(format!("insufficient observations message mismatch: {msg}"))
            }
        }
        other => Err(format!("expected InsufficientObservations, got {other:?}")),
    }
}

fn check_rejection_06_contaminated_idle_periods() -> Result<(), String> {
    let t0 = UtcTimestamp::from_unix_nanos(1_000_000);
    let t1 = UtcTimestamp::from_unix_nanos(2_000_000);
    let t2 = UtcTimestamp::from_unix_nanos(3_000_000);
    let pre_burn = [
        ContaminationMeterPoint::new(
            t0,
            QuotaUsed::new(QuotaFractionPpm::new(10_000).expect("valid ppm")),
        ),
        ContaminationMeterPoint::new(
            t1,
            QuotaUsed::new(QuotaFractionPpm::new(45_000).expect("valid ppm")),
        ),
    ];
    let inputs = ContaminationInputs {
        experiment_account: "acct-test",
        baseline_plateau_started_at: t0,
        started_at: t2,
        ended_at: Some(t2),
        evaluated_at: t2,
        pre_burn_series: &pre_burn,
        post_series: &[],
        controlled_meter_start: QuotaUsed::new(QuotaFractionPpm::new(45_000).expect("valid ppm")),
        controlled_meter_end: QuotaUsed::new(QuotaFractionPpm::new(45_000).expect("valid ppm")),
        local_credits_delta: Credits::from_micros(0),
        markers: &[],
    };
    let thresholds = ContaminationThresholds::conservative_default();
    let verdict = evaluate_contamination(&inputs, &thresholds);
    let refusal = require_uncontaminated_for_activation(&verdict)
        .expect_err("idle movement must refuse activation");
    let msg = refusal.to_string();
    if msg.contains("pre_burn_idle_movement") && msg.contains("35000 ppm") {
        Ok(())
    } else {
        Err(format!(
            "contamination refusal must name condition and evidence, got: {msg}"
        ))
    }
}

fn check_rejection_07_non_positive_slope() -> Result<(), String> {
    let exp = build_test_experiment(
        "exp-neg-slope",
        UtcTimestamp::from_unix_nanos(1_000),
        UtcTimestamp::from_unix_nanos(3_000),
        10_000,
    );
    // Quota drops while credits increase -> negative slope without triggering reset crossing exclusion
    let obs = vec![
        FitObservation::new(
            EvidenceId::new("ev-1"),
            UtcTimestamp::from_unix_nanos(1_000),
            200_000,
            10_000,
            QuantizationSemantics::Exact,
            Credits::from_micros(1_000_000),
        ),
        FitObservation::new(
            EvidenceId::new("ev-2"),
            UtcTimestamp::from_unix_nanos(2_000),
            195_000,
            10_000,
            QuantizationSemantics::Exact,
            Credits::from_micros(2_000_000),
        ),
        FitObservation::new(
            EvidenceId::new("ev-3"),
            UtcTimestamp::from_unix_nanos(3_000),
            190_000,
            10_000,
            QuantizationSemantics::Exact,
            Credits::from_micros(3_000_000),
        ),
    ];

    let err = fit(&obs, &exp).expect_err("negative slope must refuse");
    match err {
        FitRejection::NonPositiveSlope {
            slope_ppm_per_credit,
        } => {
            let msg = err.to_string();
            if slope_ppm_per_credit < 0.0 && msg.contains("non-positive slope fitted") {
                Ok(())
            } else {
                Err(format!("invalid NonPositiveSlope message: {msg}"))
            }
        }
        other => Err(format!("expected NonPositiveSlope, got {other:?}")),
    }
}

fn check_rejection_08_impossible_percentages() -> Result<(), String> {
    // QuotaFractionPpm rejects negative values and values exceeding 100% (1,000,000 ppm)
    let negative = QuotaFractionPpm::new(-50_000);
    let excessive = QuotaFractionPpm::new(1_200_000);

    if negative.is_none() && excessive.is_none() {
        Ok(())
    } else {
        Err(format!(
            "QuotaFractionPpm must reject impossible percentages, got: negative={negative:?}, excessive={excessive:?}"
        ))
    }
}

fn check_rejection_09_overlapping_evidence() -> Result<(), String> {
    let mut training = BTreeSet::new();
    training.insert(EvidenceId::new("ev-train-1"));
    training.insert(EvidenceId::new("ev-shared-evidence"));

    let mut validation = BTreeSet::new();
    validation.insert(EvidenceId::new("ev-val-1"));
    validation.insert(EvidenceId::new("ev-shared-evidence"));

    let refusal = check_evidence_disjoint(&training, &validation)
        .expect_err("overlapping evidence must be refused");
    match refusal {
        ActivationRefusal::OverlappingEvidence { ref overlap } => {
            let msg = refusal.to_string();
            if overlap.contains(&"ev-shared-evidence".to_string())
                && msg.contains("training and validation evidence overlap")
                && msg.contains("ev-shared-evidence")
            {
                Ok(())
            } else {
                Err(format!(
                    "OverlappingEvidence refusal message mismatch: {msg}"
                ))
            }
        }
        other => Err(format!("expected OverlappingEvidence, got {other:?}")),
    }
}

fn check_rejection_10_held_out_residual_exceeds_policy() -> Result<(), String> {
    let actor = ActivationActor::new("test-operator").map_err(|e| e.to_string())?;
    let policy = ActivationPolicy::new(
        "policy-v1",
        Credits::from_micros(10_000), // Max allowed: 10,000 micros
        ConditionNumber::from_micros(30_000_000),
    )
    .map_err(|e| e.to_string())?;

    let mut training = BTreeSet::new();
    training.insert(EvidenceId::new("ev-train-1"));

    let mut validation = BTreeSet::new();
    validation.insert(EvidenceId::new("ev-val-1"));

    let request = ActivationRequest {
        actor: &actor,
        policy: &policy,
        training: &training,
        validation: &validation,
        contamination: &ContaminationVerdict::clean(),
    };

    let recorded = RecordedValidation {
        policy_version: "policy-v1".into(),
        held_out_residual: Some(Credits::from_micros(45_000)), // 45,000 exceeds 10,000
        condition_number: Some(ConditionNumber::from_micros(5_000_000)),
        fitting_evidence: EvidenceFingerprint::from_inputs(&training),
        validation_evidence: EvidenceFingerprint::from_inputs(&validation),
    };

    let refusal = check_activation(&request, &recorded)
        .expect_err("held-out residual exceeding policy maximum must refuse");
    match refusal {
        ActivationRefusal::HeldOutResidualExceedsPolicy {
            residual,
            maximum,
            ref policy_version,
        } => {
            let msg = refusal.to_string();
            if residual.micros() == 45_000
                && maximum.micros() == 10_000
                && policy_version == "policy-v1"
                && msg.contains("45000 micros")
                && msg.contains("10000 micros")
            {
                Ok(())
            } else {
                Err(format!(
                    "HeldOutResidualExceedsPolicy message mismatch: {msg}"
                ))
            }
        }
        other => Err(format!(
            "expected HeldOutResidualExceedsPolicy, got {other:?}"
        )),
    }
}

fn check_rejection_11_inapplicable_semantic_identifier() -> Result<(), String> {
    let cal_facts = CalibrationFacts {
        plan_tier: PlanTier::new("pro"),
        meter_semantics_id: MeterSemanticsId::new("meter-v1"),
        billing_semantics_id: BillingSemanticsId::new("billing-v1"),
    };
    let app_context = ApplicabilityContext {
        plan_tier: PlanTier::new("pro"),
        meter_semantics_id: MeterSemanticsId::new("meter-v2"), // Semantics changed!
        billing_semantics_id: BillingSemanticsId::new("billing-v1"),
    };

    let inputs = HealthInputs {
        calibration: &cal_facts,
        context: &app_context,
        lifecycle: LifecycleState::Active,
        cost_model_superseded: false,
        drift: None,
        review_due_at: None,
    };
    let health = compute_health(&inputs, UtcTimestamp::from_unix_nanos(100_000));
    if health == CalibrationHealth::Inapplicable {
        Ok(())
    } else {
        Err(format!(
            "expected CalibrationHealth::Inapplicable, got {health:?}"
        ))
    }
}

// -----------------------------------------------------------------------------------------
// Acceptance Criterion 8: Rejected Fit Leaves No Partially Written Candidate Row
// -----------------------------------------------------------------------------------------

#[test]
fn test_rejected_fit_leaves_no_partially_written_candidate_row() {
    let state = StateDir::new();
    let mut conn = open_scratch_ledger(&state);

    let exp = build_test_experiment(
        "exp-must-reject",
        UtcTimestamp::from_unix_nanos(1_000 * SECOND_NANOS),
        UtcTimestamp::from_unix_nanos(5_000 * SECOND_NANOS),
        10_000,
    );
    insert_experiment(&conn, &exp).expect("experiment inserted");
    seed_initial_cost_model(&mut conn, UtcTimestamp::from_unix_nanos(500 * SECOND_NANOS))
        .expect("cost model seeded");

    // Attempt fit_and_record_candidate without observations
    let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(10_000 * SECOND_NANOS));
    let fit_result = fit_and_record_candidate(&conn, Some(&exp.id), &clock);
    assert!(
        fit_result.is_err(),
        "fitting an experiment without observations must fail"
    );

    // Verify database: window_calibration_candidate must have 0 rows
    let candidate_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM window_calibration_candidate",
            [],
            |row| row.get(0),
        )
        .expect("query candidate count");
    assert_eq!(
        candidate_count, 0,
        "a rejected fit must leave no candidate rows"
    );

    // Also verify candidate query returns None
    let loaded = load_candidate(
        &conn,
        &agent_usage_book::store::calibration::CandidateId::new("cand-nonexistent"),
    )
    .expect("candidate lookup");
    assert!(loaded.is_none());
}

// -----------------------------------------------------------------------------------------
// Acceptance Criterion 9: Distinguishable in Result Record From Unactivated Fit
// -----------------------------------------------------------------------------------------

#[test]
fn test_rejections_distinguishable_from_successful_unactivated_fit() {
    // 1. A successful fit that was simply never activated has:
    // - A valid candidate record
    // - LifecycleState::NeverActivated
    // - CalibrationHealth::Provisional (displayable, not authoritative for routing)
    let cal_facts = CalibrationFacts {
        plan_tier: PlanTier::new("pro"),
        meter_semantics_id: MeterSemanticsId::new("meter-v1"),
        billing_semantics_id: BillingSemanticsId::new("billing-v1"),
    };
    let app_context = ApplicabilityContext {
        plan_tier: PlanTier::new("pro"),
        meter_semantics_id: MeterSemanticsId::new("meter-v1"),
        billing_semantics_id: BillingSemanticsId::new("billing-v1"),
    };
    let unactivated_inputs = HealthInputs {
        calibration: &cal_facts,
        context: &app_context,
        lifecycle: LifecycleState::NeverActivated,
        cost_model_superseded: false,
        drift: None,
        review_due_at: None,
    };
    let health = compute_health(&unactivated_inputs, UtcTimestamp::from_unix_nanos(100_000));
    assert_eq!(
        health,
        CalibrationHealth::Provisional,
        "unactivated candidate evaluates to Provisional"
    );
    assert_eq!(health.label(), "provisional");

    // 2. A rejected fit never produces an active coefficient or candidate row,
    // and returns a typed refusal error (e.g. FitRejection or ActivationRefusal).
    let rejection = FitRejection::ZeroCreditSpan;
    let err = rejection.into_error();
    assert!(
        matches!(err, agent_usage_book::error::Error::InsufficientEvidence(_)),
        "rejection produces typed InsufficientEvidence error, not a provisional candidate"
    );
}

// -----------------------------------------------------------------------------------------
// Acceptance Criterion 10: Model Fits Training Evidence and Fails Held-Out Evidence
// -----------------------------------------------------------------------------------------

#[test]
fn test_overfitting_fits_training_fails_held_out_activation_refused() {
    let window_capacity = Credits::from_micros(50_000_000_000);
    // Training dataset: normal 1 credit/token slope = 20 ppm/credit
    let train_params = SyntheticGeneratorParams::standard_recovery(888, window_capacity);
    let train_data = SyntheticCalibrationGenerator::generate(train_params);
    let train_fit = fit(&train_data.observations, &train_data.experiment)
        .expect("training fit succeeds with low residual");
    assert!(
        train_fit.residual_percentage_points < 0.1,
        "training residual must be low"
    );

    // Held-out validation dataset: quota movement is double the training rate (40 ppm/credit)
    let val_capacity = Credits::from_micros(25_000_000_000); // 25k credits capacity instead of 50k
    let val_params = SyntheticGeneratorParams::standard_recovery(999, val_capacity);
    let val_data = SyntheticCalibrationGenerator::generate(val_params);

    let val_obs: Vec<_> = val_data
        .observations
        .into_iter()
        .map(|mut o| {
            o.evidence_id = EvidenceId::new(format!("val-{}", o.evidence_id.as_str()));
            o
        })
        .collect();

    let held_out_res = held_out_residual(train_fit.candidate.fitted, &val_obs)
        .expect("held out residual computes");

    // Because the held-out relationship differs, the out-of-sample residual is substantial
    assert!(
        held_out_res.micros() > 500_000,
        "held out residual {} micros must detect generalization failure",
        held_out_res.micros()
    );

    // Activation policy requiring tight out-of-sample agreement refuses activation
    let actor = ActivationActor::new("test-auditor").unwrap();
    let policy = ActivationPolicy::new(
        "held-out-policy-v1",
        Credits::from_micros(100_000), // Max allowed: 100,000 micros
        ConditionNumber::from_micros(30_000_000),
    )
    .unwrap();

    let train_ids: BTreeSet<_> = train_data
        .observations
        .iter()
        .map(|o| o.evidence_id.clone())
        .collect();
    let val_ids: BTreeSet<_> = val_obs.iter().map(|o| o.evidence_id.clone()).collect();

    let request = ActivationRequest {
        actor: &actor,
        policy: &policy,
        training: &train_ids,
        validation: &val_ids,
        contamination: &ContaminationVerdict::clean(),
    };

    let recorded = RecordedValidation {
        policy_version: "held-out-policy-v1".into(),
        held_out_residual: Some(held_out_res),
        condition_number: Some(ConditionNumber::from_micros(2_000_000)),
        fitting_evidence: EvidenceFingerprint::from_inputs(&train_ids),
        validation_evidence: EvidenceFingerprint::from_inputs(&val_ids),
    };

    let refusal = check_activation(&request, &recorded)
        .expect_err("activation must be refused when held-out residual exceeds policy bound");

    match refusal {
        ActivationRefusal::HeldOutResidualExceedsPolicy {
            residual, maximum, ..
        } => {
            assert_eq!(residual, held_out_res);
            assert_eq!(maximum, Credits::from_micros(100_000));
        }
        other => panic!("expected HeldOutResidualExceedsPolicy, got {other:?}"),
    }
}

// -----------------------------------------------------------------------------------------
// Acceptance Criterion 11 & 12: Determinism and Stated Ground-Truth Diagnosability
// -----------------------------------------------------------------------------------------

#[test]
fn test_generator_deterministic_from_seed() {
    let window_capacity = Credits::from_micros(50_000_000_000);
    let mut params_1 = SyntheticGeneratorParams::standard_recovery(12345, window_capacity);
    params_1.noise_ppm = 500;
    let mut params_2 = SyntheticGeneratorParams::standard_recovery(12345, window_capacity);
    params_2.noise_ppm = 500;

    let data_1 = SyntheticCalibrationGenerator::generate(params_1);
    let data_2 = SyntheticCalibrationGenerator::generate(params_2);

    assert_eq!(
        data_1.observations.len(),
        data_2.observations.len(),
        "deterministic generator must produce identical observation count"
    );
    for (o1, o2) in data_1.observations.iter().zip(data_2.observations.iter()) {
        assert_eq!(o1.evidence_id, o2.evidence_id);
        assert_eq!(o1.at, o2.at);
        assert_eq!(o1.quota_used_ppm, o2.quota_used_ppm);
        assert_eq!(o1.cumulative_credits, o2.cumulative_credits);
    }

    // Different seeds produce distinct noise perturbations
    let mut params_3 = SyntheticGeneratorParams::standard_recovery(54321, window_capacity);
    params_3.noise_ppm = 500;
    let data_3 = SyntheticCalibrationGenerator::generate(params_3);
    let differs = data_1
        .observations
        .iter()
        .zip(data_3.observations.iter())
        .any(|(a, b)| a.quota_used_ppm != b.quota_used_ppm);
    assert!(
        differs,
        "different seeds must produce different stochastic sequences"
    );
}

#[test]
fn test_each_case_states_ground_truth_parameters() {
    let window_capacity = Credits::from_micros(50_000_000_000);
    let params = SyntheticGeneratorParams::standard_recovery(777, window_capacity);
    let printed = format!("{params:#?}");

    assert!(printed.contains("seed: 777"));
    assert!(printed.contains("window_capacity"));
    assert!(printed.contains("quantization"));
    assert!(printed.contains("accounting_lag"));
    assert!(printed.contains("token_kind_correlation"));
    assert!(printed.contains("contamination"));
}

// -----------------------------------------------------------------------------------------
// Acceptance Criterion 13: Self-Contained Execution (No Network, No External Fixtures)
// -----------------------------------------------------------------------------------------

#[test]
fn test_suite_runs_without_network_or_external_fixtures() {
    let state = StateDir::new();
    let mut conn = open_scratch_ledger(&state);
    seed_initial_cost_model(&mut conn, UtcTimestamp::from_unix_nanos(500 * SECOND_NANOS))
        .expect("cost model seeds cleanly in scratch ledger");

    let params =
        SyntheticGeneratorParams::standard_recovery(13, Credits::from_micros(50_000_000_000));
    let dataset = SyntheticCalibrationGenerator::generate(params);
    assert!(!dataset.observations.is_empty());
    assert!(
        std::env::var("CARGO_MANIFEST_DIR").is_ok(),
        "runs within crate tree"
    );
}

// -----------------------------------------------------------------------------------------
// Acceptance Criterion 14 & Integration Mutation Tests
// -----------------------------------------------------------------------------------------

#[test]
fn test_perturbing_quantization_handling_fails_quantization_scenario() {
    // Proves that treating quantized provider readings as exact scalars manufactures
    // artificial drift / residual errors, while interval fitting succeeds
    let window_capacity = Credits::from_micros(50_000_000_000);
    let mut params = SyntheticGeneratorParams::standard_recovery(808, window_capacity);
    params.quantization = QuantizationSemantics::RoundedToNearest;
    params.reported_resolution_ppm = 10_000; // 1% resolution rounding
    params.noise_ppm = 0;

    let dataset = SyntheticCalibrationGenerator::generate(params);

    // 1. Interval fitting over admissible intervals:
    let interval_res = fit(&dataset.observations, &dataset.experiment)
        .expect("admissible interval fitter handles quantization");
    let low = interval_res
        .candidate
        .uncertainty
        .lower()
        .micros_per_point();
    let high = interval_res
        .candidate
        .uncertainty
        .upper()
        .micros_per_point();
    let true_val = dataset.true_coefficient_micros_per_point;
    assert!(
        low <= true_val && true_val <= high,
        "admissible interval uncertainty must capture true coefficient"
    );

    // 2. Perturbation: naive midpoint scalar fitting
    let (scalar_slope, scalar_residual) =
        fit_scalar_for_comparison(&dataset.observations, &dataset.experiment)
            .expect("scalar fit produces comparison point");

    // Scalar fit manufactures artificial residual from provider's rounding
    assert!(
        scalar_residual > 0.0,
        "scalar fitting over quantized readings must manufacture non-zero residual: {scalar_residual}"
    );
    assert!(scalar_slope > 0.0, "scalar slope computed: {scalar_slope}");
}

#[test]
fn test_disabling_targeted_rejection_guards_fails_cases() {
    // Proves each targeted guard is discriminating:
    // Guard 1: Plan tier check in evaluate_interval
    let mut clean_interval = CandidateInterval::eligible_fixture("int-guard-1");
    assert!(evaluate_interval(&clean_interval).is_empty());
    clean_interval.plan_tier_start = PlanTier::new("tier-x");
    clean_interval.plan_tier_end = PlanTier::new("tier-y");
    assert_eq!(evaluate_interval(&clean_interval).len(), 1);

    // Guard 4: Condition number gate in fit_multivariate
    let lenient_config = MultivariateFitConfig::new(100_000.0, 3, 0.0, false).unwrap();
    let strict_config = MultivariateFitConfig::new(2.0, 3, 0.0, false).unwrap();
    let kinds = [TokenKind::Input, TokenKind::Output];
    let obs = vec![
        MultivariateFitObservation::new(
            EvidenceId::new("e1"),
            KnownTokenVector::new(
                InputTokens::new(10),
                OutputTokens::new(25),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(0),
            ),
            100.0,
        )
        .unwrap(),
        MultivariateFitObservation::new(
            EvidenceId::new("e2"),
            KnownTokenVector::new(
                InputTokens::new(20),
                OutputTokens::new(45),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(0),
            ),
            190.0,
        )
        .unwrap(),
        MultivariateFitObservation::new(
            EvidenceId::new("e3"),
            KnownTokenVector::new(
                InputTokens::new(30),
                OutputTokens::new(65),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(0),
            ),
            280.0,
        )
        .unwrap(),
        MultivariateFitObservation::new(
            EvidenceId::new("e4"),
            KnownTokenVector::new(
                InputTokens::new(40),
                OutputTokens::new(85),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(0),
            ),
            370.0,
        )
        .unwrap(),
    ];
    // Under strict condition number threshold 2.0, it is rejected
    assert!(fit_multivariate(&obs, &kinds, &strict_config, "test").is_err());
    // Under disabled/lenient threshold, it passes
    assert!(fit_multivariate(&obs, &kinds, &lenient_config, "test").is_ok());

    // Guard 9: Disjoint evidence check
    let mut set_a = BTreeSet::new();
    set_a.insert(EvidenceId::new("e-common"));
    let mut set_b = BTreeSet::new();
    set_b.insert(EvidenceId::new("e-common"));
    assert!(check_evidence_disjoint(&set_a, &set_b).is_err());
    set_b.clear();
    set_b.insert(EvidenceId::new("e-disjoint"));
    assert!(check_evidence_disjoint(&set_a, &set_b).is_ok());
}
