//! Disjoint held-out validation before activation (`aub-c0b.8`, PLAN.md 24).
//!
//! A fit does not become validated by having its residuals computed against the
//! observations it was fitted to: that measures how well the fitter interpolates
//! and says nothing about whether the coefficient is right. So ordinary
//! activation requires disjoint evidence identifier sets, a held-out residual
//! diagnostic computed against the validation evidence and recorded on the
//! result, and an explicit activation action recorded append-only.
//!
//! The gate is [`check_activation`]. It judges an [`ActivationRequest`] (who
//! activates, under which [`ActivationPolicy`], over which training and
//! validation evidence sets, with which contamination verdict) against the
//! [`RecordedValidation`] the result row carries (the policy version its
//! diagnostics were recorded under, the held-out residual, the condition
//! number, and the two evidence fingerprints). Every refusal is a typed
//! [`ActivationRefusal`] naming its reason; there is no silent path.
//!
//! The checks run in a fixed order, because more than one can hold at once and
//! the most basic fact should win: the activation must judge the evidence the
//! result was validated against (policy version, then evidence binding), that
//! evidence must be independent (disjointness), the candidate must be sound
//! (contamination, then conditioning), and only then does the residual bound
//! mean anything.
//!
//! A single controlled fit may be published deliberately as provisional (a
//! result row with no lifecycle event). It never becomes current on its own:
//! health reads never-activated as provisional, and the only transition is an
//! explicit [`check_activation`]-passing activation.
//!
//! This module holds no configuration of its own: the policy travels with the
//! request, and the version it names is what the lifecycle event records. A
//! default policy would be a source constant with opinions about every future
//! experiment, which is exactly what the threshold-as-configuration rule
//! forbids.
//!
//! May not depend on:
//! - transcripts (the calibration layer never parses transcripts)
//! - presentation
//! - provider adapters

use std::collections::BTreeSet;
use std::fmt;

use crate::calibration::contamination::{
    ContaminationVerdict, require_uncontaminated_for_activation,
};
use crate::calibration::fitter::FitObservation;
use crate::domain::credits::{Credits, CreditsPerPercentagePoint};
use crate::domain::provenance::EvidenceId;
use crate::error::Error;
use crate::store::calibration::{ConditionNumber, EvidenceFingerprint};

/// A configuration error: an empty activation policy version or an empty actor
/// name. Both are recorded on the lifecycle event, so neither may be empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationConfigError {
    EmptyPolicyVersion,
    EmptyActor,
}

impl fmt::Display for ActivationConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPolicyVersion => {
                formatter.write_str("activation policy version cannot be empty")
            }
            Self::EmptyActor => formatter.write_str("activation actor cannot be empty"),
        }
    }
}

impl std::error::Error for ActivationConfigError {}

/// The activation policy an explicit activation is judged under: a version
/// plus the two numeric bounds. The version is recorded on the lifecycle
/// event; the bounds are the caller's configuration for this activation, not
/// source constants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationPolicy {
    version: String,
    max_out_of_sample_residual: Credits,
    max_condition_number: ConditionNumber,
}

impl ActivationPolicy {
    pub fn new(
        version: impl Into<String>,
        max_out_of_sample_residual: Credits,
        max_condition_number: ConditionNumber,
    ) -> Result<Self, ActivationConfigError> {
        let version = version.into();
        if version.is_empty() {
            return Err(ActivationConfigError::EmptyPolicyVersion);
        }
        Ok(Self {
            version,
            max_out_of_sample_residual,
            max_condition_number,
        })
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn max_out_of_sample_residual(&self) -> Credits {
        self.max_out_of_sample_residual
    }

    pub fn max_condition_number(&self) -> ConditionNumber {
        self.max_condition_number
    }
}

/// Who performs the activation, recorded on the lifecycle event. A person, a
/// named automation, or the fixture that seeds test scaffolding: whatever it
/// is, it is written down rather than left blank.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ActivationActor(String);

impl ActivationActor {
    pub fn new(value: impl Into<String>) -> Result<Self, ActivationConfigError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ActivationConfigError::EmptyActor);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Which of the two recorded evidence digests a presented set failed to
/// reproduce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceRole {
    Fitting,
    Validation,
}

impl EvidenceRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::Fitting => "fitting",
            Self::Validation => "validation",
        }
    }
}

/// A typed activation refusal. Every variant names its reason, because a
/// refused activation that does not say why is a mystery the operator cannot
/// act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationRefusal {
    /// The activation names a policy version different from the one the
    /// result's validation diagnostics were recorded under.
    PolicyVersionMismatch {
        result_version: String,
        activation_version: String,
    },
    /// A presented evidence set does not reproduce the digest the result
    /// recorded for that role: the activation would judge a substitute set.
    EvidenceMismatch { role: EvidenceRole },
    /// The training and validation identifier sets intersect.
    OverlappingEvidence { overlap: Vec<String> },
    /// A contamination finding stands against the candidate.
    Contaminated { mark: String },
    /// The recorded condition number exceeds the activation bound.
    IllConditioned {
        condition_number: ConditionNumber,
        threshold: ConditionNumber,
    },
    /// The result carries no held-out residual to judge.
    MissingHeldOutResidual,
    /// The recorded held-out residual exceeds the activation bound.
    HeldOutResidualExceedsPolicy {
        residual: Credits,
        maximum: Credits,
        policy_version: String,
    },
    /// The referenced cost model covers no term for a token class the
    /// calibration workload carries. This is the cache-write completeness
    /// rule (PLAN.md 23.8): no window calibration becomes active unless its
    /// referenced cost model covers every token class present in its
    /// workload. It names the cost model and the missing classes, never the
    /// calibration, so a legacy record fails it like any other would.
    IncompleteCostModel {
        cost_model_id: String,
        missing: Vec<String>,
    },
}

impl fmt::Display for ActivationRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PolicyVersionMismatch {
                result_version,
                activation_version,
            } => write!(
                formatter,
                "activation refused: policy version '{activation_version}' does not match \
                 the policy version '{result_version}' the result's validation was recorded \
                 under; re-record validation under the activation policy before activating"
            ),
            Self::EvidenceMismatch { role } => write!(
                formatter,
                "activation refused: the presented {} evidence set does not reproduce the \
                 result's recorded {} evidence digest; activation judges the recorded \
                 evidence, not a substitute set",
                role.label(),
                role.label()
            ),
            Self::OverlappingEvidence { overlap } => write!(
                formatter,
                "activation refused: training and validation evidence overlap on {}: {}; \
                 ordinary activation requires disjoint evidence identifier sets",
                overlap.len(),
                overlap.join(", ")
            ),
            Self::Contaminated { mark } => write!(
                formatter,
                "activation refused: candidate is contaminated ({mark}); a contaminated \
                 candidate cannot be activated regardless of its residuals"
            ),
            Self::IllConditioned {
                condition_number,
                threshold,
            } => write!(
                formatter,
                "activation refused: candidate is ill-conditioned (condition number {} \
                 exceeds the activation threshold {}); an ill-conditioned candidate \
                 cannot be activated regardless of its residuals",
                condition_number.micros(),
                threshold.micros()
            ),
            Self::MissingHeldOutResidual => formatter.write_str(
                "activation refused: no held-out residual is recorded on the result; \
                 activation requires a held-out diagnostic computed against the \
                 validation evidence",
            ),
            Self::HeldOutResidualExceedsPolicy {
                residual,
                maximum,
                policy_version,
            } => write!(
                formatter,
                "activation refused: held-out residual {} micros exceeds the activation \
                 policy maximum {} micros (policy {policy_version})",
                residual.micros(),
                maximum.micros()
            ),
            Self::IncompleteCostModel {
                cost_model_id,
                missing,
            } => write!(
                formatter,
                "activation refused: cost model '{cost_model_id}' is incomplete: \
                 missing coverage for token class(es): {}; \
                 no window calibration becomes active unless its referenced cost model \
                 covers every token class present in its workload",
                missing.join(", ")
            ),
        }
    }
}

impl std::error::Error for ActivationRefusal {}

impl ActivationRefusal {
    /// The one derivation into the crate error: an activation refusal is an
    /// explicit policy outcome not met, never a store failure.
    pub fn into_error(self) -> Error {
        Error::ThresholdNotMet(format!("{self}"))
    }
}

/// Everything an explicit activation presents: who activates, under which
/// policy, over which training and validation evidence sets, and with which
/// contamination verdict standing against the candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationRequest<'a> {
    pub actor: &'a ActivationActor,
    pub policy: &'a ActivationPolicy,
    pub training: &'a BTreeSet<EvidenceId>,
    pub validation: &'a BTreeSet<EvidenceId>,
    pub contamination: &'a ContaminationVerdict,
}

/// What the result row says about its own validation: the policy version its
/// diagnostics were recorded under, the held-out residual, the condition
/// number, and the two evidence fingerprints the presented sets must
/// reproduce. The store builds this from the row; tests build it by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedValidation {
    pub policy_version: String,
    pub held_out_residual: Option<Credits>,
    pub condition_number: Option<ConditionNumber>,
    pub fitting_evidence: EvidenceFingerprint,
    pub validation_evidence: EvidenceFingerprint,
}

/// Refuses when the training and validation identifier sets intersect, naming
/// the overlap in sorted order. The check is on identifiers rather than on
/// time ranges, because two experiments can share observations without
/// sharing dates.
pub fn check_evidence_disjoint(
    training: &BTreeSet<EvidenceId>,
    validation: &BTreeSet<EvidenceId>,
) -> Result<(), ActivationRefusal> {
    let overlap: Vec<String> = training
        .intersection(validation)
        .map(|id| id.as_str().to_string())
        .collect();
    if overlap.is_empty() {
        Ok(())
    } else {
        Err(ActivationRefusal::OverlappingEvidence { overlap })
    }
}

/// The cache-write completeness rule (PLAN.md 23.8): no window calibration
/// becomes active unless its referenced cost model covers every token class
/// present in its workload.
///
/// The caller names the missing classes; an empty slice passes. A non-empty
/// slice refuses with [`ActivationRefusal::IncompleteCostModel`], naming the
/// cost model and the missing classes rather than the calibration, so a
/// legacy record fails this rule like any other would.
pub fn check_cost_model_completeness(
    cost_model_id: &str,
    missing: &[String],
) -> Result<(), ActivationRefusal> {
    if missing.is_empty() {
        Ok(())
    } else {
        Err(ActivationRefusal::IncompleteCostModel {
            cost_model_id: cost_model_id.to_string(),
            missing: missing.to_vec(),
        })
    }
}

/// The activation gate: refuses unless the request judges exactly the
/// evidence the result was validated against, that evidence is independent,
/// the candidate is sound, and the recorded held-out residual is within the
/// activation bound.
pub fn check_activation(
    request: &ActivationRequest<'_>,
    recorded: &RecordedValidation,
) -> Result<(), ActivationRefusal> {
    if request.policy.version() != recorded.policy_version {
        return Err(ActivationRefusal::PolicyVersionMismatch {
            result_version: recorded.policy_version.clone(),
            activation_version: request.policy.version().to_string(),
        });
    }
    if EvidenceFingerprint::from_inputs(request.training) != recorded.fitting_evidence {
        return Err(ActivationRefusal::EvidenceMismatch {
            role: EvidenceRole::Fitting,
        });
    }
    if EvidenceFingerprint::from_inputs(request.validation) != recorded.validation_evidence {
        return Err(ActivationRefusal::EvidenceMismatch {
            role: EvidenceRole::Validation,
        });
    }
    check_evidence_disjoint(request.training, request.validation)?;
    require_uncontaminated_for_activation(request.contamination).map_err(|refusal| {
        ActivationRefusal::Contaminated {
            mark: refusal.to_string(),
        }
    })?;
    if let Some(condition_number) = recorded.condition_number {
        let threshold = request.policy.max_condition_number();
        if condition_number > threshold {
            return Err(ActivationRefusal::IllConditioned {
                condition_number,
                threshold,
            });
        }
    }
    match recorded.held_out_residual {
        None => Err(ActivationRefusal::MissingHeldOutResidual),
        Some(residual) => {
            let maximum = request.policy.max_out_of_sample_residual();
            if residual > maximum {
                return Err(ActivationRefusal::HeldOutResidualExceedsPolicy {
                    residual,
                    maximum,
                    policy_version: request.policy.version().to_string(),
                });
            }
            Ok(())
        }
    }
}

/// Computes the held-out residual diagnostic: the mean admissible-interval
/// residual of the validation observations under the fitted coefficient, in
/// the same units and by the same arithmetic the fitter reports its own
/// residual in, so the two are comparable.
///
/// The validation series is expressed in delta coordinates against its own
/// baseline, and the prediction is pure proportional (`slope * x` with zero
/// intercept): the baseline absorbs any level offset from the series sitting
/// elsewhere in the window, so the diagnostic asks only whether the slope
/// predicts held-out movement. A coefficient fitted to different training
/// evidence shows as residual growing with distance from the baseline.
///
/// At least two observations are required: a single point in delta
/// coordinates is the baseline itself, with residual zero under every slope,
/// so it cannot validate anything.
pub fn held_out_residual(
    fitted: CreditsPerPercentagePoint,
    validation: &[FitObservation],
) -> Result<Credits, Error> {
    if validation.len() < 2 {
        return Err(Error::InsufficientEvidence(format!(
            "held-out validation needs at least 2 observations, found {}",
            validation.len()
        )));
    }
    let mut sorted = validation.to_vec();
    sorted.sort_by_key(|o| (o.at, o.evidence_id.as_str().to_string()));

    let min_credits = sorted
        .iter()
        .map(|o| o.cumulative_credits.micros())
        .min()
        .unwrap_or(0);
    let micros_per_point = fitted.micros_per_point().max(1);
    let slope_ppm_per_credit = 1_000_000.0 / micros_per_point as f64;
    let credits_to_units = |micros: i64| (micros - min_credits) as f64 / 1_000_000.0;
    let x_vals: Vec<f64> = sorted
        .iter()
        .map(|o| credits_to_units(o.cumulative_credits.micros()))
        .collect();

    let intervals: Vec<_> = sorted.iter().map(|o| o.interval()).collect();
    let base_reading_ppm = (intervals[0].lower_ppm() + intervals[0].upper_ppm()) as f64 / 2.0;
    let residuals_ppm: Vec<f64> = intervals
        .iter()
        .zip(x_vals.iter())
        .map(|(interval, &x)| {
            let lower = interval.lower_ppm() as f64 - base_reading_ppm;
            let upper = interval.upper_ppm() as f64 - base_reading_ppm;
            let predicted = slope_ppm_per_credit * x;
            if predicted < lower {
                lower - predicted
            } else if predicted > upper {
                predicted - upper
            } else {
                0.0
            }
        })
        .collect();
    let mean_residual_ppm = residuals_ppm.iter().sum::<f64>() / sorted.len() as f64;
    let held_out_micros = (mean_residual_ppm * micros_per_point as f64).round() as i64;
    Ok(Credits::from_micros(held_out_micros.max(0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calibration::contamination::{
        ContaminationInputs, ContaminationMeterPoint, ContaminationThresholds,
    };
    use crate::calibration::fitter::fit;
    use crate::calibration::health::{
        ApplicabilityContext, CalibrationFacts, HealthInputs, LifecycleState, compute_health,
    };
    use crate::calibration::settlement::SettlementPolicy;
    use crate::domain::ids::{BillingSemanticsId, MeterSemanticsId};
    use crate::domain::provenance::{CostModelId, WindowCalibrationId};
    use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
    use crate::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
    use crate::domain::window::{QuantizationSemantics, ReportedResolution, WindowSemanticKey};
    use crate::store::calibration::{
        CalibrationExperiment, CalibrationScope, CoefficientUncertainty, EvidenceDigest,
        ExperimentId, LagHandling, PlanTier, WindowCalibration, WindowCalibrationFields,
        activation_events_for, insert_experiment, insert_result, load_active_at,
        publish_provisional,
    };
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::store::cost_model::{ProviderKey, ValidityInterval};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-calibration-activation-test-{}-{suffix}",
                std::process::id()
            ));
            std::fs::create_dir(&path).expect("scratch dir must be creatable");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture_conn() -> (ScratchDir, rusqlite::Connection) {
        let scratch = ScratchDir::new();
        let policy = PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1000),
        };
        let mut conn = open(
            &scratch.path().join("activation.db"),
            AccessMode::ReadWrite,
            &policy,
        )
        .unwrap();
        crate::store::migrate::run_migrations(
            &mut conn,
            &crate::store::migrations::registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .unwrap();
        (scratch, conn)
    }

    fn ts(nanos: i64) -> UtcTimestamp {
        UtcTimestamp::from_unix_nanos(nanos)
    }

    fn evidence(ids: &[&str]) -> BTreeSet<EvidenceId> {
        ids.iter().map(|s| EvidenceId::new(*s)).collect()
    }

    fn policy(version: &str, max_residual_micros: i64) -> ActivationPolicy {
        ActivationPolicy::new(
            version,
            Credits::from_micros(max_residual_micros),
            ConditionNumber::from_micros(30_000_000),
        )
        .unwrap()
    }

    fn recorded(
        policy_version: &str,
        training: &BTreeSet<EvidenceId>,
        validation: &BTreeSet<EvidenceId>,
        residual: Option<Credits>,
        condition: Option<ConditionNumber>,
    ) -> RecordedValidation {
        RecordedValidation {
            policy_version: policy_version.to_string(),
            held_out_residual: residual,
            condition_number: condition,
            fitting_evidence: EvidenceFingerprint::from_inputs(training),
            validation_evidence: EvidenceFingerprint::from_inputs(validation),
        }
    }

    fn request<'a>(
        actor: &'a ActivationActor,
        policy: &'a ActivationPolicy,
        training: &'a BTreeSet<EvidenceId>,
        validation: &'a BTreeSet<EvidenceId>,
        verdict: &'a ContaminationVerdict,
    ) -> ActivationRequest<'a> {
        ActivationRequest {
            actor,
            policy,
            training,
            validation,
            contamination: verdict,
        }
    }

    fn passing_parts() -> (ActivationActor, ActivationPolicy, ContaminationVerdict) {
        (
            ActivationActor::new("test").unwrap(),
            policy("ap-v1", 100_000),
            ContaminationVerdict::clean(),
        )
    }

    fn interval(from: i64, until: i64) -> ValidityInterval {
        ValidityInterval::new(ts(from), ts(until)).unwrap()
    }

    fn experiment(id: &str) -> CalibrationExperiment {
        CalibrationExperiment {
            id: ExperimentId::new(id),
            provider: ProviderKey::new("anthropic"),
            plan_tier: PlanTier::new("max"),
            window_semantic_key: WindowSemanticKey::new("account"),
            meter_semantics_id: MeterSemanticsId::new("meter-v1"),
            billing_semantics_id: BillingSemanticsId::new("billing-v1"),
            settlement_policy: SettlementPolicy::conservative_default(
                ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap(),
            ),
            validity: interval(100, 300),
            knowledge_time: ts(90),
        }
    }

    fn scope() -> CalibrationScope {
        CalibrationScope {
            provider: ProviderKey::new("anthropic"),
            plan_tier: PlanTier::new("max"),
            window_semantic_key: WindowSemanticKey::new("account"),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn result(
        id: &str,
        training: &BTreeSet<EvidenceId>,
        validation: &BTreeSet<EvidenceId>,
        residual: Option<Credits>,
        condition: Option<ConditionNumber>,
        policy_version: &str,
    ) -> WindowCalibration {
        let union: BTreeSet<EvidenceId> = training.union(validation).cloned().collect();
        WindowCalibration::from_fields(WindowCalibrationFields {
            id: WindowCalibrationId::new(id),
            provider: ProviderKey::new("anthropic"),
            plan_tier: PlanTier::new("max"),
            window_semantic_key: WindowSemanticKey::new("account"),
            meter_semantics_id: MeterSemanticsId::new("meter-v1"),
            billing_semantics_id: BillingSemanticsId::new("billing-v1"),
            cost_model_id: CostModelId::new("cm-1"),
            fitted: CreditsPerPercentagePoint::from_micros_per_point(500_000),
            equivalent_full_window_capacity: Credits::from_micros(500_000_000_000),
            fit_residual: Credits::from_micros(100),
            uncertainty: CoefficientUncertainty::new(
                CreditsPerPercentagePoint::from_micros_per_point(490_000),
                CreditsPerPercentagePoint::from_micros_per_point(510_000),
            )
            .unwrap(),
            lag_estimate: None,
            lag_handling: LagHandling::new("none"),
            sample_count: 10,
            fit_timestamp: ts(100),
            inputs: EvidenceDigest::from_inputs(&union),
            fitting_evidence: EvidenceFingerprint::from_inputs(training),
            validation_evidence: EvidenceFingerprint::from_inputs(validation),
            validation_method: "held-out-disjoint".to_string(),
            validation_version: "v1".to_string(),
            out_of_sample_residual: residual,
            statistical_method: "theil-sen-huber-interval".to_string(),
            statistical_parameters: "{}".to_string(),
            condition_number: condition,
            observation_coverage_requirement: "test".to_string(),
            settling_policy: "test".to_string(),
            excluded_samples: Vec::new(),
            activation_policy_version: policy_version.to_string(),
            aub_version: "0.1.0".to_string(),
            source_revision: "test".to_string(),
            validity: interval(100, 300),
            knowledge_time: ts(100),
        })
        .unwrap()
    }

    /// Overlapping training and validation sets are refused with the overlap
    /// named; the near-identical disjoint pair passes.
    #[test]
    fn overlapping_evidence_is_refused_naming_the_overlap() {
        let training = evidence(&["a", "b"]);
        let validation = evidence(&["b", "c"]);
        let refusal = check_evidence_disjoint(&training, &validation).unwrap_err();
        match &refusal {
            ActivationRefusal::OverlappingEvidence { overlap } => {
                assert_eq!(overlap, &vec!["b".to_string()]);
            }
            other @ (ActivationRefusal::PolicyVersionMismatch { .. }
            | ActivationRefusal::EvidenceMismatch { .. }
            | ActivationRefusal::Contaminated { .. }
            | ActivationRefusal::IllConditioned { .. }
            | ActivationRefusal::MissingHeldOutResidual
            | ActivationRefusal::HeldOutResidualExceedsPolicy { .. }
            | ActivationRefusal::IncompleteCostModel { .. }) => {
                panic!("wrong refusal: {other}")
            }
        }
        assert!(refusal.to_string().contains('b'));

        let disjoint = evidence(&["c", "d"]);
        assert!(check_evidence_disjoint(&training, &disjoint).is_ok());
    }

    /// An activation under a different policy version than the result's
    /// recorded one is refused, even when every other check would pass.
    #[test]
    fn policy_version_mismatch_refuses() {
        let training = evidence(&["t-1", "t-2"]);
        let validation = evidence(&["v-1"]);
        let (actor, _, verdict) = passing_parts();
        let other_policy = policy("ap-v2", 100_000);
        let rec = recorded(
            "ap-v1",
            &training,
            &validation,
            Some(Credits::from_micros(0)),
            None,
        );
        let refusal = check_activation(
            &request(&actor, &other_policy, &training, &validation, &verdict),
            &rec,
        )
        .unwrap_err();
        match &refusal {
            ActivationRefusal::PolicyVersionMismatch {
                result_version,
                activation_version,
            } => {
                assert_eq!(result_version, "ap-v1");
                assert_eq!(activation_version, "ap-v2");
            }
            other @ (ActivationRefusal::EvidenceMismatch { .. }
            | ActivationRefusal::OverlappingEvidence { .. }
            | ActivationRefusal::Contaminated { .. }
            | ActivationRefusal::IllConditioned { .. }
            | ActivationRefusal::MissingHeldOutResidual
            | ActivationRefusal::HeldOutResidualExceedsPolicy { .. }
            | ActivationRefusal::IncompleteCostModel { .. }) => {
                panic!("wrong refusal: {other}")
            }
        }
    }

    /// Presented sets that do not reproduce the recorded digests are refused
    /// on both roles: the gate judges the recorded evidence, not substitutes.
    #[test]
    fn substitute_evidence_sets_are_refused() {
        let training = evidence(&["t-1", "t-2"]);
        let validation = evidence(&["v-1"]);
        let (actor, test_policy, verdict) = passing_parts();
        let rec = recorded(
            "ap-v1",
            &training,
            &validation,
            Some(Credits::from_micros(0)),
            None,
        );
        let wrong_training = evidence(&["t-1", "t-X"]);
        let refusal = check_activation(
            &request(&actor, &test_policy, &wrong_training, &validation, &verdict),
            &rec,
        )
        .unwrap_err();
        assert!(
            matches!(
                refusal,
                ActivationRefusal::EvidenceMismatch {
                    role: EvidenceRole::Fitting
                }
            ),
            "wrong refusal: {refusal}"
        );
        let wrong_validation = evidence(&["v-X"]);
        let refusal = check_activation(
            &request(&actor, &test_policy, &training, &wrong_validation, &verdict),
            &rec,
        )
        .unwrap_err();
        assert!(
            matches!(
                refusal,
                ActivationRefusal::EvidenceMismatch {
                    role: EvidenceRole::Validation
                }
            ),
            "wrong refusal: {refusal}"
        );
    }

    /// A held-out residual within the policy bound passes the whole gate.
    #[test]
    fn held_out_residual_within_policy_passes() {
        let training = evidence(&["t-1", "t-2"]);
        let validation = evidence(&["v-1"]);
        let (actor, test_policy, verdict) = passing_parts();
        let rec = recorded(
            "ap-v1",
            &training,
            &validation,
            Some(Credits::from_micros(7_000)),
            Some(ConditionNumber::from_micros(3_500_000)),
        );
        assert!(
            check_activation(
                &request(&actor, &test_policy, &training, &validation, &verdict),
                &rec
            )
            .is_ok()
        );
    }

    /// A held-out residual above the policy maximum refuses with that reason,
    /// naming the residual, the bound and the policy version.
    #[test]
    fn held_out_residual_exceeding_policy_refuses() {
        let training = evidence(&["t-1", "t-2"]);
        let validation = evidence(&["v-1"]);
        let (actor, _, verdict) = passing_parts();
        let strict = policy("ap-v1", 1_000);
        let rec = recorded(
            "ap-v1",
            &training,
            &validation,
            Some(Credits::from_micros(7_000)),
            None,
        );
        let refusal = check_activation(
            &request(&actor, &strict, &training, &validation, &verdict),
            &rec,
        )
        .unwrap_err();
        match &refusal {
            ActivationRefusal::HeldOutResidualExceedsPolicy {
                residual,
                maximum,
                policy_version,
            } => {
                assert_eq!(residual.micros(), 7_000);
                assert_eq!(maximum.micros(), 1_000);
                assert_eq!(policy_version, "ap-v1");
            }
            other @ (ActivationRefusal::PolicyVersionMismatch { .. }
            | ActivationRefusal::EvidenceMismatch { .. }
            | ActivationRefusal::OverlappingEvidence { .. }
            | ActivationRefusal::Contaminated { .. }
            | ActivationRefusal::IllConditioned { .. }
            | ActivationRefusal::MissingHeldOutResidual
            | ActivationRefusal::IncompleteCostModel { .. }) => {
                panic!("wrong refusal: {other}")
            }
        }
        let message = refusal.to_string();
        assert!(message.contains("7000"));
        assert!(message.contains("1000"));
        assert!(message.contains("ap-v1"));
    }

    /// A result with no recorded held-out residual cannot be activated: there
    /// is nothing to judge against the policy.
    #[test]
    fn missing_held_out_residual_refuses() {
        let training = evidence(&["t-1", "t-2"]);
        let validation = evidence(&["v-1"]);
        let (actor, test_policy, verdict) = passing_parts();
        let rec = recorded("ap-v1", &training, &validation, None, None);
        let refusal = check_activation(
            &request(&actor, &test_policy, &training, &validation, &verdict),
            &rec,
        )
        .unwrap_err();
        assert!(
            matches!(refusal, ActivationRefusal::MissingHeldOutResidual),
            "wrong refusal: {refusal}"
        );
    }

    fn meter_point(at_nanos: i64, ppm: i32) -> ContaminationMeterPoint {
        ContaminationMeterPoint::new(
            ts(at_nanos),
            QuotaUsed::new(QuotaFractionPpm::new(ppm).unwrap()),
        )
    }

    fn contaminated_verdict() -> ContaminationVerdict {
        let pre = vec![meter_point(0, 100_000), meter_point(500, 150_000)];
        let post: Vec<ContaminationMeterPoint> = Vec::new();
        let markers: Vec<crate::calibration::contamination::ContaminationMarkerPoint> = Vec::new();
        let inputs = ContaminationInputs {
            experiment_account: "work-a",
            baseline_plateau_started_at: ts(0),
            started_at: ts(1_000),
            ended_at: Some(ts(2_000)),
            evaluated_at: ts(3_000),
            pre_burn_series: &pre,
            post_series: &post,
            controlled_meter_start: QuotaUsed::new(QuotaFractionPpm::new(100_000).unwrap()),
            controlled_meter_end: QuotaUsed::new(QuotaFractionPpm::new(100_000).unwrap()),
            local_credits_delta: Credits::from_micros(5_000_000),
            markers: &markers,
        };
        crate::calibration::contamination::evaluate_contamination(
            &inputs,
            &ContaminationThresholds::conservative_default(),
        )
    }

    /// A contaminated candidate cannot be activated even with a zero residual
    /// and a generous bound: soundness comes before the numbers.
    #[test]
    fn contaminated_candidate_refused_regardless_of_residuals() {
        let verdict = contaminated_verdict();
        assert!(verdict.is_contaminated());
        let training = evidence(&["t-1", "t-2"]);
        let validation = evidence(&["v-1"]);
        let (actor, test_policy, _) = passing_parts();
        let rec = recorded(
            "ap-v1",
            &training,
            &validation,
            Some(Credits::from_micros(0)),
            None,
        );
        let refusal = check_activation(
            &request(&actor, &test_policy, &training, &validation, &verdict),
            &rec,
        )
        .unwrap_err();
        match &refusal {
            ActivationRefusal::Contaminated { mark } => {
                assert!(mark.contains("pre_burn_idle_movement"), "mark: {mark}");
            }
            other @ (ActivationRefusal::PolicyVersionMismatch { .. }
            | ActivationRefusal::EvidenceMismatch { .. }
            | ActivationRefusal::OverlappingEvidence { .. }
            | ActivationRefusal::IllConditioned { .. }
            | ActivationRefusal::MissingHeldOutResidual
            | ActivationRefusal::HeldOutResidualExceedsPolicy { .. }
            | ActivationRefusal::IncompleteCostModel { .. }) => {
                panic!("wrong refusal: {other}")
            }
        }
    }

    /// An ill-conditioned candidate cannot be activated even with a zero
    /// residual: the coefficients cannot be trusted whatever they predict.
    #[test]
    fn ill_conditioned_candidate_refused_regardless_of_residuals() {
        let training = evidence(&["t-1", "t-2"]);
        let validation = evidence(&["v-1"]);
        let (actor, test_policy, verdict) = passing_parts();
        let rec = recorded(
            "ap-v1",
            &training,
            &validation,
            Some(Credits::from_micros(0)),
            Some(ConditionNumber::from_micros(100_000_000)),
        );
        let refusal = check_activation(
            &request(&actor, &test_policy, &training, &validation, &verdict),
            &rec,
        )
        .unwrap_err();
        match &refusal {
            ActivationRefusal::IllConditioned {
                condition_number,
                threshold,
            } => {
                assert_eq!(condition_number.micros(), 100_000_000);
                assert_eq!(threshold.micros(), 30_000_000);
            }
            other @ (ActivationRefusal::PolicyVersionMismatch { .. }
            | ActivationRefusal::EvidenceMismatch { .. }
            | ActivationRefusal::OverlappingEvidence { .. }
            | ActivationRefusal::Contaminated { .. }
            | ActivationRefusal::MissingHeldOutResidual
            | ActivationRefusal::HeldOutResidualExceedsPolicy { .. }
            | ActivationRefusal::IncompleteCostModel { .. }) => {
                panic!("wrong refusal: {other}")
            }
        }
    }

    fn fit_observation(
        id: &str,
        at_nanos: i64,
        quota_ppm: i64,
        cumulative_micros: i64,
    ) -> FitObservation {
        FitObservation::new(
            EvidenceId::new(id),
            ts(at_nanos),
            quota_ppm,
            10,
            QuantizationSemantics::Exact,
            Credits::from_micros(cumulative_micros),
        )
    }

    /// A validation series following the fitted slope validates cleanly: the
    /// diagnostic is zero when the coefficient predicts held-out movement.
    #[test]
    fn held_out_residual_of_a_matching_series_is_zero() {
        let fitted = CreditsPerPercentagePoint::from_micros_per_point(500_000);
        let validation: Vec<FitObservation> = (0..6)
            .map(|i| {
                fit_observation(
                    &format!("match-{i}"),
                    1_000_000_000 + i * 60_000_000_000,
                    100_000 + 2 * i,
                    i * 1_000_000,
                )
            })
            .collect();
        assert_eq!(held_out_residual(fitted, &validation).unwrap().micros(), 0);
    }

    /// A validation series on a four-times steeper slope reports a large
    /// diagnostic: 15 ppm mean at 500,000 micros per point is 7,500,000
    /// micros, which pins the scale the integration case below relies on.
    #[test]
    fn held_out_residual_of_a_divergent_series_is_large() {
        let fitted = CreditsPerPercentagePoint::from_micros_per_point(500_000);
        let validation: Vec<FitObservation> = (0..6)
            .map(|i| {
                fit_observation(
                    &format!("diverge-{i}"),
                    1_000_000_000 + i * 60_000_000_000,
                    100_000 + 8 * i,
                    i * 1_000_000,
                )
            })
            .collect();
        assert_eq!(
            held_out_residual(fitted, &validation).unwrap().micros(),
            7_500_000
        );
    }

    /// Fewer than two held-out observations cannot validate anything: one
    /// point is its own baseline with zero residual under every slope.
    #[test]
    fn held_out_residual_needs_two_observations() {
        let fitted = CreditsPerPercentagePoint::from_micros_per_point(500_000);
        assert!(held_out_residual(fitted, &[]).is_err());
        assert!(
            held_out_residual(
                fitted,
                &[fit_observation("solo", 1_000_000_000, 100_000, 0)]
            )
            .is_err()
        );
    }

    /// The scenario this bead exists for: a candidate that fits its training
    /// evidence well (zero fit residual on a clean slope) but fails disjoint
    /// held-out evidence is refused activation.
    #[test]
    fn fits_training_well_but_fails_held_out_is_refused_activation() {
        let (_scratch, mut conn) = fixture_conn();
        insert_experiment(&conn, &experiment("exp-heldout")).unwrap();

        let training: Vec<FitObservation> = (0..10)
            .map(|i| {
                fit_observation(
                    &format!("train-{i}"),
                    1_000_000_000 + i * 60_000_000_000,
                    100_000 + 2 * i,
                    i * 1_000_000,
                )
            })
            .collect();
        let fit_result = fit(&training, &experiment("exp-heldout")).unwrap();
        assert_eq!(
            fit_result.candidate.fitted.micros_per_point(),
            500_000,
            "the training slope of 2 ppm per credit must fit exactly"
        );
        assert_eq!(
            fit_result.candidate.fit_residual.micros(),
            0,
            "the candidate fits its training evidence perfectly"
        );

        let validation: Vec<FitObservation> = (0..6)
            .map(|i| {
                fit_observation(
                    &format!("valid-{i}"),
                    20_000_000_000 + i * 60_000_000_000,
                    100_000 + 8 * i,
                    i * 1_000_000,
                )
            })
            .collect();
        let held_out = held_out_residual(fit_result.candidate.fitted, &validation).unwrap();
        assert_eq!(held_out.micros(), 7_500_000);

        let training_ids: BTreeSet<EvidenceId> =
            training.iter().map(|o| o.evidence_id.clone()).collect();
        let validation_ids: BTreeSet<EvidenceId> =
            validation.iter().map(|o| o.evidence_id.clone()).collect();
        let calibration = result(
            "wc-heldout",
            &training_ids,
            &validation_ids,
            Some(held_out),
            None,
            "test-policy-v1",
        );
        insert_result(&mut conn, &calibration, &[ExperimentId::new("exp-heldout")]).unwrap();

        let actor = ActivationActor::new("test").unwrap();
        let strict = policy("test-policy-v1", 100_000);
        let verdict = ContaminationVerdict::clean();
        let refusal = crate::store::calibration::activate(
            &mut conn,
            calibration.id(),
            ts(500),
            None,
            &request(&actor, &strict, &training_ids, &validation_ids, &verdict),
        )
        .unwrap_err();
        match &refusal {
            Error::ThresholdNotMet(message) => {
                assert!(message.contains("7500000"), "message: {message}");
                assert!(message.contains("100000"), "message: {message}");
            }
            other @ (Error::Internal(_)
            | Error::Usage(_)
            | Error::AuthRequired(_)
            | Error::RemoteUnavailable(_)
            | Error::Store(_)
            | Error::InsufficientEvidence(_)
            | Error::IngestIncomplete(_)) => {
                panic!("wrong error class: {other:?}")
            }
        }
        assert!(
            load_active_at(&conn, &scope(), ts(600)).unwrap().is_none(),
            "a refused activation must leave nothing active"
        );
    }

    /// A single controlled fit publishes as provisional, stays provisional
    /// through a refused (overlapping) activation, and becomes current only
    /// through an explicit activation backed by disjoint validation.
    #[test]
    fn provisional_never_becomes_current_without_disjoint_backed_activation() {
        let (_scratch, mut conn) = fixture_conn();
        insert_experiment(&conn, &experiment("exp-prov")).unwrap();
        let training = evidence(&["p-fit-1", "p-fit-2"]);
        let validation = evidence(&["p-val-1"]);
        let calibration = result(
            "wc-prov",
            &training,
            &validation,
            Some(Credits::from_micros(100)),
            None,
            "test-policy-v1",
        );
        publish_provisional(&mut conn, &calibration, &[ExperimentId::new("exp-prov")]).unwrap();
        assert!(
            load_active_at(&conn, &scope(), ts(600)).unwrap().is_none(),
            "publishing must not activate"
        );

        let facts = CalibrationFacts {
            plan_tier: PlanTier::new("max"),
            meter_semantics_id: MeterSemanticsId::new("meter-v1"),
            billing_semantics_id: BillingSemanticsId::new("billing-v1"),
        };
        let context = ApplicabilityContext {
            plan_tier: PlanTier::new("max"),
            meter_semantics_id: MeterSemanticsId::new("meter-v1"),
            billing_semantics_id: BillingSemanticsId::new("billing-v1"),
        };
        let provisional_inputs = HealthInputs {
            calibration: &facts,
            context: &context,
            lifecycle: LifecycleState::NeverActivated,
            cost_model_superseded: false,
            drift: None,
            review_due_at: None,
        };
        assert_eq!(
            compute_health(&provisional_inputs, ts(600)),
            crate::calibration::health::CalibrationHealth::Provisional
        );

        let (actor, _, verdict) = passing_parts();
        let strict = policy("test-policy-v1", 100_000);
        let overlapping = evidence(&["p-fit-1", "p-other"]);
        assert!(
            crate::store::calibration::activate(
                &mut conn,
                calibration.id(),
                ts(500),
                None,
                &request(&actor, &strict, &training, &overlapping, &verdict),
            )
            .is_err()
        );
        assert!(
            load_active_at(&conn, &scope(), ts(600)).unwrap().is_none(),
            "a refused overlapping activation must not promote the provisional result"
        );

        crate::store::calibration::activate(
            &mut conn,
            calibration.id(),
            ts(500),
            None,
            &request(&actor, &strict, &training, &validation, &verdict),
        )
        .unwrap();
        let active = load_active_at(&conn, &scope(), ts(600)).unwrap().unwrap();
        assert_eq!(active.id(), calibration.id());
        let current_inputs = HealthInputs {
            lifecycle: LifecycleState::Active,
            ..provisional_inputs
        };
        assert_eq!(
            compute_health(&current_inputs, ts(600)),
            crate::calibration::health::CalibrationHealth::Current
        );
    }

    /// The lifecycle event records who activated, when, under which policy
    /// version, and over which evidence; a supersession records its
    /// predecessor the same way.
    #[test]
    fn activation_event_carries_actor_timestamp_policy_and_evidence() {
        let (_scratch, mut conn) = fixture_conn();
        insert_experiment(&conn, &experiment("exp-evt")).unwrap();
        let training = evidence(&["e-fit-1", "e-fit-2"]);
        let validation = evidence(&["e-val-1"]);
        let first = result(
            "wc-evt-1",
            &training,
            &validation,
            Some(Credits::from_micros(100)),
            None,
            "test-policy-v1",
        );
        let second = result(
            "wc-evt-2",
            &training,
            &validation,
            Some(Credits::from_micros(200)),
            None,
            "test-policy-v1",
        );
        insert_result(&mut conn, &first, &[ExperimentId::new("exp-evt")]).unwrap();
        insert_result(&mut conn, &second, &[ExperimentId::new("exp-evt")]).unwrap();

        let actor = ActivationActor::new("operator").unwrap();
        let test_policy = policy("test-policy-v1", 100_000);
        let verdict = ContaminationVerdict::clean();
        crate::store::calibration::activate(
            &mut conn,
            first.id(),
            ts(500),
            None,
            &request(&actor, &test_policy, &training, &validation, &verdict),
        )
        .unwrap();
        crate::store::calibration::activate(
            &mut conn,
            second.id(),
            ts(700),
            Some(first.id()),
            &request(&actor, &test_policy, &training, &validation, &verdict),
        )
        .unwrap();

        let first_events = activation_events_for(&conn, first.id()).unwrap();
        assert_eq!(first_events.len(), 1);
        let event = &first_events[0];
        assert_eq!(
            event.kind,
            crate::store::calibration::CalibrationEventKind::Activation
        );
        assert_eq!(event.event_at, ts(500));
        assert_eq!(event.actor.as_str(), "operator");
        assert_eq!(event.activation_policy_version, "test-policy-v1");
        assert_eq!(
            event.fitting_evidence,
            EvidenceFingerprint::from_inputs(&training)
        );
        assert_eq!(
            event.validation_evidence,
            EvidenceFingerprint::from_inputs(&validation)
        );
        assert_eq!(event.supersedes, None);

        let second_events = activation_events_for(&conn, second.id()).unwrap();
        assert_eq!(second_events.len(), 1);
        let supersession = &second_events[0];
        assert_eq!(
            supersession.kind,
            crate::store::calibration::CalibrationEventKind::Supersession
        );
        assert_eq!(supersession.event_at, ts(700));
        assert_eq!(supersession.actor.as_str(), "operator");
        assert_eq!(supersession.supersedes, Some(first.id().clone()));
    }

    /// An empty policy version or actor name is refused at construction: both
    /// are recorded on the event, so neither may be blank.
    #[test]
    fn empty_policy_version_or_actor_is_refused() {
        assert!(
            ActivationPolicy::new("", Credits::from_micros(1), ConditionNumber::from_micros(1))
                .is_err()
        );
        assert!(ActivationActor::new("").is_err());
        assert!(
            ActivationPolicy::new(
                "ap-v1",
                Credits::from_micros(1),
                ConditionNumber::from_micros(1)
            )
            .is_ok()
        );
        assert!(ActivationActor::new("operator").is_ok());
    }
}
