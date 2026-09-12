//! Multivariate calibration candidates: the joint per-kind fit recorded from a
//! controlled experiment's observations (`aub-73xa`).
//!
//! A candidate here is evidence and candidate generation, never truth: it is
//! written once, immutably, and nothing in this module activates anything.
//! The tables are `window_calibration_multivariate_candidate` and its child
//! `window_calibration_multivariate_coefficient`, one row per token kind the
//! experiment's premise named (migration 0039).
//!
//! May not depend on:
//! - transcripts
//! - presentation
//! - calibration (the store persists a fit; it never performs one)

use rusqlite::{Connection, OptionalExtension, params};

use crate::domain::provenance::EvidenceId;
use crate::domain::time::UtcTimestamp;
use crate::domain::tokens::TokenKind;
use crate::domain::window::WindowSemanticKey;
use crate::error::Error;
use crate::store::account::account_id_by_identity;
use crate::store::calibration::{ConditionNumber, EvidenceDigest, PlanTier, StoredFitObservation};
use crate::store::calibration_controlled::{ControlledExperimentId, ControlledExperimentRun};
use crate::store::cost_model::{ProviderKey, ValidityInterval};

/// The semantic identifier of one multivariate candidate, distinct from the
/// univariate `CandidateId` because the two live in different tables and a
/// reader that confused them would look a candidate up where it is not.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MultivariateCandidateId(String);

impl MultivariateCandidateId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One fitted coefficient as stored: quota parts per million per token of
/// `kind`, carried in micro-ppm so the row is an integer like every other
/// quantity column in the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredKindCoefficient {
    pub kind: TokenKind,
    pub estimate_micro_ppm_per_token: i64,
    pub std_error_micro_ppm_per_token: i64,
    pub interval_low_micro_ppm_per_token: i64,
    pub interval_high_micro_ppm_per_token: i64,
}

/// A recorded multivariate candidate: one coefficient per named kind plus the
/// identifiability figures the fit was accepted under, so the threshold that
/// admitted it is readable from the row rather than from a source constant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultivariateCandidate {
    pub id: MultivariateCandidateId,
    pub experiment: ControlledExperimentId,
    pub provider: ProviderKey,
    pub plan_tier: PlanTier,
    pub window_semantic_key: WindowSemanticKey,
    pub coefficients: Vec<StoredKindCoefficient>,
    pub condition_number: ConditionNumber,
    pub condition_number_threshold: ConditionNumber,
    pub fit_residual_ppm: i64,
    pub sample_count: u32,
    pub inputs: EvidenceDigest,
    pub statistical_method: String,
    pub statistical_parameters: String,
    pub phase_design: String,
    pub validity: ValidityInterval,
    pub knowledge_time: UtcTimestamp,
}

impl MultivariateCandidate {
    /// The kinds this candidate covers, in the order its coefficients were recorded.
    pub fn kinds(&self) -> Vec<TokenKind> {
        self.coefficients.iter().map(|c| c.kind).collect()
    }
}

fn resolve_run_db_id(conn: &Connection, id: &ControlledExperimentId) -> Result<i64, Error> {
    conn.query_row(
        "SELECT id FROM calibration_controlled_run WHERE experiment_id = ?1",
        params![id.as_str()],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .map_err(|e| {
        Error::Store(format!(
            "cannot resolve controlled experiment '{}': {e}",
            id.as_str()
        ))
    })?
    .ok_or_else(|| Error::Store(format!("no controlled experiment '{}'", id.as_str())))
}

fn kind_from_label(label: &str) -> Result<TokenKind, Error> {
    TokenKind::ALL
        .into_iter()
        .find(|kind| kind.label() == label)
        .ok_or_else(|| {
            Error::Store(format!(
                "stored coefficient names unknown token kind '{label}'"
            ))
        })
}

/// Inserts a candidate and its coefficient rows in one transaction. Fails if
/// the controlled run does not exist or the `candidate_id` is already taken.
pub fn insert_multivariate_candidate(
    conn: &mut Connection,
    candidate: &MultivariateCandidate,
) -> Result<i64, Error> {
    let run_db_id = resolve_run_db_id(conn, &candidate.experiment)?;
    let tx = conn.transaction().map_err(|e| {
        Error::Store(format!(
            "cannot open the multivariate candidate transaction: {e}"
        ))
    })?;
    let candidate_db_id = tx
        .query_row(
            "INSERT INTO window_calibration_multivariate_candidate (
                candidate_id, calibration_controlled_run_id, provider, plan_tier,
                window_semantic_key, condition_number_micros, condition_number_threshold_micros,
                fit_residual_ppm, sample_count, inputs_digest, inputs_count,
                statistical_method, statistical_parameters, phase_design,
                valid_from, valid_until, knowledge_time
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
            RETURNING id",
            params![
                candidate.id.as_str(),
                run_db_id,
                candidate.provider.as_str(),
                candidate.plan_tier.as_str(),
                candidate.window_semantic_key.as_str(),
                candidate.condition_number.micros(),
                candidate.condition_number_threshold.micros(),
                candidate.fit_residual_ppm,
                i64::from(candidate.sample_count),
                format!("{:016x}", candidate.inputs.digest()),
                i64::try_from(candidate.inputs.count())
                    .map_err(|_| Error::Store("inputs count out of i64 range".into()))?,
                candidate.statistical_method,
                candidate.statistical_parameters,
                candidate.phase_design,
                candidate.validity.valid_from().unix_nanos(),
                candidate.validity.valid_until().unix_nanos(),
                candidate.knowledge_time.unix_nanos(),
            ],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|e| {
            Error::Store(format!(
                "cannot insert the window_calibration_multivariate_candidate row: {e}"
            ))
        })?;
    for coefficient in &candidate.coefficients {
        tx.execute(
            "INSERT INTO window_calibration_multivariate_coefficient (
                window_calibration_multivariate_candidate_id, token_kind,
                estimate_micro_ppm_per_token, std_error_micro_ppm_per_token,
                interval_low_micro_ppm_per_token, interval_high_micro_ppm_per_token
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                candidate_db_id,
                coefficient.kind.label(),
                coefficient.estimate_micro_ppm_per_token,
                coefficient.std_error_micro_ppm_per_token,
                coefficient.interval_low_micro_ppm_per_token,
                coefficient.interval_high_micro_ppm_per_token,
            ],
        )
        .map_err(|e| {
            Error::Store(format!(
                "cannot insert the {} coefficient row: {e}",
                coefficient.kind.label()
            ))
        })?;
    }
    tx.commit()
        .map_err(|e| Error::Store(format!("cannot commit the multivariate candidate: {e}")))?;
    Ok(candidate_db_id)
}

/// Loads a candidate and its coefficients by semantic identifier.
pub fn load_multivariate_candidate(
    conn: &Connection,
    id: &MultivariateCandidateId,
) -> Result<Option<MultivariateCandidate>, Error> {
    let head = conn
        .query_row(
            "SELECT c.id, r.experiment_id, c.provider, c.plan_tier, c.window_semantic_key,
                    c.condition_number_micros, c.condition_number_threshold_micros,
                    c.fit_residual_ppm, c.sample_count, c.inputs_digest, c.inputs_count,
                    c.statistical_method, c.statistical_parameters, c.phase_design,
                    c.valid_from, c.valid_until, c.knowledge_time
             FROM window_calibration_multivariate_candidate c
             JOIN calibration_controlled_run r ON r.id = c.calibration_controlled_run_id
             WHERE c.candidate_id = ?1",
            params![id.as_str()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, String>(12)?,
                    row.get::<_, String>(13)?,
                    row.get::<_, i64>(14)?,
                    row.get::<_, i64>(15)?,
                    row.get::<_, i64>(16)?,
                ))
            },
        )
        .optional()
        .map_err(|e| {
            Error::Store(format!(
                "cannot load the window_calibration_multivariate_candidate row: {e}"
            ))
        })?;
    let Some((
        db_id,
        experiment,
        provider,
        plan_tier,
        window,
        condition_micros,
        threshold_micros,
        residual_ppm,
        sample_count,
        digest_hex,
        inputs_count,
        method,
        parameters,
        phase_design,
        valid_from,
        valid_until,
        knowledge_time,
    )) = head
    else {
        return Ok(None);
    };
    let digest = u64::from_str_radix(&digest_hex, 16)
        .map_err(|e| Error::Store(format!("stored inputs digest is not hex: {e}")))?;
    let inputs_count = usize::try_from(inputs_count)
        .map_err(|_| Error::Store("stored inputs count is negative".into()))?;
    let sample_count = u32::try_from(sample_count)
        .map_err(|_| Error::Store("stored sample count is out of u32 range".into()))?;
    let validity = ValidityInterval::new(
        UtcTimestamp::from_unix_nanos(valid_from),
        UtcTimestamp::from_unix_nanos(valid_until),
    )?;

    let mut statement = conn
        .prepare(
            "SELECT token_kind, estimate_micro_ppm_per_token, std_error_micro_ppm_per_token,
                    interval_low_micro_ppm_per_token, interval_high_micro_ppm_per_token
             FROM window_calibration_multivariate_coefficient
             WHERE window_calibration_multivariate_candidate_id = ?1
             ORDER BY id",
        )
        .map_err(|e| Error::Store(format!("cannot prepare the coefficient query: {e}")))?;
    let rows = statement
        .query_map(params![db_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(|e| Error::Store(format!("cannot read the coefficient rows: {e}")))?;
    let mut coefficients = Vec::new();
    for row in rows {
        let (label, estimate, std_error, low, high) =
            row.map_err(|e| Error::Store(format!("cannot read a coefficient row: {e}")))?;
        coefficients.push(StoredKindCoefficient {
            kind: kind_from_label(&label)?,
            estimate_micro_ppm_per_token: estimate,
            std_error_micro_ppm_per_token: std_error,
            interval_low_micro_ppm_per_token: low,
            interval_high_micro_ppm_per_token: high,
        });
    }

    Ok(Some(MultivariateCandidate {
        id: id.clone(),
        experiment: ControlledExperimentId::new(experiment),
        provider: ProviderKey::new(provider),
        plan_tier: PlanTier::new(plan_tier),
        window_semantic_key: WindowSemanticKey::new(window),
        coefficients,
        condition_number: ConditionNumber::from_micros(condition_micros),
        condition_number_threshold: ConditionNumber::from_micros(threshold_micros),
        fit_residual_ppm: residual_ppm,
        sample_count,
        inputs: EvidenceDigest::from_parts(digest, inputs_count),
        statistical_method: method,
        statistical_parameters: parameters,
        phase_design,
        validity,
        knowledge_time: UtcTimestamp::from_unix_nanos(knowledge_time),
    }))
}

/// How many multivariate candidates the ledger holds. A refused fit records
/// nothing, and this is the count that proves it.
pub fn count_multivariate_candidates(conn: &Connection) -> Result<i64, Error> {
    conn.query_row(
        "SELECT COUNT(*) FROM window_calibration_multivariate_candidate",
        [],
        |row| row.get(0),
    )
    .map_err(|e| {
        Error::Store(format!(
            "cannot count window_calibration_multivariate_candidate rows: {e}"
        ))
    })
}

/// The meter observations of the run's own account and target window from
/// the baseline reading up to `until`, oldest first, in the shape the fitter
/// consumes. Filtered on the account rather than the provider alone, because
/// another account on the same provider is exactly the contamination the
/// exclusivity assertion was written to exclude.
pub fn observations_for_run(
    conn: &Connection,
    run: &ControlledExperimentRun,
    until: UtcTimestamp,
) -> Result<Vec<StoredFitObservation>, Error> {
    let Some(account_id) = account_id_by_identity(conn, run.provider.as_str(), &run.account)?
    else {
        return Ok(Vec::new());
    };
    let mut statement = conn
        .prepare(
            "SELECT re.content_hash, mo.received_at, mw.quota_used_ppm,
                    mw.reported_resolution_ppm, mw.quantization, mw.resets_at
             FROM meter_observation mo
             JOIN meter_window mw ON mw.observation_id = mo.id
             JOIN meter_response_evidence re ON re.id = mo.evidence_id
             WHERE mo.account_id = ?1
               AND mw.semantic_key = ?2
               AND mo.received_at >= ?3
               AND mo.received_at <= ?4
             ORDER BY mo.received_at ASC, mo.id ASC",
        )
        .map_err(|e| Error::Store(format!("cannot prepare the run observations query: {e}")))?;
    let rows = statement
        .query_map(
            params![
                account_id.value(),
                run.window_semantic_key.as_str(),
                run.baseline_observed_at.unix_nanos(),
                until.unix_nanos(),
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                ))
            },
        )
        .map_err(|e| Error::Store(format!("cannot query the run observations: {e}")))?;
    let mut observations = Vec::new();
    for row in rows {
        let (hash, received_at, quota_used_ppm, reported_resolution_ppm, quantization, resets_at) =
            row.map_err(|e| Error::Store(format!("cannot read a run observation row: {e}")))?;
        observations.push(StoredFitObservation {
            evidence_id: EvidenceId::new(hash),
            at: UtcTimestamp::from_unix_nanos(received_at),
            quota_used_ppm,
            reported_resolution_ppm,
            quantization: crate::store::meter_evidence::quantization_sql::from_sql(&quantization)?,
            resets_at: UtcTimestamp::from_unix_nanos(resets_at.unwrap_or(0)),
        });
    }
    Ok(observations)
}
