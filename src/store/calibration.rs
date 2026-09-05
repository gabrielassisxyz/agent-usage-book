//! The calibration tables and their repository (`aub-c0b.1`, PLAN.md 12.14, 23.1, 24).
//!
//! Calibration is a versioned experiment, not a coefficient someone typed once: the
//! duplicate hand fit in a second program is the defect that created this project
//! (`bin/checks/55-coefficient-tombstone` keeps that literal out of source), and the
//! repair is a record complete enough that a corrected fit always has a path in.
//!
//! Every witness here carries two independent times. `valid_from`/`valid_until` say
//! when the witness describes the physical world; `knowledge_time` says when `aub`
//! learned or recorded it. A calibration whose validity started in June but which was
//! imported in August means a report produced in July was right about what `aub` then
//! knew and wrong about the world, and both questions stay answerable because nothing
//! is ever mutated. `results_valid_at` answers by valid time; `load_active_at` answers
//! by knowledge time, following the append-only [`calibration_lifecycle`] chain.
//!
//! A production [`WindowCalibration`] is constructible only inside this crate: its
//! fields are private and both its constructor and its field-bundle are `pub(crate)`,
//! so a consumer resolves a calibration through this repository and never assembles one
//! from primitives (`aub-c0b.13`, invariant 12).
//!
//! May not depend on:
//! - HTTP or terminal-formatting crates
//! - presentation
//! - provider adapters

use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::calibration::settlement::{SettlementCriterion, SettlementPolicy};
use crate::domain::credits::{Credits, CreditsPerPercentagePoint};
use crate::domain::ids::{BillingSemanticsId, MeterSemanticsId};
use crate::domain::provenance::{
    CostModelId, EvidenceId, WindowCalibrationId, canonical_inputs_hash,
};
use crate::domain::quota::QuotaFractionPpm;
use crate::domain::time::{MonotonicDuration, UtcTimestamp};
use crate::domain::window::{ReportedResolution, WindowSemanticKey};
use crate::error::Error;
use crate::store::cost_model::{ProviderKey, ValidityInterval};

/// A `calibration_experiment` row's SQLite rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExperimentDbId(i64);

impl ExperimentDbId {
    pub const fn value(self) -> i64 {
        self.0
    }
}

/// A `window_calibration_candidate` row's SQLite rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CandidateDbId(i64);

impl CandidateDbId {
    pub const fn value(self) -> i64 {
        self.0
    }
}

/// A `window_calibration_result` row's SQLite rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CalibrationResultDbId(i64);

impl CalibrationResultDbId {
    pub const fn value(self) -> i64 {
        self.0
    }
}

/// A `calibration_lifecycle` row's SQLite rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CalibrationLifecycleEventId(i64);

impl CalibrationLifecycleEventId {
    pub const fn value(self) -> i64 {
        self.0
    }
}

/// The semantic identifier of a calibration experiment.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExperimentId(String);

impl ExperimentId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The semantic identifier of a calibration candidate.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CandidateId(String);

impl CandidateId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A plan tier a calibration applies to. Calibration cannot cross incompatible plan
/// tiers (`aub-c0b.11`, invariant 13); this repository stores the tier so that rule
/// has something to check against.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlanTier(String);

impl PlanTier {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Free-text metadata describing how accounting lag was estimated and handled
/// (PLAN.md 23.5). Not enumerated: the design leaves the vocabulary to the fitter.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LagHandling(String);

impl LagHandling {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A condition number, carried as fixed-point micros so a multivariate fit's
/// numerical conditioning survives a round trip through an integer column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ConditionNumber(i64);

impl ConditionNumber {
    pub const fn from_micros(micros: i64) -> Self {
        Self(micros)
    }

    pub const fn micros(self) -> i64 {
        self.0
    }
}

/// The scope an active calibration is unique within: every applicable window has its
/// own calibration (`src/domain/window.rs`), so a lookup filters on all three columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrationScope {
    pub provider: ProviderKey,
    pub plan_tier: PlanTier,
    pub window_semantic_key: WindowSemanticKey,
}

/// The stated uncertainty of a fitted coefficient: an absolute interval over it, the
/// lower bound never above the upper one (enforced here and by the table CHECK).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoefficientUncertainty {
    lower: CreditsPerPercentagePoint,
    upper: CreditsPerPercentagePoint,
}

impl CoefficientUncertainty {
    pub fn new(
        lower: CreditsPerPercentagePoint,
        upper: CreditsPerPercentagePoint,
    ) -> Result<Self, Error> {
        if lower.micros_per_point() > upper.micros_per_point() {
            return Err(Error::Store(
                "coefficient uncertainty lower bound exceeds upper bound".into(),
            ));
        }
        Ok(Self { lower, upper })
    }

    pub fn lower(&self) -> CreditsPerPercentagePoint {
        self.lower
    }

    pub fn upper(&self) -> CreditsPerPercentagePoint {
        self.upper
    }
}

/// A content-addressed statement of the evidence set consumed by a fit, plus its
/// size, so a rerun of the same fitter on the same evidence is recognizable as the
/// same fit and a claimed expansion can be verified rather than merely believed. The
/// exact pair `ProvenanceManifest` derives its own content address from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvidenceDigest {
    digest: u64,
    count: usize,
}

impl EvidenceDigest {
    /// The digest of an evidence set, order-independent by construction.
    pub fn from_inputs(inputs: &BTreeSet<EvidenceId>) -> Self {
        Self {
            digest: canonical_inputs_hash(inputs).as_u64(),
            count: inputs.len(),
        }
    }

    /// Rebuilds a digest from its stored parts.
    pub fn from_parts(digest: u64, count: usize) -> Self {
        Self { digest, count }
    }

    pub fn digest(&self) -> u64 {
        self.digest
    }

    pub fn count(&self) -> usize {
        self.count
    }

    /// True when `inputs` is exactly the set whose canonical digest produced this.
    pub fn verify_expansion(&self, inputs: &BTreeSet<EvidenceId>) -> bool {
        inputs.len() == self.count && canonical_inputs_hash(inputs).as_u64() == self.digest
    }
}

/// An opaque fingerprint of an evidence subset (the fitting slice, the validation
/// slice). Unlike [`EvidenceDigest`] it carries no count and supports no expansion
/// check: the design asks only for a hash here (PLAN.md 12.14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvidenceFingerprint(u64);

impl EvidenceFingerprint {
    pub fn from_inputs(inputs: &BTreeSet<EvidenceId>) -> Self {
        Self(canonical_inputs_hash(inputs).as_u64())
    }

    pub fn from_raw(value: u64) -> Self {
        Self(value)
    }

    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// One excluded sample and the explicit reason it was left out of the fit (PLAN.md
/// 12.14: "explicit reason for excluded samples").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExcludedSample {
    sample_ref: String,
    reason: String,
}

impl ExcludedSample {
    /// Rejects a reference or reason containing the record or field separators the
    /// column encoding uses, so the serialization round-trips unambiguously.
    pub fn new(sample_ref: impl Into<String>, reason: impl Into<String>) -> Result<Self, Error> {
        let sample_ref = sample_ref.into();
        let reason = reason.into();
        for (label, text) in [("reference", &sample_ref), ("reason", &reason)] {
            if text.is_empty() {
                return Err(Error::Store(format!("excluded sample {label} is empty")));
            }
            if text.contains('\n') || text.contains('\u{1f}') {
                return Err(Error::Store(format!(
                    "excluded sample {label} contains a separator character"
                )));
            }
        }
        Ok(Self { sample_ref, reason })
    }

    pub fn sample_ref(&self) -> &str {
        &self.sample_ref
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// The sentinel stored when no sample was excluded, because the column is
/// `CHECK (length(excluded_samples) > 0)` and an empty list still has to write
/// something an unambiguous decode can recognize.
const EXCLUDED_SAMPLES_NONE: &str = "[]";

fn encode_excluded(samples: &[ExcludedSample]) -> String {
    if samples.is_empty() {
        return EXCLUDED_SAMPLES_NONE.to_string();
    }
    samples
        .iter()
        .map(|s| format!("{}\u{1f}{}", s.sample_ref, s.reason))
        .collect::<Vec<_>>()
        .join("\n")
}

fn decode_excluded(text: &str) -> Result<Vec<ExcludedSample>, Error> {
    if text == EXCLUDED_SAMPLES_NONE {
        return Ok(Vec::new());
    }
    text.split('\n')
        .map(|record| match record.split_once('\u{1f}') {
            Some((sample_ref, reason)) => ExcludedSample::new(sample_ref, reason),
            None => Err(Error::Store(format!(
                "malformed excluded-sample record: '{record}'"
            ))),
        })
        .collect()
}

/// A recorded calibration experiment: the observation-gathering exercise a candidate
/// or result is fitted from. An experiment is an input to calibration, not the
/// protected witness, so its constructor is public.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrationExperiment {
    pub id: ExperimentId,
    pub provider: ProviderKey,
    pub plan_tier: PlanTier,
    pub window_semantic_key: WindowSemanticKey,
    pub meter_semantics_id: MeterSemanticsId,
    pub billing_semantics_id: BillingSemanticsId,
    pub settlement_policy: SettlementPolicy,
    pub validity: ValidityInterval,
    pub knowledge_time: UtcTimestamp,
}

/// A proposed fit kept as evidence and candidate generation, not automatic truth
/// (`aub-c0b.7`, invariant 14). Lighter than a result: no validation metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowCalibrationCandidate {
    pub id: CandidateId,
    pub experiment: ExperimentId,
    pub provider: ProviderKey,
    pub plan_tier: PlanTier,
    pub window_semantic_key: WindowSemanticKey,
    pub fitted: CreditsPerPercentagePoint,
    pub equivalent_full_window_capacity: Credits,
    pub fit_residual: Credits,
    pub uncertainty: CoefficientUncertainty,
    pub sample_count: u32,
    pub inputs: EvidenceDigest,
    pub validity: ValidityInterval,
    pub knowledge_time: UtcTimestamp,
}

/// The field bundle a [`WindowCalibration`] is built from.
///
/// `pub(crate)`: external code cannot even name this, so the only way to obtain a
/// `WindowCalibration` outside this crate is to read one back from the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WindowCalibrationFields {
    pub id: WindowCalibrationId,
    pub provider: ProviderKey,
    pub plan_tier: PlanTier,
    pub window_semantic_key: WindowSemanticKey,
    pub meter_semantics_id: MeterSemanticsId,
    pub billing_semantics_id: BillingSemanticsId,
    pub cost_model_id: CostModelId,
    pub fitted: CreditsPerPercentagePoint,
    pub equivalent_full_window_capacity: Credits,
    pub fit_residual: Credits,
    pub uncertainty: CoefficientUncertainty,
    pub lag_estimate: Option<MonotonicDuration>,
    pub lag_handling: LagHandling,
    pub sample_count: u32,
    pub fit_timestamp: UtcTimestamp,
    pub inputs: EvidenceDigest,
    pub fitting_evidence: EvidenceFingerprint,
    pub validation_evidence: EvidenceFingerprint,
    pub validation_method: String,
    pub validation_version: String,
    pub out_of_sample_residual: Option<Credits>,
    pub statistical_method: String,
    pub statistical_parameters: String,
    pub condition_number: Option<ConditionNumber>,
    pub observation_coverage_requirement: String,
    pub settling_policy: String,
    pub excluded_samples: Vec<ExcludedSample>,
    pub activation_policy_version: String,
    pub aub_version: String,
    pub source_revision: String,
    pub validity: ValidityInterval,
    pub knowledge_time: UtcTimestamp,
}

/// A validated window calibration: the credits-per-percentage-point coefficient in
/// force for one scope over one validity interval, with everything the design asks a
/// calibration record to carry so a past number this system printed stays explainable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowCalibration {
    fields: WindowCalibrationFields,
}

impl WindowCalibration {
    /// The one construction path. Kept `pub(crate)` and taking a `pub(crate)` bundle
    /// so a production calibration can only be built inside this crate.
    pub(crate) fn from_fields(fields: WindowCalibrationFields) -> Result<Self, Error> {
        for (label, text) in [
            ("validation method", &fields.validation_method),
            ("validation version", &fields.validation_version),
            ("statistical method", &fields.statistical_method),
            ("statistical parameters", &fields.statistical_parameters),
            (
                "observation coverage requirement",
                &fields.observation_coverage_requirement,
            ),
            ("settling policy", &fields.settling_policy),
            (
                "activation policy version",
                &fields.activation_policy_version,
            ),
            ("aub version", &fields.aub_version),
            ("source revision", &fields.source_revision),
        ] {
            if text.is_empty() {
                return Err(Error::Store(format!(
                    "window calibration '{}' has an empty {label}",
                    fields.id.as_str()
                )));
            }
        }
        Ok(Self { fields })
    }

    pub fn id(&self) -> &WindowCalibrationId {
        &self.fields.id
    }

    pub fn scope(&self) -> CalibrationScope {
        CalibrationScope {
            provider: self.fields.provider.clone(),
            plan_tier: self.fields.plan_tier.clone(),
            window_semantic_key: self.fields.window_semantic_key.clone(),
        }
    }

    pub fn provider(&self) -> &ProviderKey {
        &self.fields.provider
    }

    pub fn plan_tier(&self) -> &PlanTier {
        &self.fields.plan_tier
    }

    pub fn window_semantic_key(&self) -> &WindowSemanticKey {
        &self.fields.window_semantic_key
    }

    pub fn meter_semantics_id(&self) -> &MeterSemanticsId {
        &self.fields.meter_semantics_id
    }

    pub fn billing_semantics_id(&self) -> &BillingSemanticsId {
        &self.fields.billing_semantics_id
    }

    pub fn cost_model_id(&self) -> &CostModelId {
        &self.fields.cost_model_id
    }

    pub fn fitted(&self) -> CreditsPerPercentagePoint {
        self.fields.fitted
    }

    pub fn equivalent_full_window_capacity(&self) -> Credits {
        self.fields.equivalent_full_window_capacity
    }

    pub fn fit_residual(&self) -> Credits {
        self.fields.fit_residual
    }

    pub fn uncertainty(&self) -> CoefficientUncertainty {
        self.fields.uncertainty
    }

    pub fn lag_estimate(&self) -> Option<MonotonicDuration> {
        self.fields.lag_estimate
    }

    pub fn lag_handling(&self) -> &LagHandling {
        &self.fields.lag_handling
    }

    pub fn sample_count(&self) -> u32 {
        self.fields.sample_count
    }

    pub fn fit_timestamp(&self) -> UtcTimestamp {
        self.fields.fit_timestamp
    }

    pub fn inputs(&self) -> EvidenceDigest {
        self.fields.inputs
    }

    pub fn fitting_evidence(&self) -> EvidenceFingerprint {
        self.fields.fitting_evidence
    }

    pub fn validation_evidence(&self) -> EvidenceFingerprint {
        self.fields.validation_evidence
    }

    pub fn validation_method(&self) -> &str {
        &self.fields.validation_method
    }

    pub fn validation_version(&self) -> &str {
        &self.fields.validation_version
    }

    pub fn out_of_sample_residual(&self) -> Option<Credits> {
        self.fields.out_of_sample_residual
    }

    pub fn statistical_method(&self) -> &str {
        &self.fields.statistical_method
    }

    pub fn statistical_parameters(&self) -> &str {
        &self.fields.statistical_parameters
    }

    pub fn condition_number(&self) -> Option<ConditionNumber> {
        self.fields.condition_number
    }

    pub fn observation_coverage_requirement(&self) -> &str {
        &self.fields.observation_coverage_requirement
    }

    pub fn settling_policy(&self) -> &str {
        &self.fields.settling_policy
    }

    pub fn excluded_samples(&self) -> &[ExcludedSample] {
        &self.fields.excluded_samples
    }

    pub fn activation_policy_version(&self) -> &str {
        &self.fields.activation_policy_version
    }

    pub fn aub_version(&self) -> &str {
        &self.fields.aub_version
    }

    pub fn source_revision(&self) -> &str {
        &self.fields.source_revision
    }

    pub fn validity(&self) -> ValidityInterval {
        self.fields.validity
    }

    pub fn knowledge_time(&self) -> UtcTimestamp {
        self.fields.knowledge_time
    }
}

/// One activation or supersession event on a calibration result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalibrationEventKind {
    Activation,
    Supersession,
}

impl CalibrationEventKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Activation => "activation",
            Self::Supersession => "supersession",
        }
    }
}

// --- column lists -----------------------------------------------------------

const EXPERIMENT_COLUMNS: &str = "experiment_id, provider, plan_tier, window_semantic_key, \
     meter_semantics_id, billing_semantics_id, valid_from, valid_until, knowledge_time, \
     settlement_policy_version, baseline_sampling_interval_nanos, baseline_observation_count, \
     baseline_minimum_span_nanos, baseline_max_change_resolution_units, \
     baseline_maximum_settlement_window_nanos, baseline_reported_resolution_ppm, \
     terminal_sampling_interval_nanos, terminal_observation_count, terminal_minimum_span_nanos, \
     terminal_max_change_resolution_units, terminal_maximum_settlement_window_nanos, \
     terminal_reported_resolution_ppm, settlement_shared_criteria_reason";

const CANDIDATE_COLUMNS: &str = "candidate_id, provider, plan_tier, window_semantic_key, \
     fitted_micros_per_point, equivalent_full_window_capacity_micros, fit_residual_micros, \
     uncertainty_low_micros, uncertainty_high_micros, sample_count, inputs_digest, inputs_count, \
     valid_from, valid_until, knowledge_time, \
     (SELECT experiment_id FROM calibration_experiment e WHERE e.id = c.experiment_id)";

const RESULT_COLUMNS: &str = "calibration_id, provider, plan_tier, window_semantic_key, \
     meter_semantics_id, billing_semantics_id, cost_model_id, fitted_micros_per_point, \
     equivalent_full_window_capacity_micros, fit_residual_micros, uncertainty_low_micros, \
     uncertainty_high_micros, lag_estimate_nanos, lag_handling, sample_count, fit_timestamp, \
     inputs_digest, inputs_count, fitting_evidence_digest, validation_evidence_digest, \
     validation_method, validation_version, out_of_sample_residual_micros, statistical_method, \
     statistical_parameters, condition_number_micros, observation_coverage_requirement, \
     settling_policy, excluded_samples, activation_policy_version, aub_version, source_revision, \
     valid_from, valid_until, knowledge_time";

// --- row helpers -----------------------------------------------------------

fn get<T: rusqlite::types::FromSql>(row: &Row<'_>, index: usize) -> Result<T, Error> {
    row.get::<_, T>(index)
        .map_err(|e| Error::Store(format!("cannot read column {index}: {e}")))
}

fn digest_to_hex(digest: u64) -> String {
    format!("{digest:016x}")
}

fn digest_from_hex(text: &str) -> Result<u64, Error> {
    u64::from_str_radix(text, 16)
        .map_err(|e| Error::Store(format!("malformed evidence digest '{text}': {e}")))
}

fn count_from_i64(value: i64) -> Result<usize, Error> {
    usize::try_from(value).map_err(|_| Error::Store("stored count is negative".into()))
}

fn sample_count_from_i64(value: i64) -> Result<u32, Error> {
    u32::try_from(value).map_err(|_| Error::Store("stored sample count out of u32 range".into()))
}

fn store_error_to_sql(e: Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
}

fn ppp(micros: i64) -> CreditsPerPercentagePoint {
    CreditsPerPercentagePoint::from_micros_per_point(micros)
}

fn uncertainty_from_row(low: i64, high: i64) -> Result<CoefficientUncertainty, Error> {
    CoefficientUncertainty::new(ppp(low), ppp(high))
}

fn duration_from_row(
    row: &Row<'_>,
    index: usize,
    role: &str,
    field: &str,
) -> Result<MonotonicDuration, Error> {
    let nanos = get::<i64>(row, index)?;
    u64::try_from(nanos)
        .map(MonotonicDuration::from_nanos)
        .map_err(|_| Error::Store(format!("stored {role} settlement {field} is negative")))
}

fn u32_from_row(row: &Row<'_>, index: usize, role: &str, field: &str) -> Result<u32, Error> {
    let value = get::<i64>(row, index)?;
    u32::try_from(value).map_err(|_| {
        Error::Store(format!(
            "stored {role} settlement {field} is outside the u32 range"
        ))
    })
}

fn resolution_from_row(
    row: &Row<'_>,
    index: usize,
    role: &str,
) -> Result<ReportedResolution, Error> {
    let value = get::<i64>(row, index)?;
    let value = i32::try_from(value).map_err(|_| {
        Error::Store(format!(
            "stored {role} settlement reported resolution is outside the i32 range"
        ))
    })?;
    let ppm = QuotaFractionPpm::new(value).ok_or_else(|| {
        Error::Store(format!(
            "stored {role} settlement reported resolution is invalid"
        ))
    })?;
    ReportedResolution::new(ppm).ok_or_else(|| {
        Error::Store(format!(
            "stored {role} settlement reported resolution cannot be zero"
        ))
    })
}

fn settlement_criterion_from_row(
    row: &Row<'_>,
    first_index: usize,
    role: &str,
) -> Result<SettlementCriterion, Error> {
    SettlementCriterion::new(
        duration_from_row(row, first_index, role, "sampling interval")?,
        u32_from_row(row, first_index + 1, role, "observation count")?,
        duration_from_row(row, first_index + 2, role, "minimum span")?,
        u32_from_row(row, first_index + 3, role, "maximum change")?,
        duration_from_row(row, first_index + 4, role, "maximum window")?,
        resolution_from_row(row, first_index + 5, role)?,
    )
    .map_err(|error| Error::Store(format!("invalid {role} settlement criterion: {error}")))
}

fn settlement_policy_from_row(row: &Row<'_>) -> Result<SettlementPolicy, Error> {
    let baseline = settlement_criterion_from_row(row, 10, "baseline")?;
    let terminal = settlement_criterion_from_row(row, 16, "terminal")?;
    let shared_reason = get::<String>(row, 22)?;
    let shared_reason = (!shared_reason.is_empty()).then_some(shared_reason);
    SettlementPolicy::new(get::<String>(row, 9)?, baseline, terminal, shared_reason)
        .map_err(|error| Error::Store(format!("invalid settlement policy: {error}")))
}

fn experiment_from_row(row: &Row<'_>) -> Result<CalibrationExperiment, Error> {
    Ok(CalibrationExperiment {
        id: ExperimentId::new(get::<String>(row, 0)?),
        provider: ProviderKey::new(get::<String>(row, 1)?),
        plan_tier: PlanTier::new(get::<String>(row, 2)?),
        window_semantic_key: WindowSemanticKey::new(get::<String>(row, 3)?),
        meter_semantics_id: MeterSemanticsId::new(get::<String>(row, 4)?),
        billing_semantics_id: BillingSemanticsId::new(get::<String>(row, 5)?),
        validity: ValidityInterval::new(
            UtcTimestamp::from_unix_nanos(get::<i64>(row, 6)?),
            UtcTimestamp::from_unix_nanos(get::<i64>(row, 7)?),
        )?,
        knowledge_time: UtcTimestamp::from_unix_nanos(get::<i64>(row, 8)?),
        settlement_policy: settlement_policy_from_row(row)?,
    })
}

fn candidate_from_row(row: &Row<'_>) -> Result<WindowCalibrationCandidate, Error> {
    Ok(WindowCalibrationCandidate {
        id: CandidateId::new(get::<String>(row, 0)?),
        provider: ProviderKey::new(get::<String>(row, 1)?),
        plan_tier: PlanTier::new(get::<String>(row, 2)?),
        window_semantic_key: WindowSemanticKey::new(get::<String>(row, 3)?),
        fitted: ppp(get::<i64>(row, 4)?),
        equivalent_full_window_capacity: Credits::from_micros(get::<i64>(row, 5)?),
        fit_residual: Credits::from_micros(get::<i64>(row, 6)?),
        uncertainty: uncertainty_from_row(get::<i64>(row, 7)?, get::<i64>(row, 8)?)?,
        sample_count: sample_count_from_i64(get::<i64>(row, 9)?)?,
        inputs: EvidenceDigest::from_parts(
            digest_from_hex(&get::<String>(row, 10)?)?,
            count_from_i64(get::<i64>(row, 11)?)?,
        ),
        validity: ValidityInterval::new(
            UtcTimestamp::from_unix_nanos(get::<i64>(row, 12)?),
            UtcTimestamp::from_unix_nanos(get::<i64>(row, 13)?),
        )?,
        knowledge_time: UtcTimestamp::from_unix_nanos(get::<i64>(row, 14)?),
        experiment: ExperimentId::new(get::<String>(row, 15)?),
    })
}

#[allow(clippy::too_many_lines)]
fn result_from_row(row: &Row<'_>) -> Result<WindowCalibration, Error> {
    let lag_estimate = get::<Option<i64>>(row, 12)?
        .map(|nanos| {
            u64::try_from(nanos)
                .map(MonotonicDuration::from_nanos)
                .map_err(|_| Error::Store("stored lag estimate is negative".into()))
        })
        .transpose()?;
    let fields = WindowCalibrationFields {
        id: WindowCalibrationId::new(get::<String>(row, 0)?),
        provider: ProviderKey::new(get::<String>(row, 1)?),
        plan_tier: PlanTier::new(get::<String>(row, 2)?),
        window_semantic_key: WindowSemanticKey::new(get::<String>(row, 3)?),
        meter_semantics_id: MeterSemanticsId::new(get::<String>(row, 4)?),
        billing_semantics_id: BillingSemanticsId::new(get::<String>(row, 5)?),
        cost_model_id: CostModelId::new(get::<String>(row, 6)?),
        fitted: ppp(get::<i64>(row, 7)?),
        equivalent_full_window_capacity: Credits::from_micros(get::<i64>(row, 8)?),
        fit_residual: Credits::from_micros(get::<i64>(row, 9)?),
        uncertainty: uncertainty_from_row(get::<i64>(row, 10)?, get::<i64>(row, 11)?)?,
        lag_estimate,
        lag_handling: LagHandling::new(get::<String>(row, 13)?),
        sample_count: sample_count_from_i64(get::<i64>(row, 14)?)?,
        fit_timestamp: UtcTimestamp::from_unix_nanos(get::<i64>(row, 15)?),
        inputs: EvidenceDigest::from_parts(
            digest_from_hex(&get::<String>(row, 16)?)?,
            count_from_i64(get::<i64>(row, 17)?)?,
        ),
        fitting_evidence: EvidenceFingerprint::from_raw(digest_from_hex(&get::<String>(row, 18)?)?),
        validation_evidence: EvidenceFingerprint::from_raw(digest_from_hex(&get::<String>(
            row, 19,
        )?)?),
        validation_method: get::<String>(row, 20)?,
        validation_version: get::<String>(row, 21)?,
        out_of_sample_residual: get::<Option<i64>>(row, 22)?.map(Credits::from_micros),
        statistical_method: get::<String>(row, 23)?,
        statistical_parameters: get::<String>(row, 24)?,
        condition_number: get::<Option<i64>>(row, 25)?.map(ConditionNumber::from_micros),
        observation_coverage_requirement: get::<String>(row, 26)?,
        settling_policy: get::<String>(row, 27)?,
        excluded_samples: decode_excluded(&get::<String>(row, 28)?)?,
        activation_policy_version: get::<String>(row, 29)?,
        aub_version: get::<String>(row, 30)?,
        source_revision: get::<String>(row, 31)?,
        validity: ValidityInterval::new(
            UtcTimestamp::from_unix_nanos(get::<i64>(row, 32)?),
            UtcTimestamp::from_unix_nanos(get::<i64>(row, 33)?),
        )?,
        knowledge_time: UtcTimestamp::from_unix_nanos(get::<i64>(row, 34)?),
    };
    WindowCalibration::from_fields(fields)
}

// --- experiments ---------------------------------------------------------

/// Records a calibration experiment. The `experiment_id` is unique; a second insert
/// of the same identifier fails at the database.
pub fn insert_experiment(
    conn: &Connection,
    experiment: &CalibrationExperiment,
) -> Result<ExperimentDbId, Error> {
    let policy = &experiment.settlement_policy;
    let baseline = policy.baseline();
    let terminal = policy.terminal();
    let sqlite_duration = |duration: MonotonicDuration, field: &str| {
        i64::try_from(duration.as_nanos())
            .map_err(|_| Error::Store(format!("settlement {field} does not fit in SQLite INTEGER")))
    };
    let id = conn
        .query_row(
            "INSERT INTO calibration_experiment (
                experiment_id, provider, plan_tier, window_semantic_key,
                meter_semantics_id, billing_semantics_id, valid_from, valid_until, knowledge_time,
                settlement_policy_version, baseline_sampling_interval_nanos,
                baseline_observation_count, baseline_minimum_span_nanos,
                baseline_max_change_resolution_units, baseline_maximum_settlement_window_nanos,
                baseline_reported_resolution_ppm, terminal_sampling_interval_nanos,
                terminal_observation_count, terminal_minimum_span_nanos,
                terminal_max_change_resolution_units, terminal_maximum_settlement_window_nanos,
                terminal_reported_resolution_ppm, settlement_shared_criteria_reason
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                ?17, ?18, ?19, ?20, ?21, ?22, ?23
            ) RETURNING id",
            params![
                experiment.id.as_str(),
                experiment.provider.as_str(),
                experiment.plan_tier.as_str(),
                experiment.window_semantic_key.as_str(),
                experiment.meter_semantics_id.as_str(),
                experiment.billing_semantics_id.as_str(),
                experiment.validity.valid_from().unix_nanos(),
                experiment.validity.valid_until().unix_nanos(),
                experiment.knowledge_time.unix_nanos(),
                policy.version(),
                sqlite_duration(baseline.sampling_interval(), "baseline sampling interval")?,
                i64::from(baseline.required_observations()),
                sqlite_duration(baseline.minimum_span(), "baseline minimum span")?,
                i64::from(baseline.maximum_change_resolution_units()),
                sqlite_duration(
                    baseline.maximum_settlement_window(),
                    "baseline maximum settlement window",
                )?,
                i64::from(baseline.reported_resolution().as_ppm().get()),
                sqlite_duration(terminal.sampling_interval(), "terminal sampling interval")?,
                i64::from(terminal.required_observations()),
                sqlite_duration(terminal.minimum_span(), "terminal minimum span")?,
                i64::from(terminal.maximum_change_resolution_units()),
                sqlite_duration(
                    terminal.maximum_settlement_window(),
                    "terminal maximum settlement window",
                )?,
                i64::from(terminal.reported_resolution().as_ppm().get()),
                policy.shared_criteria_reason().unwrap_or_default(),
            ],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|e| Error::Store(format!("cannot insert the calibration_experiment row: {e}")))?;
    Ok(ExperimentDbId(id))
}

/// Loads an experiment by its semantic identifier.
pub fn load_experiment(
    conn: &Connection,
    id: &ExperimentId,
) -> Result<Option<CalibrationExperiment>, Error> {
    conn.query_row(
        &format!(
            "SELECT {EXPERIMENT_COLUMNS} FROM calibration_experiment WHERE experiment_id = ?1"
        ),
        params![id.as_str()],
        |row| experiment_from_row(row).map_err(store_error_to_sql),
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot load the calibration_experiment row: {e}")))
}

fn resolve_experiment_db_id(conn: &Connection, id: &ExperimentId) -> Result<i64, Error> {
    conn.query_row(
        "SELECT id FROM calibration_experiment WHERE experiment_id = ?1",
        params![id.as_str()],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot resolve experiment '{}': {e}", id.as_str())))?
    .ok_or_else(|| Error::Store(format!("no calibration experiment '{}'", id.as_str())))
}

// --- candidates ---------------------------------------------------------

/// Inserts a candidate, resolving its experiment by semantic id. Fails if the
/// experiment does not exist or the `candidate_id` is already taken.
pub fn insert_candidate(
    conn: &Connection,
    candidate: &WindowCalibrationCandidate,
) -> Result<CandidateDbId, Error> {
    let experiment_db_id = resolve_experiment_db_id(conn, &candidate.experiment)?;
    let id = conn
        .query_row(
            "INSERT INTO window_calibration_candidate (
                candidate_id, experiment_id, provider, plan_tier, window_semantic_key,
                fitted_micros_per_point, equivalent_full_window_capacity_micros, fit_residual_micros,
                uncertainty_low_micros, uncertainty_high_micros, sample_count, inputs_digest,
                inputs_count, valid_from, valid_until, knowledge_time
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
            RETURNING id",
            params![
                candidate.id.as_str(),
                experiment_db_id,
                candidate.provider.as_str(),
                candidate.plan_tier.as_str(),
                candidate.window_semantic_key.as_str(),
                candidate.fitted.micros_per_point(),
                candidate.equivalent_full_window_capacity.micros(),
                candidate.fit_residual.micros(),
                candidate.uncertainty.lower().micros_per_point(),
                candidate.uncertainty.upper().micros_per_point(),
                i64::from(candidate.sample_count),
                digest_to_hex(candidate.inputs.digest()),
                i64::try_from(candidate.inputs.count())
                    .map_err(|_| Error::Store("inputs count out of i64 range".into()))?,
                candidate.validity.valid_from().unix_nanos(),
                candidate.validity.valid_until().unix_nanos(),
                candidate.knowledge_time.unix_nanos(),
            ],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|e| {
            Error::Store(format!(
                "cannot insert the window_calibration_candidate row: {e}"
            ))
        })?;
    Ok(CandidateDbId(id))
}

/// Loads a candidate by its semantic identifier.
pub fn load_candidate(
    conn: &Connection,
    id: &CandidateId,
) -> Result<Option<WindowCalibrationCandidate>, Error> {
    conn.query_row(
        &format!(
            "SELECT {CANDIDATE_COLUMNS} FROM window_calibration_candidate c WHERE candidate_id = ?1"
        ),
        params![id.as_str()],
        |row| candidate_from_row(row).map_err(store_error_to_sql),
    )
    .optional()
    .map_err(|e| {
        Error::Store(format!(
            "cannot load the window_calibration_candidate row: {e}"
        ))
    })
}

// --- results ----------------------------------------------------------

/// Inserts a result and its source-experiment links in one short transaction.
///
/// `source_experiments` must be non-empty and every identifier must resolve: a result
/// with no traceable experiment behind it cannot answer where its number came from.
pub fn insert_result(
    conn: &mut Connection,
    calibration: &WindowCalibration,
    source_experiments: &[ExperimentId],
) -> Result<CalibrationResultDbId, Error> {
    if source_experiments.is_empty() {
        return Err(Error::Store(format!(
            "window calibration '{}' names no source experiment",
            calibration.id().as_str()
        )));
    }
    let tx = conn.transaction().map_err(|e| {
        Error::Store(format!(
            "cannot open the calibration result transaction: {e}"
        ))
    })?;
    let db_id = insert_result_row(&tx, calibration)?;
    for experiment in source_experiments {
        let experiment_db_id = resolve_experiment_db_id(&tx, experiment)?;
        tx.execute(
            "INSERT INTO window_calibration_source_experiment (result_id, experiment_id)
             VALUES (?1, ?2)",
            params![db_id, experiment_db_id],
        )
        .map_err(|e| {
            Error::Store(format!(
                "cannot link result '{}' to experiment '{}': {e}",
                calibration.id().as_str(),
                experiment.as_str()
            ))
        })?;
    }
    tx.commit()
        .map_err(|e| Error::Store(format!("cannot commit the calibration result: {e}")))?;
    Ok(CalibrationResultDbId(db_id))
}

#[allow(clippy::too_many_lines)]
fn insert_result_row(tx: &rusqlite::Transaction<'_>, c: &WindowCalibration) -> Result<i64, Error> {
    let f = &c.fields;
    tx.query_row(
        "INSERT INTO window_calibration_result (
            calibration_id, provider, plan_tier, window_semantic_key, meter_semantics_id,
            billing_semantics_id, cost_model_id, fitted_micros_per_point,
            equivalent_full_window_capacity_micros, fit_residual_micros, uncertainty_low_micros,
            uncertainty_high_micros, lag_estimate_nanos, lag_handling, sample_count, fit_timestamp,
            inputs_digest, inputs_count, fitting_evidence_digest, validation_evidence_digest,
            validation_method, validation_version, out_of_sample_residual_micros, statistical_method,
            statistical_parameters, condition_number_micros, observation_coverage_requirement,
            settling_policy, excluded_samples, activation_policy_version, aub_version,
            source_revision, valid_from, valid_until, knowledge_time
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19,
            ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35
        ) RETURNING id",
        params![
            f.id.as_str(),
            f.provider.as_str(),
            f.plan_tier.as_str(),
            f.window_semantic_key.as_str(),
            f.meter_semantics_id.as_str(),
            f.billing_semantics_id.as_str(),
            f.cost_model_id.as_str(),
            f.fitted.micros_per_point(),
            f.equivalent_full_window_capacity.micros(),
            f.fit_residual.micros(),
            f.uncertainty.lower().micros_per_point(),
            f.uncertainty.upper().micros_per_point(),
            f.lag_estimate.map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX)),
            f.lag_handling.as_str(),
            i64::from(f.sample_count),
            f.fit_timestamp.unix_nanos(),
            digest_to_hex(f.inputs.digest()),
            i64::try_from(f.inputs.count())
                .map_err(|_| Error::Store("inputs count out of i64 range".into()))?,
            digest_to_hex(f.fitting_evidence.as_u64()),
            digest_to_hex(f.validation_evidence.as_u64()),
            f.validation_method,
            f.validation_version,
            f.out_of_sample_residual.map(Credits::micros),
            f.statistical_method,
            f.statistical_parameters,
            f.condition_number.map(ConditionNumber::micros),
            f.observation_coverage_requirement,
            f.settling_policy,
            encode_excluded(&f.excluded_samples),
            f.activation_policy_version,
            f.aub_version,
            f.source_revision,
            f.validity.valid_from().unix_nanos(),
            f.validity.valid_until().unix_nanos(),
            f.knowledge_time.unix_nanos(),
        ],
        |row| row.get::<_, i64>(0),
    )
    .map_err(|e| Error::Store(format!("cannot insert the window_calibration_result row: {e}")))
}

/// Loads a result by its semantic identifier.
pub fn load_result(
    conn: &Connection,
    id: &WindowCalibrationId,
) -> Result<Option<WindowCalibration>, Error> {
    conn.query_row(
        &format!(
            "SELECT {RESULT_COLUMNS} FROM window_calibration_result WHERE calibration_id = ?1"
        ),
        params![id.as_str()],
        |row| result_from_row(row).map_err(store_error_to_sql),
    )
    .optional()
    .map_err(|e| {
        Error::Store(format!(
            "cannot load the window_calibration_result row: {e}"
        ))
    })
}

/// The source experiments a result was fitted from, by semantic id, in insertion
/// order.
pub fn source_experiments(
    conn: &Connection,
    id: &WindowCalibrationId,
) -> Result<Vec<ExperimentId>, Error> {
    let mut stmt = conn
        .prepare(
            "SELECT e.experiment_id
             FROM window_calibration_source_experiment link
             JOIN window_calibration_result r ON r.id = link.result_id
             JOIN calibration_experiment e ON e.id = link.experiment_id
             WHERE r.calibration_id = ?1
             ORDER BY link.id",
        )
        .map_err(|e| Error::Store(format!("cannot prepare the source-experiment query: {e}")))?;
    let rows = stmt
        .query_map(params![id.as_str()], |row| row.get::<_, String>(0))
        .map_err(|e| Error::Store(format!("cannot query source experiments: {e}")))?;
    let mut ids = Vec::new();
    for row in rows {
        ids.push(ExperimentId::new(row.map_err(|e| {
            Error::Store(format!("cannot read a source-experiment row: {e}"))
        })?));
    }
    Ok(ids)
}

// --- lifecycle: point-in-time by knowledge time --------------------------

/// Records `id` becoming the active calibration for its scope at `event_at`.
///
/// The chain is enforced, not merely described, and it is scoped: the first
/// activation in a scope has no predecessor, and every later activation must name, as
/// its superseded calibration, the one active in that scope just before `event_at`. A
/// call that breaks either rule is refused before anything is written, so
/// `load_active_at` can never fork.
pub fn activate(
    conn: &mut Connection,
    id: &WindowCalibrationId,
    event_at: UtcTimestamp,
    supersedes: Option<&WindowCalibrationId>,
) -> Result<CalibrationLifecycleEventId, Error> {
    let tx = conn
        .transaction()
        .map_err(|e| Error::Store(format!("cannot open the activation transaction: {e}")))?;

    let calibration = load_result_in(&tx, id)?.ok_or_else(|| {
        Error::Store(format!(
            "no window calibration '{}' to activate",
            id.as_str()
        ))
    })?;
    let scope = calibration.scope();
    let active_before = load_active_at_in(&tx, &scope, instant_before(event_at))?;

    match (active_before.as_ref(), supersedes) {
        (None, None) => {}
        (None, Some(named)) => {
            return Err(Error::Store(format!(
                "first activation of '{}' in its scope names predecessor '{}', which is not active",
                id.as_str(),
                named.as_str()
            )));
        }
        (Some(active), None) => {
            return Err(Error::Store(format!(
                "activation of '{}' must supersede the active calibration '{}'",
                id.as_str(),
                active.id().as_str()
            )));
        }
        (Some(active), Some(named)) => {
            if active.id() != named {
                return Err(Error::Store(format!(
                    "activation of '{}' supersedes '{}' but '{}' is active in that scope",
                    id.as_str(),
                    named.as_str(),
                    active.id().as_str()
                )));
            }
            if id == named {
                return Err(Error::Store(format!(
                    "activation of '{}' would supersede itself",
                    id.as_str()
                )));
            }
        }
    }

    let result_db_id = resolve_result_db_id(&tx, id)?;
    let supersedes_db_id = supersedes
        .map(|named| resolve_result_db_id(&tx, named))
        .transpose()?;
    let kind = if supersedes.is_some() {
        CalibrationEventKind::Supersession
    } else {
        CalibrationEventKind::Activation
    };

    let event_id = tx
        .query_row(
            "INSERT INTO calibration_lifecycle (
                calibration_result_id, event_kind, event_at, supersedes_result_id
            ) VALUES (?1, ?2, ?3, ?4) RETURNING id",
            params![
                result_db_id,
                kind.as_str(),
                event_at.unix_nanos(),
                supersedes_db_id
            ],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|e| {
            Error::Store(format!(
                "cannot insert the calibration_lifecycle event: {e}"
            ))
        })?;

    tx.commit()
        .map_err(|e| Error::Store(format!("cannot commit the activation: {e}")))?;
    Ok(CalibrationLifecycleEventId(event_id))
}

/// The calibration active for `scope` as of knowledge instant `knowledge_time`: the
/// result of the latest lifecycle event at or before that instant whose result sits
/// in the scope. `None` before the scope's first activation.
pub fn load_active_at(
    conn: &Connection,
    scope: &CalibrationScope,
    knowledge_time: UtcTimestamp,
) -> Result<Option<WindowCalibration>, Error> {
    load_active_at_in(conn, scope, knowledge_time)
}

fn load_active_at_in(
    conn: &Connection,
    scope: &CalibrationScope,
    knowledge_time: UtcTimestamp,
) -> Result<Option<WindowCalibration>, Error> {
    let sql = format!(
        "SELECT {RESULT_COLUMNS} FROM window_calibration_result
         WHERE id = (
            SELECT l.calibration_result_id
            FROM calibration_lifecycle l
            JOIN window_calibration_result r ON r.id = l.calibration_result_id
            WHERE l.event_at <= ?1
              AND r.provider = ?2 AND r.plan_tier = ?3 AND r.window_semantic_key = ?4
            ORDER BY l.event_at DESC, l.id DESC
            LIMIT 1
         )"
    );
    conn.query_row(
        &sql,
        params![
            knowledge_time.unix_nanos(),
            scope.provider.as_str(),
            scope.plan_tier.as_str(),
            scope.window_semantic_key.as_str(),
        ],
        |row| result_from_row(row).map_err(store_error_to_sql),
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot load the active calibration: {e}")))
}

// --- point-in-time by valid time ---------------------------------------

/// Every result in `scope` whose validity interval contains `valid_time`, optionally
/// restricted to what `aub` knew as of `known_by`. Ordered by validity start.
///
/// This is the physical-world reading: it asks which calibration describes the given
/// instant, independent of which one was ever activated. With `known_by` set it is the
/// "what did `aub` then know about the world" reading, which can legitimately differ
/// from the unrestricted one for a witness imported after it took effect.
pub fn results_valid_at(
    conn: &Connection,
    scope: &CalibrationScope,
    valid_time: UtcTimestamp,
    known_by: Option<UtcTimestamp>,
) -> Result<Vec<WindowCalibration>, Error> {
    let knowledge_bound = known_by.map_or(i64::MAX, UtcTimestamp::unix_nanos);
    let sql = format!(
        "SELECT {RESULT_COLUMNS} FROM window_calibration_result
         WHERE provider = ?1 AND plan_tier = ?2 AND window_semantic_key = ?3
           AND valid_from <= ?4 AND valid_until >= ?4
           AND knowledge_time <= ?5
         ORDER BY valid_from, id"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| Error::Store(format!("cannot prepare the valid-time query: {e}")))?;
    let rows = stmt
        .query_map(
            params![
                scope.provider.as_str(),
                scope.plan_tier.as_str(),
                scope.window_semantic_key.as_str(),
                valid_time.unix_nanos(),
                knowledge_bound,
            ],
            |row| result_from_row(row).map_err(store_error_to_sql),
        )
        .map_err(|e| Error::Store(format!("cannot query calibrations by valid time: {e}")))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| Error::Store(format!("cannot read a calibration row: {e}")))?);
    }
    Ok(out)
}

fn load_result_in(
    conn: &Connection,
    id: &WindowCalibrationId,
) -> Result<Option<WindowCalibration>, Error> {
    conn.query_row(
        &format!(
            "SELECT {RESULT_COLUMNS} FROM window_calibration_result WHERE calibration_id = ?1"
        ),
        params![id.as_str()],
        |row| result_from_row(row).map_err(store_error_to_sql),
    )
    .optional()
    .map_err(|e| {
        Error::Store(format!(
            "cannot load the window_calibration_result row: {e}"
        ))
    })
}

fn resolve_result_db_id(conn: &Connection, id: &WindowCalibrationId) -> Result<i64, Error> {
    conn.query_row(
        "SELECT id FROM window_calibration_result WHERE calibration_id = ?1",
        params![id.as_str()],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot resolve calibration '{}': {e}", id.as_str())))?
    .ok_or_else(|| Error::Store(format!("no window calibration '{}'", id.as_str())))
}

/// One nanosecond before `at`: the instant `load_active_at` reads to find the state a
/// new event at `at` replaces.
fn instant_before(at: UtcTimestamp) -> UtcTimestamp {
    UtcTimestamp::from_unix_nanos(at.unix_nanos().saturating_sub(1))
}

/// Returns distinct calibration scopes that have ever had a result fitted.
pub fn fitted_calibration_scopes(conn: &Connection) -> Result<Vec<CalibrationScope>, Error> {
    let mut statement = conn
        .prepare(
            "SELECT DISTINCT provider, plan_tier, window_semantic_key FROM window_calibration_result",
        )
        .map_err(|e| Error::Store(format!("cannot list calibration scopes: {e}")))?;
    let rows = statement
        .query_map([], |row| {
            let provider: String = row.get(0)?;
            let plan_tier: String = row.get(1)?;
            let window_semantic_key: String = row.get(2)?;
            Ok(CalibrationScope {
                provider: ProviderKey::new(provider),
                plan_tier: PlanTier::new(plan_tier),
                window_semantic_key: WindowSemanticKey::new(window_semantic_key),
            })
        })
        .map_err(|e| Error::Store(format!("cannot query calibration scopes: {e}")))?;
    let mut scopes = Vec::new();
    for row in rows {
        scopes.push(
            row.map_err(|e| Error::Store(format!("cannot read calibration scope row: {e}")))?,
        );
    }
    Ok(scopes)
}

/// Returns the count of calibration lifecycle events.
pub fn lifecycle_event_count(conn: &Connection) -> Result<i64, Error> {
    conn.query_row("SELECT COUNT(*) FROM calibration_lifecycle", [], |row| {
        row.get(0)
    })
    .map_err(|e| Error::Store(format!("cannot count calibration lifecycle events: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::{FakeClock, MonotonicDuration};
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-store-calibration-test-{}-{suffix}",
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

    fn fixture_conn() -> (ScratchDir, Connection) {
        let scratch = ScratchDir::new();
        let policy = PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1000),
        };
        let mut conn = open(
            &scratch.path().join("calibration.db"),
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

    fn interval(from: i64, until: i64) -> ValidityInterval {
        ValidityInterval::new(ts(from), ts(until)).unwrap()
    }

    fn settlement_policy() -> SettlementPolicy {
        SettlementPolicy::conservative_default(
            ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap(),
        )
    }

    fn scope() -> CalibrationScope {
        CalibrationScope {
            provider: ProviderKey::new("anthropic"),
            plan_tier: PlanTier::new("max"),
            window_semantic_key: WindowSemanticKey::new("account"),
        }
    }

    fn evidence(ids: &[&str]) -> BTreeSet<EvidenceId> {
        ids.iter().map(|s| EvidenceId::new(*s)).collect()
    }

    fn experiment(id: &str, validity: ValidityInterval, knowledge: i64) -> CalibrationExperiment {
        CalibrationExperiment {
            id: ExperimentId::new(id),
            provider: ProviderKey::new("anthropic"),
            plan_tier: PlanTier::new("max"),
            window_semantic_key: WindowSemanticKey::new("account"),
            meter_semantics_id: MeterSemanticsId::new("meter-v1"),
            billing_semantics_id: BillingSemanticsId::new("billing-v1"),
            settlement_policy: settlement_policy(),
            validity,
            knowledge_time: ts(knowledge),
        }
    }

    fn calibration(
        id: &str,
        validity: ValidityInterval,
        knowledge: i64,
        fitted_micros: i64,
    ) -> WindowCalibration {
        WindowCalibration::from_fields(WindowCalibrationFields {
            id: WindowCalibrationId::new(id),
            provider: ProviderKey::new("anthropic"),
            plan_tier: PlanTier::new("max"),
            window_semantic_key: WindowSemanticKey::new("account"),
            meter_semantics_id: MeterSemanticsId::new("meter-v1"),
            billing_semantics_id: BillingSemanticsId::new("billing-v1"),
            cost_model_id: CostModelId::new("cm-1"),
            fitted: ppp(fitted_micros),
            equivalent_full_window_capacity: Credits::from_micros(12_000_000),
            fit_residual: Credits::from_micros(4_200),
            uncertainty: CoefficientUncertainty::new(
                ppp(fitted_micros - 10_000),
                ppp(fitted_micros + 10_000),
            )
            .unwrap(),
            lag_estimate: Some(MonotonicDuration::from_seconds(90)),
            lag_handling: LagHandling::new("shifted-by-estimate"),
            sample_count: 40,
            fit_timestamp: ts(knowledge),
            inputs: EvidenceDigest::from_inputs(&evidence(&["s-1", "s-2", "s-3"])),
            fitting_evidence: EvidenceFingerprint::from_inputs(&evidence(&["s-1", "s-2"])),
            validation_evidence: EvidenceFingerprint::from_inputs(&evidence(&["s-3"])),
            validation_method: "holdout".to_string(),
            validation_version: "v2".to_string(),
            out_of_sample_residual: Some(Credits::from_micros(7_000)),
            statistical_method: "ols".to_string(),
            statistical_parameters: "{\"ridge\":0}".to_string(),
            condition_number: Some(ConditionNumber::from_micros(3_500_000)),
            observation_coverage_requirement: "ninety-percent".to_string(),
            settling_policy: "plateau-3".to_string(),
            excluded_samples: vec![
                ExcludedSample::new("s-9", "reset boundary contamination").unwrap(),
            ],
            activation_policy_version: "ap-v1".to_string(),
            aub_version: "0.1.0".to_string(),
            source_revision: "abc1234".to_string(),
            validity,
            knowledge_time: ts(knowledge),
        })
        .unwrap()
    }

    #[test]
    fn experiment_round_trips() {
        let (_scratch, conn) = fixture_conn();
        let exp = experiment("exp-rt", interval(100, 200), 300);
        insert_experiment(&conn, &exp).unwrap();
        let loaded = load_experiment(&conn, &exp.id).unwrap().unwrap();
        assert_eq!(loaded, exp);
    }

    #[test]
    fn experiment_keeps_the_recorded_policy_when_a_later_policy_is_created() {
        let (_scratch, conn) = fixture_conn();
        let mut exp = experiment("exp-policy-snapshot", interval(100, 200), 300);
        let recorded_policy = SettlementPolicy::new(
            "policy-before-change",
            SettlementCriterion::new(
                MonotonicDuration::from_seconds(60),
                2,
                MonotonicDuration::from_seconds(60),
                0,
                MonotonicDuration::from_seconds(600),
                ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap(),
            )
            .unwrap(),
            SettlementCriterion::new(
                MonotonicDuration::from_seconds(120),
                3,
                MonotonicDuration::from_seconds(240),
                1,
                MonotonicDuration::from_seconds(1_200),
                ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap(),
            )
            .unwrap(),
            None,
        )
        .unwrap();
        exp.settlement_policy = recorded_policy.clone();
        insert_experiment(&conn, &exp).unwrap();

        let later_policy = settlement_policy();
        assert_ne!(later_policy, recorded_policy);
        let loaded = load_experiment(&conn, &exp.id).unwrap().unwrap();
        assert_eq!(loaded.settlement_policy, recorded_policy);
    }

    #[test]
    fn candidate_round_trips_and_needs_a_real_experiment() {
        let (_scratch, conn) = fixture_conn();
        insert_experiment(&conn, &experiment("exp-cand", interval(1, 9), 1)).unwrap();
        let candidate = WindowCalibrationCandidate {
            id: CandidateId::new("cand-rt"),
            experiment: ExperimentId::new("exp-cand"),
            provider: ProviderKey::new("anthropic"),
            plan_tier: PlanTier::new("max"),
            window_semantic_key: WindowSemanticKey::new("account"),
            fitted: ppp(880_000),
            equivalent_full_window_capacity: Credits::from_micros(11_000_000),
            fit_residual: Credits::from_micros(9_100),
            uncertainty: CoefficientUncertainty::new(ppp(870_000), ppp(890_000)).unwrap(),
            sample_count: 12,
            inputs: EvidenceDigest::from_inputs(&evidence(&["s-1", "s-2"])),
            validity: interval(1, 9),
            knowledge_time: ts(2),
        };
        insert_candidate(&conn, &candidate).unwrap();
        assert_eq!(
            load_candidate(&conn, &candidate.id).unwrap().unwrap(),
            candidate
        );

        // A candidate naming an experiment that does not exist is refused by the
        // repository, before any row is written.
        let orphan = WindowCalibrationCandidate {
            id: CandidateId::new("cand-orphan"),
            experiment: ExperimentId::new("exp-missing"),
            ..candidate
        };
        assert!(insert_candidate(&conn, &orphan).is_err());
        assert!(load_candidate(&conn, &orphan.id).unwrap().is_none());
    }

    /// A full result round-trips with every field intact, and its provenance verifies
    /// against exactly the evidence set it was built from.
    #[test]
    fn result_round_trips_with_every_field_and_verifiable_provenance() {
        let (_scratch, mut conn) = fixture_conn();
        insert_experiment(&conn, &experiment("exp-1", interval(100, 300), 90)).unwrap();
        let cal = calibration("wc-rt", interval(100, 300), 100, 900_000);
        insert_result(&mut conn, &cal, &[ExperimentId::new("exp-1")]).unwrap();

        let loaded = load_result(&conn, cal.id()).unwrap().unwrap();
        assert_eq!(loaded, cal);
        assert_eq!(loaded.excluded_samples().len(), 1);
        assert_eq!(
            loaded.excluded_samples()[0].reason(),
            "reset boundary contamination"
        );
        assert_eq!(
            loaded.lag_estimate(),
            Some(MonotonicDuration::from_seconds(90))
        );
        assert_eq!(
            loaded.condition_number(),
            Some(ConditionNumber::from_micros(3_500_000))
        );

        assert!(
            loaded
                .inputs()
                .verify_expansion(&evidence(&["s-1", "s-2", "s-3"]))
        );
        assert!(!loaded.inputs().verify_expansion(&evidence(&["s-1", "s-2"])));

        assert_eq!(
            source_experiments(&conn, cal.id()).unwrap(),
            vec![ExperimentId::new("exp-1")]
        );
    }

    /// Every field the design lists is a column on the result table: a result missing
    /// one cannot answer the question it exists for.
    #[test]
    fn the_result_table_carries_every_designed_field() {
        let (_scratch, conn) = fixture_conn();
        let mut columns: Vec<String> = conn
            .prepare("PRAGMA table_info(window_calibration_result)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        columns.sort();
        let mut expected: Vec<String> = [
            "id",
            "calibration_id",
            "provider",
            "plan_tier",
            "window_semantic_key",
            "meter_semantics_id",
            "billing_semantics_id",
            "cost_model_id",
            "fitted_micros_per_point",
            "equivalent_full_window_capacity_micros",
            "fit_residual_micros",
            "uncertainty_low_micros",
            "uncertainty_high_micros",
            "lag_estimate_nanos",
            "lag_handling",
            "sample_count",
            "fit_timestamp",
            "inputs_digest",
            "inputs_count",
            "fitting_evidence_digest",
            "validation_evidence_digest",
            "validation_method",
            "validation_version",
            "out_of_sample_residual_micros",
            "statistical_method",
            "statistical_parameters",
            "condition_number_micros",
            "observation_coverage_requirement",
            "settling_policy",
            "excluded_samples",
            "activation_policy_version",
            "aub_version",
            "source_revision",
            "valid_from",
            "valid_until",
            "knowledge_time",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        expected.sort();
        assert_eq!(columns, expected);
    }

    /// The repository exposes no update path, and a direct UPDATE is refused by the
    /// table: immutability is a property of the schema, not of repository politeness.
    #[test]
    fn results_are_immutable_in_the_repository_and_the_database() {
        let (_scratch, mut conn) = fixture_conn();
        insert_experiment(&conn, &experiment("exp-imm", interval(1, 9), 1)).unwrap();
        let cal = calibration("wc-imm", interval(1, 9), 1, 500_000);
        insert_result(&mut conn, &cal, &[ExperimentId::new("exp-imm")]).unwrap();

        let err = conn
            .execute(
                "UPDATE window_calibration_result SET fitted_micros_per_point = 1",
                [],
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("immutable"), "unexpected: {err}");
        // Nothing in the module's surface can mutate a result: the only writers are
        // insert_* and activate, and activate writes only the lifecycle table.
        assert!(load_result(&conn, cal.id()).unwrap().is_some());
    }

    /// Point-in-time by knowledge time and by valid time select independently, and
    /// the two legitimately return different records for a calibration imported after
    /// it took effect: a report is right about what `aub` then knew and can still be
    /// wrong about the world.
    #[test]
    fn knowledge_time_and_valid_time_select_independently() {
        let (_scratch, mut conn) = fixture_conn();
        insert_experiment(&conn, &experiment("exp-old", interval(100, 200), 1_000)).unwrap();
        insert_experiment(&conn, &experiment("exp-new", interval(201, 300), 2_000)).unwrap();

        // R_old describes [100,200] and was known from t=1000.
        let r_old = calibration("wc-old", interval(100, 200), 1_000, 900_000);
        // R_new describes [201,300] but was imported and activated only at t=2000.
        let r_new = calibration("wc-new", interval(201, 300), 2_000, 700_000);
        insert_result(&mut conn, &r_old, &[ExperimentId::new("exp-old")]).unwrap();
        insert_result(&mut conn, &r_new, &[ExperimentId::new("exp-new")]).unwrap();

        activate(&mut conn, r_old.id(), ts(1_000), None).unwrap();
        activate(&mut conn, r_new.id(), ts(2_000), Some(r_old.id())).unwrap();

        // Knowledge time: what was active as of an instant.
        assert!(load_active_at(&conn, &scope(), ts(999)).unwrap().is_none());
        assert_eq!(
            load_active_at(&conn, &scope(), ts(1_500))
                .unwrap()
                .unwrap()
                .id(),
            r_old.id()
        );
        assert_eq!(
            load_active_at(&conn, &scope(), ts(2_500))
                .unwrap()
                .unwrap()
                .id(),
            r_new.id()
        );

        // Valid time with full knowledge: which calibration describes the instant.
        let valid_250: Vec<_> = results_valid_at(&conn, &scope(), ts(250), None)
            .unwrap()
            .into_iter()
            .map(|c| c.id().clone())
            .collect();
        assert_eq!(valid_250, vec![r_new.id().clone()]);

        // The two axes disagree: as of knowledge instant 1500 nothing active covers
        // valid instant 250, yet with full knowledge R_new does.
        let known_by_1500 = results_valid_at(&conn, &scope(), ts(250), Some(ts(1_500))).unwrap();
        assert!(
            known_by_1500.is_empty(),
            "R_new was not yet known at t=1500"
        );
        assert_eq!(
            load_active_at(&conn, &scope(), ts(1_500))
                .unwrap()
                .unwrap()
                .id(),
            r_old.id(),
            "knowledge-time active answer differs from the full-knowledge valid-time answer"
        );
    }

    /// The scoped activation chain is enforced before anything is written: a first
    /// activation cannot name a predecessor, a successor must supersede exactly the
    /// active calibration, and a calibration cannot supersede itself.
    #[test]
    fn scoped_activation_chain_is_enforced() {
        let (_scratch, mut conn) = fixture_conn();
        insert_experiment(&conn, &experiment("exp-c", interval(1, 9), 1)).unwrap();
        let a = calibration("wc-a", interval(1, 9), 1, 900_000);
        let b = calibration("wc-b", interval(1, 9), 1, 800_000);
        insert_result(&mut conn, &a, &[ExperimentId::new("exp-c")]).unwrap();
        insert_result(&mut conn, &b, &[ExperimentId::new("exp-c")]).unwrap();

        assert!(activate(&mut conn, a.id(), ts(1_000), Some(b.id())).is_err());
        activate(&mut conn, a.id(), ts(1_000), None).unwrap();
        assert!(activate(&mut conn, b.id(), ts(2_000), None).is_err());
        assert!(activate(&mut conn, a.id(), ts(2_000), Some(a.id())).is_err());
        activate(&mut conn, b.id(), ts(2_000), Some(a.id())).unwrap();
        assert_eq!(
            load_active_at(&conn, &scope(), ts(2_000))
                .unwrap()
                .unwrap()
                .id(),
            b.id()
        );
    }

    /// A different scope has its own independent active calibration: activating in the
    /// `model` window does not disturb the `account` window's chain.
    #[test]
    fn activation_scopes_do_not_interfere() {
        let (_scratch, mut conn) = fixture_conn();
        insert_experiment(&conn, &experiment("exp-s", interval(1, 9), 1)).unwrap();
        let account = calibration("wc-account", interval(1, 9), 1, 900_000);
        let mut model_fields = account.fields.clone();
        model_fields.id = WindowCalibrationId::new("wc-model");
        model_fields.window_semantic_key = WindowSemanticKey::new("model");
        let model = WindowCalibration::from_fields(model_fields).unwrap();

        insert_result(&mut conn, &account, &[ExperimentId::new("exp-s")]).unwrap();
        insert_result(&mut conn, &model, &[ExperimentId::new("exp-s")]).unwrap();

        activate(&mut conn, account.id(), ts(1_000), None).unwrap();
        // First activation in the `model` scope: also has no predecessor, despite the
        // `account` scope already being active.
        activate(&mut conn, model.id(), ts(1_500), None).unwrap();

        let model_scope = CalibrationScope {
            window_semantic_key: WindowSemanticKey::new("model"),
            ..scope()
        };
        assert_eq!(
            load_active_at(&conn, &scope(), ts(2_000))
                .unwrap()
                .unwrap()
                .id(),
            account.id()
        );
        assert_eq!(
            load_active_at(&conn, &model_scope, ts(2_000))
                .unwrap()
                .unwrap()
                .id(),
            model.id()
        );
    }

    /// The inputs digest is stable under evidence reordering and changes when the
    /// evidence set changes, over many shuffled inputs.
    #[test]
    fn inputs_digest_is_order_independent_and_set_sensitive() {
        let base = evidence(&["alpha", "beta", "gamma", "delta"]);
        let base_digest = EvidenceDigest::from_inputs(&base);

        // Reordered insertion, same set: same digest.
        let mut state: u64 = 0x1234_5678;
        for _ in 0..64 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let mut shuffled: Vec<&str> = vec!["alpha", "beta", "gamma", "delta"];
            let rot = (state % 4) as usize;
            shuffled.rotate_left(rot);
            let reordered = evidence(&shuffled);
            assert_eq!(EvidenceDigest::from_inputs(&reordered), base_digest);
        }

        // A changed set changes the digest.
        assert_ne!(
            EvidenceDigest::from_inputs(&evidence(&["alpha", "beta", "gamma"])),
            base_digest
        );
        assert_ne!(
            EvidenceDigest::from_inputs(&evidence(&["alpha", "beta", "gamma", "epsilon"])),
            base_digest
        );
        assert!(base_digest.verify_expansion(&base));
    }

    /// A result with no source experiment is refused: nothing traceable stands behind
    /// its number.
    #[test]
    fn a_result_without_a_source_experiment_is_refused() {
        let (_scratch, mut conn) = fixture_conn();
        let cal = calibration("wc-orphan", interval(1, 9), 1, 900_000);
        assert!(insert_result(&mut conn, &cal, &[]).is_err());
        assert!(load_result(&conn, cal.id()).unwrap().is_none());
    }

    /// The excluded-sample encoding round-trips, including the empty list.
    #[test]
    fn excluded_sample_encoding_round_trips() {
        assert_eq!(decode_excluded(&encode_excluded(&[])).unwrap(), vec![]);
        let samples = vec![
            ExcludedSample::new("s-1", "outlier").unwrap(),
            ExcludedSample::new("s-2", "reset edge").unwrap(),
        ];
        assert_eq!(
            decode_excluded(&encode_excluded(&samples)).unwrap(),
            samples
        );
        assert!(ExcludedSample::new("s-3", "bad\nreason").is_err());
    }

    /// An inverted validity interval never reaches the database: the domain
    /// constructor rejects it first.
    #[test]
    fn an_inverted_validity_interval_is_rejected_by_the_constructor() {
        assert!(ValidityInterval::new(ts(200), ts(100)).is_err());
        assert!(
            CoefficientUncertainty::new(ppp(1_000), ppp(500)).is_err(),
            "an inverted uncertainty interval is rejected too"
        );
    }
}
