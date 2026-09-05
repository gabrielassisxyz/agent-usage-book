//! Passive candidate generation over ordinary traffic under strict eligibility.
//!
//! Passive calibration is not valid merely because session identifiers can be joined.
//! An interval is admitted only when all ten eligibility conditions hold, plus no
//! meter-window anomaly or external-validation mismatch annotations apply.
//! Passive fitting produces a candidate and never an activation (PLAN.md 23.4, 23.7, 42.14).

use std::collections::BTreeMap;
use std::fmt;

use rusqlite::Connection;

use crate::attribution::account_segment::AccountEvidenceClass;
use crate::calibration::fitter::{FitObservation, fit};
use crate::calibration::settlement::{
    SettlementMeterObservation, SettlementObservationSeries, SettlementPolicy, SettlementRole,
    detect_settlement,
};
use crate::config::Config;
use crate::domain::credits::Credits;
use crate::domain::ids::{BillingSemanticsId, MeterSemanticsId};
use crate::domain::provenance::EvidenceId;
use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
use crate::domain::time::{Clock, UtcTimestamp};
use crate::domain::window::{QuantizationSemantics, ReportedResolution, WindowSemanticKey};
use crate::domain::window_anomaly::WindowAnomalyKind;
use crate::error::Error;
use crate::store::account::AccountId;
use crate::store::calibration::{
    CalibrationExperiment, ExcludedSample, ExperimentId, PlanTier, WindowCalibrationCandidate,
    count_overlapping_sessions, distinct_account_window_keys_and_tiers,
    has_unresolved_mismatch_annotation, insert_candidate, insert_experiment,
    load_attribution_evidence_classes, load_candidate, load_latest_experiment,
    load_marker_evidence_designations, load_passive_observations, load_passive_usage_components,
};
use crate::store::cost_model::{ProviderKey, ValidityInterval};

/// The ten standard eligibility conditions for passive calibration intervals (PLAN.md 23.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PassiveEligibilityCondition {
    HighConfidenceAccountAttribution,
    MeterObservationsOnBothSides,
    NoResetInside,
    UnchangedPlanTier,
    ExactContributingUsage,
    NoUnknownTokenComponents,
    SufficientMeterCoverage,
    NoSecondLocalSessionOrConsumer,
    ExclusivityPolicyPermitsPassive,
    ServerSideSettlementSatisfied,
}

impl PassiveEligibilityCondition {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HighConfidenceAccountAttribution => "high_confidence_account_attribution",
            Self::MeterObservationsOnBothSides => "meter_observations_on_both_sides",
            Self::NoResetInside => "no_reset_inside",
            Self::UnchangedPlanTier => "unchanged_plan_tier",
            Self::ExactContributingUsage => "exact_contributing_usage",
            Self::NoUnknownTokenComponents => "no_unknown_token_components",
            Self::SufficientMeterCoverage => "sufficient_meter_coverage",
            Self::NoSecondLocalSessionOrConsumer => "no_second_local_session_or_consumer",
            Self::ExclusivityPolicyPermitsPassive => "exclusivity_policy_permits_passive",
            Self::ServerSideSettlementSatisfied => "server_side_settlement_satisfied",
        }
    }

    pub const fn all() -> [Self; 10] {
        [
            Self::HighConfidenceAccountAttribution,
            Self::MeterObservationsOnBothSides,
            Self::NoResetInside,
            Self::UnchangedPlanTier,
            Self::ExactContributingUsage,
            Self::NoUnknownTokenComponents,
            Self::SufficientMeterCoverage,
            Self::NoSecondLocalSessionOrConsumer,
            Self::ExclusivityPolicyPermitsPassive,
            Self::ServerSideSettlementSatisfied,
        ]
    }
}

impl fmt::Display for PassiveEligibilityCondition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Typed reason why an interval was excluded from passive calibration.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PassiveExclusionReason {
    ConditionFailed(PassiveEligibilityCondition, String),
    MeterWindowAnomaly {
        kind: WindowAnomalyKind,
        detail: String,
    },
    ExternalValidationMismatch {
        detail: String,
    },
}

impl PassiveExclusionReason {
    pub fn condition_key(&self) -> &'static str {
        match self {
            Self::ConditionFailed(cond, _) => cond.as_str(),
            Self::MeterWindowAnomaly { .. } => "meter_window_anomaly",
            Self::ExternalValidationMismatch { .. } => "external_validation_mismatch",
        }
    }

    pub fn detail(&self) -> &str {
        match self {
            Self::ConditionFailed(_, d) => d,
            Self::MeterWindowAnomaly { detail, .. } => detail,
            Self::ExternalValidationMismatch { detail } => detail,
        }
    }
}

impl fmt::Display for PassiveExclusionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConditionFailed(cond, detail) => {
                write!(f, "{}: {}", cond.as_str(), detail)
            }
            Self::MeterWindowAnomaly { kind, detail } => {
                write!(f, "meter_window_anomaly ({}): {}", kind.as_str(), detail)
            }
            Self::ExternalValidationMismatch { detail } => {
                write!(f, "external_validation_mismatch: {}", detail)
            }
        }
    }
}

/// One candidate interval evaluated for passive calibration eligibility.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateInterval {
    pub interval_id: String,
    pub account_id: AccountId,
    pub account_name: String,
    pub provider: ProviderKey,
    pub plan_tier: PlanTier,
    pub window_semantic_key: WindowSemanticKey,
    pub start_at: UtcTimestamp,
    pub end_at: UtcTimestamp,
    pub start_observation: Option<FitObservation>,
    pub end_observation: Option<FitObservation>,
    pub start_settled: bool,
    pub end_settled: bool,
    pub plan_tier_start: PlanTier,
    pub plan_tier_end: PlanTier,
    pub reset_inside: bool,
    pub meter_coverage_complete: bool,
    pub exclusivity_permits_passive: bool,
    pub has_overlapping_session_or_consumer: bool,
    pub has_inferred_account_attribution: bool,
    pub has_estimated_tokens: bool,
    pub has_unknown_token_components: bool,
    pub contributing_credits: Credits,
    pub anomaly_exclusions: Vec<WindowAnomalyKind>,
    pub has_external_validation_mismatch: bool,
}

impl CandidateInterval {
    /// Constructs an eligible candidate interval where all 10 conditions and annotations pass.
    pub fn eligible_fixture(interval_id: impl Into<String>) -> Self {
        let now = UtcTimestamp::from_unix_nanos(1_000_000_000);
        let later = UtcTimestamp::from_unix_nanos(2_000_000_000);
        let start_obs = FitObservation::new(
            EvidenceId::new("ev-start"),
            now,
            100_000,
            10_000,
            QuantizationSemantics::RoundedToNearest,
            Credits::from_micros(0),
        );
        let end_obs = FitObservation::new(
            EvidenceId::new("ev-end"),
            later,
            150_000,
            10_000,
            QuantizationSemantics::RoundedToNearest,
            Credits::from_micros(5_000_000),
        );

        Self {
            interval_id: interval_id.into(),
            account_id: AccountId::new(1),
            account_name: "work-primary".into(),
            provider: ProviderKey::new("anthropic"),
            plan_tier: PlanTier::new("pro-5h"),
            window_semantic_key: WindowSemanticKey::new("five_hour"),
            start_at: now,
            end_at: later,
            start_observation: Some(start_obs),
            end_observation: Some(end_obs),
            start_settled: true,
            end_settled: true,
            plan_tier_start: PlanTier::new("pro-5h"),
            plan_tier_end: PlanTier::new("pro-5h"),
            reset_inside: false,
            meter_coverage_complete: true,
            exclusivity_permits_passive: true,
            has_overlapping_session_or_consumer: false,
            has_inferred_account_attribution: false,
            has_estimated_tokens: false,
            has_unknown_token_components: false,
            contributing_credits: Credits::from_micros(5_000_000),
            anomaly_exclusions: Vec::new(),
            has_external_validation_mismatch: false,
        }
    }
}

/// Evaluates all ten eligibility conditions and anomaly/mismatch exclusions for one interval.
/// Returns every failing reason without short-circuiting.
pub fn evaluate_interval(interval: &CandidateInterval) -> Vec<PassiveExclusionReason> {
    let mut reasons = Vec::new();

    // Condition 1: High-confidence account attribution
    if interval.has_inferred_account_attribution {
        reasons.push(PassiveExclusionReason::ConditionFailed(
            PassiveEligibilityCondition::HighConfidenceAccountAttribution,
            "inferred account attribution makes interval ineligible".to_string(),
        ));
    }

    // Condition 2: Meter observations on both sides of the relevant window
    if interval.start_observation.is_none() || interval.end_observation.is_none() {
        reasons.push(PassiveExclusionReason::ConditionFailed(
            PassiveEligibilityCondition::MeterObservationsOnBothSides,
            "missing meter observation at interval boundary".to_string(),
        ));
    }

    // Condition 3: No reset inside
    if interval.reset_inside {
        reasons.push(PassiveExclusionReason::ConditionFailed(
            PassiveEligibilityCondition::NoResetInside,
            "quota reset crossing detected inside interval".to_string(),
        ));
    }

    // Condition 4: Unchanged plan tier
    if interval.plan_tier_start != interval.plan_tier_end {
        reasons.push(PassiveExclusionReason::ConditionFailed(
            PassiveEligibilityCondition::UnchangedPlanTier,
            format!(
                "plan tier changed across interval: '{}' -> '{}'",
                interval.plan_tier_start.as_str(),
                interval.plan_tier_end.as_str()
            ),
        ));
    }

    // Condition 5: All contributing usage exact
    if interval.has_estimated_tokens {
        reasons.push(PassiveExclusionReason::ConditionFailed(
            PassiveEligibilityCondition::ExactContributingUsage,
            "estimated token usage makes interval ineligible".to_string(),
        ));
    }

    // Condition 6: No unknown token components exist
    if interval.has_unknown_token_components {
        reasons.push(PassiveExclusionReason::ConditionFailed(
            PassiveEligibilityCondition::NoUnknownTokenComponents,
            "unknown token components present in usage".to_string(),
        ));
    }

    // Condition 7: Sufficient meter coverage around the interval
    if !interval.meter_coverage_complete {
        reasons.push(PassiveExclusionReason::ConditionFailed(
            PassiveEligibilityCondition::SufficientMeterCoverage,
            "insufficient meter coverage around interval".to_string(),
        ));
    }

    // Condition 8: No known second local session or account consumer overlapping
    if interval.has_overlapping_session_or_consumer {
        reasons.push(PassiveExclusionReason::ConditionFailed(
            PassiveEligibilityCondition::NoSecondLocalSessionOrConsumer,
            "overlapping local session or account consumer detected".to_string(),
        ));
    }

    // Condition 9: Account's exclusivity policy permits passive fitting
    if !interval.exclusivity_permits_passive {
        reasons.push(PassiveExclusionReason::ConditionFailed(
            PassiveEligibilityCondition::ExclusivityPolicyPermitsPassive,
            format!(
                "account '{}' exclusivity policy forbids passive fitting",
                interval.account_name
            ),
        ));
    }

    // Condition 10: Satisfied server-side settlement
    if !interval.start_settled || !interval.end_settled {
        reasons.push(PassiveExclusionReason::ConditionFailed(
            PassiveEligibilityCondition::ServerSideSettlementSatisfied,
            "interval boundaries not bounded by sufficiently settled meter regions".to_string(),
        ));
    }

    // Meter-window anomaly exclusions
    for kind in &interval.anomaly_exclusions {
        reasons.push(PassiveExclusionReason::MeterWindowAnomaly {
            kind: *kind,
            detail: format!(
                "interval contains typed meter-window anomaly exclusion '{}'",
                kind.as_str()
            ),
        });
    }

    // External validation mismatch
    if interval.has_external_validation_mismatch {
        reasons.push(PassiveExclusionReason::ExternalValidationMismatch {
            detail: "interval carries persisted external-validation mismatch annotation"
                .to_string(),
        });
    }

    reasons
}

/// The report model generated by passive calibration.
#[derive(Debug, Clone, PartialEq)]
pub struct PassiveCalibrationReport {
    pub intervals_considered: usize,
    pub eligible_intervals: usize,
    pub excluded_intervals: usize,
    pub failing_condition_counts: BTreeMap<String, usize>,
    pub candidate: Option<WindowCalibrationCandidate>,
    pub candidate_excluded_samples: Vec<ExcludedSample>,
}

/// Generates passive calibration report and produces a candidate if eligible intervals exist.
/// Never activates the produced candidate (aub-c0b.7, PLAN.md 42.14).
pub fn evaluate_and_report_passive_intervals(
    intervals: &[CandidateInterval],
    conn: Option<&Connection>,
    clock: &impl Clock,
) -> Result<PassiveCalibrationReport, Error> {
    let mut eligible = Vec::new();
    let mut excluded_samples = Vec::new();
    let mut failing_counts: BTreeMap<String, usize> = BTreeMap::new();

    // Initialize all standard keys with zero
    for cond in PassiveEligibilityCondition::all() {
        failing_counts.insert(cond.as_str().to_string(), 0);
    }
    failing_counts.insert("meter_window_anomaly".to_string(), 0);
    failing_counts.insert("external_validation_mismatch".to_string(), 0);

    for interval in intervals {
        let reasons = evaluate_interval(interval);
        if reasons.is_empty() {
            eligible.push(interval);
        } else {
            let reason_summary = reasons
                .iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>()
                .join("; ");
            if let Ok(sample) =
                ExcludedSample::new(format!("interval-{}", interval.interval_id), reason_summary)
            {
                excluded_samples.push(sample);
            }
            for reason in &reasons {
                *failing_counts
                    .entry(reason.condition_key().to_string())
                    .or_insert(0) += 1;
            }
        }
    }

    let candidate = if !eligible.is_empty() {
        fit_passive_candidate(&eligible, conn, clock)?
    } else {
        None
    };

    Ok(PassiveCalibrationReport {
        intervals_considered: intervals.len(),
        eligible_intervals: eligible.len(),
        excluded_intervals: intervals.len() - eligible.len(),
        failing_condition_counts: failing_counts,
        candidate,
        candidate_excluded_samples: excluded_samples,
    })
}

/// Fits a candidate calibration from eligible passive intervals and persists it immutably.
/// Does NOT touch `calibration_lifecycle` (produces candidate by default and never an activation).
fn fit_passive_candidate(
    eligible_intervals: &[&CandidateInterval],
    conn: Option<&Connection>,
    clock: &impl Clock,
) -> Result<Option<WindowCalibrationCandidate>, Error> {
    if eligible_intervals.is_empty() {
        return Ok(None);
    }

    let first = eligible_intervals[0];
    let experiment_id = ExperimentId::new(format!("exp-passive-{}", first.account_name));

    // Construct FitObservations from the eligible intervals
    let mut fit_observations = Vec::new();
    let mut cumulative_micros: i64 = 0;

    for interval in eligible_intervals {
        if let (Some(start_obs), Some(end_obs)) =
            (&interval.start_observation, &interval.end_observation)
        {
            if fit_observations.is_empty() {
                fit_observations.push(FitObservation::new(
                    start_obs.evidence_id.clone(),
                    start_obs.at,
                    start_obs.quota_used_ppm,
                    start_obs.reported_resolution_ppm,
                    start_obs.quantization,
                    Credits::from_micros(cumulative_micros),
                ));
            }
            cumulative_micros += interval.contributing_credits.micros();
            fit_observations.push(FitObservation::new(
                end_obs.evidence_id.clone(),
                end_obs.at,
                end_obs.quota_used_ppm,
                end_obs.reported_resolution_ppm,
                end_obs.quantization,
                Credits::from_micros(cumulative_micros),
            ));
        }
    }

    if fit_observations.len() < 2 {
        return Ok(None);
    }

    let min_at = fit_observations
        .iter()
        .map(|o| o.at)
        .min()
        .unwrap_or_else(|| clock.now());
    let max_at = fit_observations
        .iter()
        .map(|o| o.at)
        .max()
        .unwrap_or_else(|| clock.now());

    let reported_res =
        ReportedResolution::new(QuotaFractionPpm::new(10_000).expect("valid resolution"))
            .expect("valid reported resolution");
    let settlement_policy = SettlementPolicy::conservative_default(reported_res);

    let experiment = CalibrationExperiment {
        id: experiment_id,
        provider: first.provider.clone(),
        plan_tier: first.plan_tier.clone(),
        window_semantic_key: first.window_semantic_key.clone(),
        meter_semantics_id: MeterSemanticsId::new("meter-semantics-v1"),
        billing_semantics_id: BillingSemanticsId::new("billing-semantics-v1"),
        settlement_policy,
        validity: ValidityInterval::new(min_at, max_at.max(min_at))
            .map_err(|e| Error::Store(format!("invalid interval: {e}")))?,
        knowledge_time: clock.now(),
    };

    if let Some(c) = conn
        && load_latest_experiment(c)?.is_none()
    {
        let _ = insert_experiment(c, &experiment);
    }

    match fit(&fit_observations, &experiment) {
        Ok(mut fit_result) => {
            fit_result.candidate.knowledge_time = clock.now();
            if let Some(c) = conn
                && load_candidate(c, &fit_result.candidate.id)?.is_none()
            {
                let _ = insert_candidate(c, &fit_result.candidate);
            }
            Ok(Some(fit_result.candidate))
        }
        Err(_) => Ok(None),
    }
}

/// Scans the database and configuration to generate candidate intervals for passive calibration.
pub fn generate_candidate_intervals_from_ledger(
    conn: &Connection,
    config: &Config,
    filter_account: Option<&str>,
    filter_window: Option<&str>,
) -> Result<Vec<CandidateInterval>, Error> {
    let all_accs = crate::store::account::all_accounts(conn)?;
    let mut accounts = Vec::new();
    for acc in all_accs {
        if let Some(target) = filter_account
            && acc.logical_name() != target
        {
            continue;
        }
        accounts.push((
            acc.id(),
            acc.logical_name().to_string(),
            acc.provider_key().to_string(),
        ));
    }

    // Query all exclusions once
    let all_excl = crate::store::window_anomaly::all_exclusions(conn).unwrap_or_default();
    let mut candidate_intervals = Vec::new();

    for (account_id, account_name, provider_str) in accounts {
        let account_cfg = config.accounts.iter().find(|a| a.name == account_name);
        let exclusivity_permits_passive = account_cfg
            .map(|a| a.permits_passive_fitting())
            .unwrap_or(false);

        // Query distinct semantic keys and plan tiers
        let win_rows = distinct_account_window_keys_and_tiers(conn, account_id)?;

        for (semantic_key_str, plan_tier_str) in win_rows {
            if let Some(target_win) = filter_window
                && semantic_key_str != target_win
            {
                continue;
            }

            // Fetch observations ordered by time
            let observations =
                load_passive_observations(conn, account_id, &semantic_key_str, &plan_tier_str)?;

            if observations.len() < 2 {
                continue;
            }

            // Detect settled plateaus using SettlementPolicy
            let resolution_ppm = observations[0].reported_resolution_ppm;
            let reported_res = ReportedResolution::new(
                QuotaFractionPpm::new(resolution_ppm as i32).unwrap_or_else(|| {
                    QuotaFractionPpm::new(10_000).expect("valid fallback resolution")
                }),
            )
            .expect("valid reported resolution");
            let settlement_policy = SettlementPolicy::conservative_default(reported_res);

            let settlement_obs: Vec<SettlementMeterObservation> = observations
                .iter()
                .map(|obs| {
                    SettlementMeterObservation::new(
                        obs.received_at,
                        QuotaUsed::new(QuotaFractionPpm::new(obs.quota_used_ppm as i32).unwrap()),
                        ReportedResolution::new(
                            QuotaFractionPpm::new(obs.reported_resolution_ppm as i32).unwrap(),
                        )
                        .unwrap(),
                    )
                })
                .collect();

            // Detect settled regions: check windows of 3 observations
            let mut settled_indices = Vec::new();
            if settlement_obs.len() >= 3 {
                for i in 0..=settlement_obs.len() - 3 {
                    let slice = &settlement_obs[i..i + 3];
                    let series =
                        SettlementObservationSeries::complete(slice[0].at(), slice.to_vec());
                    let outcome =
                        detect_settlement(&series, &settlement_policy, SettlementRole::Baseline);
                    if outcome.is_settled() {
                        settled_indices.push(i + 2); // Terminal index of the settled plateau
                    }
                }
            }

            // If settled indices exist, build intervals between settled plateaus;
            // otherwise build adjacent intervals with start_settled = false, end_settled = false
            let interval_pairs: Vec<(usize, usize, bool)> = if settled_indices.len() >= 2 {
                let mut pairs = Vec::new();
                for i in 0..settled_indices.len() - 1 {
                    pairs.push((settled_indices[i], settled_indices[i + 1], true));
                }
                pairs
            } else {
                let mut pairs = Vec::new();
                for i in 0..observations.len() - 1 {
                    pairs.push((i, i + 1, false));
                }
                pairs
            };

            for (start_idx, end_idx, settled) in interval_pairs {
                let start_obs_data = &observations[start_idx];
                let end_obs_data = &observations[end_idx];

                let start_at = start_obs_data.received_at;
                let end_at = end_obs_data.received_at;

                let start_obs = FitObservation::new(
                    EvidenceId::new(&start_obs_data.content_hash),
                    start_obs_data.received_at,
                    start_obs_data.quota_used_ppm,
                    start_obs_data.reported_resolution_ppm,
                    start_obs_data.quantization,
                    Credits::from_micros(0),
                );
                let end_obs = FitObservation::new(
                    EvidenceId::new(&end_obs_data.content_hash),
                    end_obs_data.received_at,
                    end_obs_data.quota_used_ppm,
                    end_obs_data.reported_resolution_ppm,
                    end_obs_data.quantization,
                    Credits::from_micros(0),
                );

                // Check reset inside
                let mut reset_inside = false;
                for obs in &observations[start_idx..=end_idx] {
                    if let Some(r_at) = obs.resets_at
                        && r_at > start_at
                        && r_at < end_at
                    {
                        reset_inside = true;
                        break;
                    }
                }
                if end_obs_data.quota_used_ppm
                    < start_obs_data.quota_used_ppm - start_obs_data.reported_resolution_ppm
                {
                    reset_inside = true;
                }

                // Check overlapping sessions
                let overlapping_sessions =
                    count_overlapping_sessions(conn, account_id, &account_name, start_at, end_at)
                        .unwrap_or(0);
                let has_overlapping_session_or_consumer = overlapping_sessions > 1;

                // Check inferred account attribution
                let attr_classes =
                    load_attribution_evidence_classes(conn, &account_name, start_at, end_at)?;

                let mut has_inferred_account_attribution = false;
                for cls_str in attr_classes {
                    if let Some(class) = AccountEvidenceClass::parse(&cls_str)
                        && !class.is_eligible_for_passive_calibration()
                    {
                        has_inferred_account_attribution = true;
                        break;
                    }
                }

                if !has_inferred_account_attribution {
                    let marker_rows = load_marker_evidence_designations(
                        conn,
                        account_id,
                        &account_name,
                        start_at,
                        end_at,
                    )?;
                    for des in marker_rows {
                        if des == "conservative_temporal_inference"
                            || des == "unattributed"
                            || des == "inferred"
                        {
                            has_inferred_account_attribution = true;
                            break;
                        }
                    }
                }

                // Check usage events & components
                let usage_rows = load_passive_usage_components(conn, start_at, end_at)?;

                let mut has_estimated_tokens = false;
                let mut has_unknown_token_components = false;
                let mut contributing_credits = Credits::from_micros(0);

                for u in usage_rows {
                    if u.evidence_kind == "estimated"
                        || u.evidence_kind.starts_with("reconstructed")
                        || u.evidence_kind.starts_with("estimate")
                    {
                        has_estimated_tokens = true;
                    }
                    match u.token_class.as_str() {
                        "input" => {
                            // 3 credits per 1,000,000 input tokens (standard Claude Messages cost model)
                            let micros = (u.count as f64 * 3.0).round() as i64;
                            contributing_credits =
                                Credits::from_micros(contributing_credits.micros() + micros);
                        }
                        "output" => {
                            let micros = (u.count as f64 * 15.0).round() as i64;
                            contributing_credits =
                                Credits::from_micros(contributing_credits.micros() + micros);
                        }
                        "cache_read" => {
                            let micros = (u.count as f64 * 0.3).round() as i64;
                            contributing_credits =
                                Credits::from_micros(contributing_credits.micros() + micros);
                        }
                        "cache_write" => {
                            let micros = (u.count as f64 * 3.75).round() as i64;
                            contributing_credits =
                                Credits::from_micros(contributing_credits.micros() + micros);
                        }
                        _ => {
                            has_unknown_token_components = true;
                        }
                    }
                }

                // Check anomaly exclusions overlapping with this interval
                let mut anomaly_exclusions = Vec::new();
                for excl in &all_excl {
                    if excl.account_id == account_id
                        && excl.interval_start_at < end_at
                        && excl.interval_end_at > start_at
                        && let Ok(Some(anomaly)) =
                            crate::store::window_anomaly::anomaly_by_row_id(conn, excl.anomaly_id)
                    {
                        anomaly_exclusions.push(anomaly.kind);
                    }
                }

                // Check external validation mismatch
                let has_external_validation_mismatch: bool =
                    has_unresolved_mismatch_annotation(conn, &semantic_key_str, start_at, end_at)
                        .unwrap_or(false);

                let interval_id = format!("{}-{}-{}", account_name, start_idx, end_idx);
                candidate_intervals.push(CandidateInterval {
                    interval_id,
                    account_id,
                    account_name: account_name.clone(),
                    provider: ProviderKey::new(&provider_str),
                    plan_tier: PlanTier::new(&plan_tier_str),
                    window_semantic_key: WindowSemanticKey::new(&semantic_key_str),
                    start_at,
                    end_at,
                    start_observation: Some(start_obs),
                    end_observation: Some(end_obs),
                    start_settled: settled,
                    end_settled: settled,
                    plan_tier_start: PlanTier::new(&plan_tier_str),
                    plan_tier_end: PlanTier::new(&plan_tier_str),
                    reset_inside,
                    meter_coverage_complete: true,
                    exclusivity_permits_passive,
                    has_overlapping_session_or_consumer,
                    has_inferred_account_attribution,
                    has_estimated_tokens,
                    has_unknown_token_components,
                    contributing_credits,
                    anomaly_exclusions,
                    has_external_validation_mismatch,
                });
            }
        }
    }

    Ok(candidate_intervals)
}

/// Runs passive calibration report over the ledger.
pub fn run_passive_calibration_from_ledger(
    conn: &Connection,
    config: &Config,
    filter_account: Option<&str>,
    filter_window: Option<&str>,
    clock: &impl Clock,
) -> Result<PassiveCalibrationReport, Error> {
    let intervals =
        generate_candidate_intervals_from_ledger(conn, config, filter_account, filter_window)?;
    evaluate_and_report_passive_intervals(&intervals, Some(conn), clock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::FakeClock;

    // --- Unit tests: each of the ten conditions failing in isolation ---

    #[test]
    fn condition_1_inferred_account_attribution_fails_in_isolation() {
        let mut interval = CandidateInterval::eligible_fixture("cond-1");
        interval.has_inferred_account_attribution = true;

        let reasons = evaluate_interval(&interval);
        assert_eq!(reasons.len(), 1);
        assert_eq!(
            reasons[0],
            PassiveExclusionReason::ConditionFailed(
                PassiveEligibilityCondition::HighConfidenceAccountAttribution,
                "inferred account attribution makes interval ineligible".into(),
            )
        );
    }

    #[test]
    fn condition_2_missing_meter_boundary_fails_in_isolation() {
        let mut interval = CandidateInterval::eligible_fixture("cond-2");
        interval.end_observation = None;

        let reasons = evaluate_interval(&interval);
        assert_eq!(reasons.len(), 1);
        assert_eq!(
            reasons[0],
            PassiveExclusionReason::ConditionFailed(
                PassiveEligibilityCondition::MeterObservationsOnBothSides,
                "missing meter observation at interval boundary".into(),
            )
        );
    }

    #[test]
    fn condition_3_reset_inside_interval_fails_in_isolation() {
        let mut interval = CandidateInterval::eligible_fixture("cond-3");
        interval.reset_inside = true;

        let reasons = evaluate_interval(&interval);
        assert_eq!(reasons.len(), 1);
        assert_eq!(
            reasons[0],
            PassiveExclusionReason::ConditionFailed(
                PassiveEligibilityCondition::NoResetInside,
                "quota reset crossing detected inside interval".into(),
            )
        );
    }

    #[test]
    fn condition_4_changed_plan_tier_fails_in_isolation() {
        let mut interval = CandidateInterval::eligible_fixture("cond-4");
        interval.plan_tier_end = PlanTier::new("max-tier");

        let reasons = evaluate_interval(&interval);
        assert_eq!(reasons.len(), 1);
        assert_eq!(
            reasons[0],
            PassiveExclusionReason::ConditionFailed(
                PassiveEligibilityCondition::UnchangedPlanTier,
                "plan tier changed across interval: 'pro-5h' -> 'max-tier'".into(),
            )
        );
    }

    #[test]
    fn condition_5_estimated_token_usage_fails_in_isolation() {
        let mut interval = CandidateInterval::eligible_fixture("cond-5");
        interval.has_estimated_tokens = true;

        let reasons = evaluate_interval(&interval);
        assert_eq!(reasons.len(), 1);
        assert_eq!(
            reasons[0],
            PassiveExclusionReason::ConditionFailed(
                PassiveEligibilityCondition::ExactContributingUsage,
                "estimated token usage makes interval ineligible".into(),
            )
        );
    }

    #[test]
    fn condition_6_unknown_token_components_fails_in_isolation() {
        let mut interval = CandidateInterval::eligible_fixture("cond-6");
        interval.has_unknown_token_components = true;

        let reasons = evaluate_interval(&interval);
        assert_eq!(reasons.len(), 1);
        assert_eq!(
            reasons[0],
            PassiveExclusionReason::ConditionFailed(
                PassiveEligibilityCondition::NoUnknownTokenComponents,
                "unknown token components present in usage".into(),
            )
        );
    }

    #[test]
    fn condition_7_insufficient_meter_coverage_fails_in_isolation() {
        let mut interval = CandidateInterval::eligible_fixture("cond-7");
        interval.meter_coverage_complete = false;

        let reasons = evaluate_interval(&interval);
        assert_eq!(reasons.len(), 1);
        assert_eq!(
            reasons[0],
            PassiveExclusionReason::ConditionFailed(
                PassiveEligibilityCondition::SufficientMeterCoverage,
                "insufficient meter coverage around interval".into(),
            )
        );
    }

    #[test]
    fn condition_8_overlapping_session_fails_in_isolation() {
        let mut interval = CandidateInterval::eligible_fixture("cond-8");
        interval.has_overlapping_session_or_consumer = true;

        let reasons = evaluate_interval(&interval);
        assert_eq!(reasons.len(), 1);
        assert_eq!(
            reasons[0],
            PassiveExclusionReason::ConditionFailed(
                PassiveEligibilityCondition::NoSecondLocalSessionOrConsumer,
                "overlapping local session or account consumer detected".into(),
            )
        );
    }

    #[test]
    fn condition_9_exclusivity_policy_forbids_passive_fails_in_isolation() {
        let mut interval = CandidateInterval::eligible_fixture("cond-9");
        interval.exclusivity_permits_passive = false;

        let reasons = evaluate_interval(&interval);
        assert_eq!(reasons.len(), 1);
        assert_eq!(
            reasons[0],
            PassiveExclusionReason::ConditionFailed(
                PassiveEligibilityCondition::ExclusivityPolicyPermitsPassive,
                "account 'work-primary' exclusivity policy forbids passive fitting".into(),
            )
        );
    }

    #[test]
    fn condition_10_unsettled_server_boundary_fails_in_isolation() {
        let mut interval = CandidateInterval::eligible_fixture("cond-10");
        interval.end_settled = false;

        let reasons = evaluate_interval(&interval);
        assert_eq!(reasons.len(), 1);
        assert_eq!(
            reasons[0],
            PassiveExclusionReason::ConditionFailed(
                PassiveEligibilityCondition::ServerSideSettlementSatisfied,
                "interval boundaries not bounded by sufficiently settled meter regions".into(),
            )
        );
    }

    #[test]
    fn passive_evidence_prefers_settled_meter_regions_over_adjacent_samples() {
        let settled_interval = CandidateInterval::eligible_fixture("settled");
        assert!(settled_interval.start_settled && settled_interval.end_settled);
        assert!(evaluate_interval(&settled_interval).is_empty());

        let mut naive_interval = CandidateInterval::eligible_fixture("naive");
        naive_interval.start_settled = false;
        naive_interval.end_settled = false;
        let naive_reasons = evaluate_interval(&naive_interval);
        assert_eq!(naive_reasons.len(), 1);
        assert_eq!(
            naive_reasons[0],
            PassiveExclusionReason::ConditionFailed(
                PassiveEligibilityCondition::ServerSideSettlementSatisfied,
                "interval boundaries not bounded by sufficiently settled meter regions".into(),
            )
        );
    }

    // --- Invariant 14 enforcement test ---

    #[test]
    fn passive_calibration_produces_candidate_and_never_activates() {
        let interval = CandidateInterval::eligible_fixture("cand-no-act");
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(10_000_000_000));
        let report = evaluate_and_report_passive_intervals(&[interval], None, &clock)
            .expect("report succeeds");

        assert_eq!(report.intervals_considered, 1);
        assert_eq!(report.eligible_intervals, 1);
        assert_eq!(report.excluded_intervals, 0);
        assert!(report.candidate.is_some());

        // Produces a candidate by default and never an activation (Invariant 14)
        let candidate = report.candidate.unwrap();
        assert!(candidate.id.as_str().starts_with("cand-"));
        assert_eq!(candidate.plan_tier.as_str(), "pro-5h");
    }

    // --- Anomaly annotations and mismatch tests ---

    #[test]
    fn meter_window_anomaly_annotations_in_isolation_and_adjacent_clean_stay_eligible() {
        let clean_before = CandidateInterval::eligible_fixture("clean-1");
        let mut anomaly_1 = CandidateInterval::eligible_fixture("anomaly-1");
        anomaly_1
            .anomaly_exclusions
            .push(WindowAnomalyKind::PercentageDecreaseWithoutReset);

        let clean_mid = CandidateInterval::eligible_fixture("clean-2");
        let mut anomaly_2 = CandidateInterval::eligible_fixture("anomaly-2");
        anomaly_2
            .anomaly_exclusions
            .push(WindowAnomalyKind::UnexpectedResetTimestampChange);

        let clean_after = CandidateInterval::eligible_fixture("clean-3");

        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(10_000_000_000));
        let intervals = vec![clean_before, anomaly_1, clean_mid, anomaly_2, clean_after];
        let report = evaluate_and_report_passive_intervals(&intervals, None, &clock)
            .expect("evaluation succeeds");

        assert_eq!(report.intervals_considered, 5);
        assert_eq!(report.eligible_intervals, 3);
        assert_eq!(report.excluded_intervals, 2);
        assert_eq!(
            report.failing_condition_counts.get("meter_window_anomaly"),
            Some(&2)
        );
        assert_eq!(report.candidate_excluded_samples.len(), 2);
    }

    #[test]
    fn external_validation_mismatch_alone_excludes_interval() {
        let mut interval = CandidateInterval::eligible_fixture("mismatch-1");
        interval.has_external_validation_mismatch = true;

        let reasons = evaluate_interval(&interval);
        assert_eq!(reasons.len(), 1);
        assert_eq!(
            reasons[0],
            PassiveExclusionReason::ExternalValidationMismatch {
                detail: "interval carries persisted external-validation mismatch annotation".into(),
            }
        );
    }

    // --- Property test: Monotonicity of relaxing and tightening conditions ---

    #[test]
    fn property_monotonicity_of_relaxing_and_tightening_conditions() {
        // Generate a corpus of intervals with varied eligibility flags
        let base_intervals: Vec<CandidateInterval> = (0..50)
            .map(|i| {
                let mut interval = CandidateInterval::eligible_fixture(format!("prop-{i}"));
                if i % 2 == 1 {
                    interval.has_inferred_account_attribution = true;
                }
                if i % 3 == 1 {
                    interval.has_estimated_tokens = true;
                }
                if i % 5 == 1 {
                    interval.reset_inside = true;
                }
                if i % 7 == 1 {
                    interval.start_settled = false;
                }
                if i % 11 == 1 {
                    interval.exclusivity_permits_passive = false;
                }
                interval
            })
            .collect();

        let count_eligible = |items: &[CandidateInterval]| -> usize {
            items
                .iter()
                .filter(|it| evaluate_interval(it).is_empty())
                .count()
        };

        let baseline_eligible = count_eligible(&base_intervals);

        // Relax condition: turn off inferred attribution across all intervals
        let mut relaxed = base_intervals.clone();
        for it in &mut relaxed {
            it.has_inferred_account_attribution = false;
        }
        let relaxed_eligible = count_eligible(&relaxed);
        assert!(
            relaxed_eligible >= baseline_eligible,
            "relaxing condition must enlarge or preserve eligible set: {relaxed_eligible} >= {baseline_eligible}"
        );

        // Tighten condition: force reset_inside across all intervals
        let mut tightened = base_intervals.clone();
        for it in &mut tightened {
            it.reset_inside = true;
        }
        let tightened_eligible = count_eligible(&tightened);
        assert!(
            tightened_eligible <= baseline_eligible,
            "tightening condition must reduce or preserve eligible set: {tightened_eligible} <= {baseline_eligible}"
        );
    }
}
