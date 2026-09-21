//! Per-kind window calibration results and their lifecycle
//! (`aub-multivariate-result-shape-2hvt`, migration 0044).
//!
//! A per-kind result is a promoted joint candidate: one coefficient per token
//! kind with its interval, the condition number the fit was accepted under, and
//! the held-out residual over evidence disjoint from the fit. It is never
//! reduced to one credits-per-point scalar, because that reduction would record
//! as truth the cost-model assumption the joint fit was run to test (PLAN.md
//! 22.1).
//!
//! Its activation events live in `calibration_multivariate_lifecycle`, and the
//! scope's active calibration is decided across both lifecycle tables by
//! [`crate::store::calibration::load_active_at`].
//!
//! May not depend on:
//! - transcripts
//! - presentation

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::calibration::activation::{ActivationRequest, HeldOutResidual, RecordedValidation};
use crate::domain::provenance::WindowCalibrationId;
use crate::domain::quota::QuotaFractionPpm;
use crate::domain::time::UtcTimestamp;
use crate::domain::tokens::TokenKind;
use crate::domain::window::WindowSemanticKey;
use crate::error::Error;
use crate::store::calibration::{
    ActivationEvent, ActiveCalibration, CalibrationEventKind, CalibrationLifecycleEventId,
    CalibrationScope, ConditionNumber, EvidenceDigest, EvidenceFingerprint, PlanTier,
};
use crate::store::calibration_controlled::ControlledExperimentId;
use crate::store::calibration_multivariate::{MultivariateCandidateId, StoredKindCoefficient};
use crate::store::cost_model::{ProviderKey, ValidityInterval};

/// A validated per-kind window calibration: everything the joint candidate
/// recorded, plus the validation half a candidate does not carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultivariateCalibration {
    pub id: WindowCalibrationId,
    pub candidate: MultivariateCandidateId,
    pub experiment: ControlledExperimentId,
    pub provider: ProviderKey,
    pub plan_tier: PlanTier,
    pub window_semantic_key: WindowSemanticKey,
    pub coefficients: Vec<StoredKindCoefficient>,
    pub condition_number: ConditionNumber,
    pub condition_number_threshold: ConditionNumber,
    /// The in-sample mean absolute block residual, in quota parts per million.
    pub fit_residual: QuotaFractionPpm,
    /// The mean absolute block residual over the validation evidence.
    pub held_out_residual: QuotaFractionPpm,
    pub validation_observations: u32,
    pub sample_count: u32,
    pub inputs: EvidenceDigest,
    pub fitting_evidence: EvidenceFingerprint,
    pub validation_evidence: EvidenceFingerprint,
    pub validation_method: String,
    pub validation_version: String,
    pub statistical_method: String,
    pub statistical_parameters: String,
    pub phase_design: String,
    pub activation_policy_version: String,
    pub aub_version: String,
    pub source_revision: String,
    pub validity: ValidityInterval,
    /// When the joint fit was recorded.
    pub fit_timestamp: UtcTimestamp,
    /// When this result was recorded.
    pub knowledge_time: UtcTimestamp,
}

impl MultivariateCalibration {
    pub fn scope(&self) -> CalibrationScope {
        CalibrationScope {
            provider: self.provider.clone(),
            plan_tier: self.plan_tier.clone(),
            window_semantic_key: self.window_semantic_key.clone(),
        }
    }

    /// What activation judges: the recorded policy version, the held-out
    /// residual in its own unit, the condition number and both fingerprints.
    pub fn recorded_validation(&self) -> RecordedValidation {
        RecordedValidation {
            policy_version: self.activation_policy_version.clone(),
            held_out_residual: Some(HeldOutResidual::QuotaPpm(self.held_out_residual)),
            condition_number: Some(self.condition_number),
            fitting_evidence: self.fitting_evidence,
            validation_evidence: self.validation_evidence,
        }
    }
}

fn store_error(context: &str) -> impl Fn(rusqlite::Error) -> Error + '_ {
    move |e| Error::Store(format!("{context}: {e}"))
}

fn ppm_from_row(value: i64, field: &str) -> Result<QuotaFractionPpm, Error> {
    i32::try_from(value)
        .ok()
        .and_then(QuotaFractionPpm::new)
        .ok_or_else(|| Error::Store(format!("stored {field} {value} is not a quota fraction")))
}

fn candidate_db_id(conn: &Connection, id: &MultivariateCandidateId) -> Result<i64, Error> {
    conn.query_row(
        "SELECT id FROM window_calibration_multivariate_candidate WHERE candidate_id = ?1",
        params![id.as_str()],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .map_err(store_error("cannot resolve the multivariate candidate"))?
    .ok_or_else(|| Error::Store(format!("no multivariate candidate '{}'", id.as_str())))
}

/// Inserts a per-kind result and one coefficient row per kind in one
/// transaction. The database refuses an id a scalar result already holds and a
/// second result for one candidate.
pub fn insert_multivariate_result(
    conn: &mut Connection,
    result: &MultivariateCalibration,
) -> Result<i64, Error> {
    let candidate_row = candidate_db_id(conn, &result.candidate)?;
    let tx = conn
        .transaction()
        .map_err(store_error("cannot open the per-kind result transaction"))?;
    let result_row = tx
        .query_row(
            "INSERT INTO window_calibration_multivariate_result (
                calibration_id, window_calibration_multivariate_candidate_id, provider,
                plan_tier, window_semantic_key, condition_number_micros,
                condition_number_threshold_micros, fit_residual_ppm, held_out_residual_ppm,
                validation_observation_count, sample_count, inputs_digest, inputs_count,
                fitting_evidence_digest, validation_evidence_digest, validation_method,
                validation_version, statistical_method, statistical_parameters, phase_design,
                activation_policy_version, aub_version, source_revision, valid_from,
                valid_until, knowledge_time
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)
            RETURNING id",
            params![
                result.id.as_str(),
                candidate_row,
                result.provider.as_str(),
                result.plan_tier.as_str(),
                result.window_semantic_key.as_str(),
                result.condition_number.micros(),
                result.condition_number_threshold.micros(),
                i64::from(result.fit_residual.get()),
                i64::from(result.held_out_residual.get()),
                i64::from(result.validation_observations),
                i64::from(result.sample_count),
                format!("{:016x}", result.inputs.digest()),
                i64::try_from(result.inputs.count())
                    .map_err(|_| Error::Store("inputs count out of i64 range".into()))?,
                format!("{:016x}", result.fitting_evidence.as_u64()),
                format!("{:016x}", result.validation_evidence.as_u64()),
                result.validation_method,
                result.validation_version,
                result.statistical_method,
                result.statistical_parameters,
                result.phase_design,
                result.activation_policy_version,
                result.aub_version,
                result.source_revision,
                result.validity.valid_from().unix_nanos(),
                result.validity.valid_until().unix_nanos(),
                result.knowledge_time.unix_nanos(),
            ],
            |row| row.get::<_, i64>(0),
        )
        .map_err(store_error(
            "cannot insert the window_calibration_multivariate_result row",
        ))?;
    for coefficient in &result.coefficients {
        tx.execute(
            "INSERT INTO window_calibration_multivariate_result_coefficient (
                window_calibration_multivariate_result_id, token_kind,
                estimate_micro_ppm_per_token, std_error_micro_ppm_per_token,
                interval_low_micro_ppm_per_token, interval_high_micro_ppm_per_token
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                result_row,
                coefficient.kind.label(),
                coefficient.estimate_micro_ppm_per_token,
                coefficient.std_error_micro_ppm_per_token,
                coefficient.interval_low_micro_ppm_per_token,
                coefficient.interval_high_micro_ppm_per_token,
            ],
        )
        .map_err(|e| {
            Error::Store(format!(
                "cannot insert the {} result coefficient row: {e}",
                coefficient.kind.label()
            ))
        })?;
    }
    tx.commit()
        .map_err(store_error("cannot commit the per-kind result"))?;
    Ok(result_row)
}

const RESULT_COLUMNS: &str = "r.id, r.calibration_id, c.candidate_id, run.experiment_id, \
     r.provider, r.plan_tier, r.window_semantic_key, r.condition_number_micros, \
     r.condition_number_threshold_micros, r.fit_residual_ppm, r.held_out_residual_ppm, \
     r.validation_observation_count, r.sample_count, r.inputs_digest, r.inputs_count, \
     r.fitting_evidence_digest, r.validation_evidence_digest, r.validation_method, \
     r.validation_version, r.statistical_method, r.statistical_parameters, r.phase_design, \
     r.activation_policy_version, r.aub_version, r.source_revision, r.valid_from, \
     r.valid_until, c.knowledge_time, r.knowledge_time";

const RESULT_FROM: &str = "FROM window_calibration_multivariate_result r \
     JOIN window_calibration_multivariate_candidate c \
       ON c.id = r.window_calibration_multivariate_candidate_id \
     JOIN calibration_controlled_run run ON run.id = c.calibration_controlled_run_id";

fn digest_from_hex(text: &str) -> Result<u64, Error> {
    u64::from_str_radix(text, 16)
        .map_err(|e| Error::Store(format!("malformed evidence digest '{text}': {e}")))
}

fn count_from_row(value: i64, field: &str) -> Result<u32, Error> {
    u32::try_from(value).map_err(|_| Error::Store(format!("stored {field} is out of u32 range")))
}

/// The head row without its coefficients, plus its database id.
fn head_from_row(row: &Row<'_>) -> rusqlite::Result<(i64, [i64; 11], [String; 17])> {
    Ok((
        row.get(0)?,
        [
            row.get(7)?,
            row.get(8)?,
            row.get(9)?,
            row.get(10)?,
            row.get(11)?,
            row.get(12)?,
            row.get(14)?,
            row.get(25)?,
            row.get(26)?,
            row.get(27)?,
            row.get(28)?,
        ],
        [
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
            row.get(13)?,
            row.get(15)?,
            row.get(16)?,
            row.get(17)?,
            row.get(18)?,
            row.get(19)?,
            row.get(20)?,
            row.get(21)?,
            row.get(22)?,
            row.get(23)?,
            row.get(24)?,
        ],
    ))
}

fn kind_from_label(label: &str) -> Result<TokenKind, Error> {
    TokenKind::ALL
        .into_iter()
        .find(|kind| kind.label() == label)
        .ok_or_else(|| Error::Store(format!("stored result names unknown token kind '{label}'")))
}

fn load_coefficients(
    conn: &Connection,
    result_row: i64,
) -> Result<Vec<StoredKindCoefficient>, Error> {
    let mut statement = conn
        .prepare(
            "SELECT token_kind, estimate_micro_ppm_per_token, std_error_micro_ppm_per_token,
                    interval_low_micro_ppm_per_token, interval_high_micro_ppm_per_token
             FROM window_calibration_multivariate_result_coefficient
             WHERE window_calibration_multivariate_result_id = ?1
             ORDER BY id",
        )
        .map_err(store_error("cannot prepare the result coefficient query"))?;
    let rows = statement
        .query_map(params![result_row], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(store_error("cannot read the result coefficient rows"))?;
    let mut coefficients = Vec::new();
    for row in rows {
        let (label, estimate, std_error, low, high) =
            row.map_err(store_error("cannot read a result coefficient row"))?;
        coefficients.push(StoredKindCoefficient {
            kind: kind_from_label(&label)?,
            estimate_micro_ppm_per_token: estimate,
            std_error_micro_ppm_per_token: std_error,
            interval_low_micro_ppm_per_token: low,
            interval_high_micro_ppm_per_token: high,
        });
    }
    Ok(coefficients)
}

fn assemble(
    conn: &Connection,
    (row_id, numbers, texts): (i64, [i64; 11], [String; 17]),
) -> Result<MultivariateCalibration, Error> {
    let [
        condition,
        threshold,
        fit_residual,
        held_out,
        validation_count,
        sample_count,
        inputs_count,
        valid_from,
        valid_until,
        fit_timestamp,
        knowledge_time,
    ] = numbers;
    let [
        id,
        candidate,
        experiment,
        provider,
        plan_tier,
        window,
        inputs_digest,
        fitting_digest,
        validation_digest,
        validation_method,
        validation_version,
        statistical_method,
        statistical_parameters,
        phase_design,
        activation_policy_version,
        aub_version,
        source_revision,
    ] = texts;
    Ok(MultivariateCalibration {
        id: WindowCalibrationId::new(id),
        candidate: MultivariateCandidateId::new(candidate),
        experiment: ControlledExperimentId::new(experiment),
        provider: ProviderKey::new(provider),
        plan_tier: PlanTier::new(plan_tier),
        window_semantic_key: WindowSemanticKey::new(window),
        coefficients: load_coefficients(conn, row_id)?,
        condition_number: ConditionNumber::from_micros(condition),
        condition_number_threshold: ConditionNumber::from_micros(threshold),
        fit_residual: ppm_from_row(fit_residual, "fit residual")?,
        held_out_residual: ppm_from_row(held_out, "held-out residual")?,
        validation_observations: count_from_row(validation_count, "validation count")?,
        sample_count: count_from_row(sample_count, "sample count")?,
        inputs: EvidenceDigest::from_parts(
            digest_from_hex(&inputs_digest)?,
            usize::try_from(inputs_count)
                .map_err(|_| Error::Store("stored inputs count is negative".into()))?,
        ),
        fitting_evidence: EvidenceFingerprint::from_raw(digest_from_hex(&fitting_digest)?),
        validation_evidence: EvidenceFingerprint::from_raw(digest_from_hex(&validation_digest)?),
        validation_method,
        validation_version,
        statistical_method,
        statistical_parameters,
        phase_design,
        activation_policy_version,
        aub_version,
        source_revision,
        validity: ValidityInterval::new(
            UtcTimestamp::from_unix_nanos(valid_from),
            UtcTimestamp::from_unix_nanos(valid_until),
        )?,
        fit_timestamp: UtcTimestamp::from_unix_nanos(fit_timestamp),
        knowledge_time: UtcTimestamp::from_unix_nanos(knowledge_time),
    })
}

fn load_where(
    conn: &Connection,
    condition: &str,
    value: &dyn rusqlite::ToSql,
) -> Result<Option<MultivariateCalibration>, Error> {
    let head = conn
        .query_row(
            &format!("SELECT {RESULT_COLUMNS} {RESULT_FROM} WHERE {condition}"),
            [value],
            head_from_row,
        )
        .optional()
        .map_err(store_error(
            "cannot load the window_calibration_multivariate_result row",
        ))?;
    head.map(|head| assemble(conn, head)).transpose()
}

/// Loads a per-kind result by its calibration id.
pub fn load_multivariate_result(
    conn: &Connection,
    id: &WindowCalibrationId,
) -> Result<Option<MultivariateCalibration>, Error> {
    load_where(conn, "r.calibration_id = ?1", &id.as_str())
}

/// The per-kind result promoted from `candidate`, if one was.
pub fn load_multivariate_result_for_candidate(
    conn: &Connection,
    candidate: &MultivariateCandidateId,
) -> Result<Option<MultivariateCalibration>, Error> {
    load_where(conn, "c.candidate_id = ?1", &candidate.as_str())
}

/// Loads a per-kind result by its row id, for the active-calibration lookup.
pub(crate) fn load_multivariate_result_by_row(
    conn: &Connection,
    row_id: i64,
) -> Result<Option<MultivariateCalibration>, Error> {
    load_where(conn, "r.id = ?1", &row_id)
}

/// Every per-kind result, ordered by knowledge time then row id, as
/// `calibrate history` lists them.
pub fn list_all_multivariate_results(
    conn: &Connection,
) -> Result<Vec<MultivariateCalibration>, Error> {
    let mut statement = conn
        .prepare(&format!(
            "SELECT {RESULT_COLUMNS} {RESULT_FROM} ORDER BY r.knowledge_time, r.id"
        ))
        .map_err(store_error("cannot prepare the per-kind history query"))?;
    let heads = statement
        .query_map([], head_from_row)
        .map_err(store_error("cannot query the per-kind history"))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_error("cannot read a per-kind result row"))?;
    heads.into_iter().map(|head| assemble(conn, head)).collect()
}

/// Every lifecycle event of one per-kind result, in event order. A predecessor
/// is named by its calibration id whichever shape it was.
pub fn multivariate_activation_events_for(
    conn: &Connection,
    id: &WindowCalibrationId,
) -> Result<Vec<ActivationEvent>, Error> {
    let mut statement = conn
        .prepare(
            "SELECT l.event_kind, l.event_at, l.actor, l.activation_policy_version,
                    l.fitting_evidence_digest, l.validation_evidence_digest,
                    COALESCE(s.calibration_id, sm.calibration_id)
             FROM calibration_multivariate_lifecycle l
             JOIN window_calibration_multivariate_result r
               ON r.id = l.window_calibration_multivariate_result_id
             LEFT JOIN window_calibration_result s ON s.id = l.supersedes_result_id
             LEFT JOIN window_calibration_multivariate_result sm
               ON sm.id = l.supersedes_multivariate_result_id
             WHERE r.calibration_id = ?1
             ORDER BY l.event_at, l.id",
        )
        .map_err(store_error("cannot prepare the per-kind lifecycle query"))?;
    let rows = statement
        .query_map(params![id.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })
        .map_err(store_error("cannot query the per-kind lifecycle"))?;
    let mut events = Vec::new();
    for row in rows {
        let (kind, event_at, actor, policy, fitting, validation, supersedes) =
            row.map_err(store_error("cannot read a per-kind lifecycle row"))?;
        events.push(ActivationEvent {
            kind: CalibrationEventKind::from_kind_label(&kind)?,
            event_at: UtcTimestamp::from_unix_nanos(event_at),
            actor: crate::calibration::activation::ActivationActor::new(actor)
                .map_err(|e| Error::Store(format!("stored activation actor is invalid: {e}")))?,
            activation_policy_version: policy,
            fitting_evidence: EvidenceFingerprint::from_raw(digest_from_hex(&fitting)?),
            validation_evidence: EvidenceFingerprint::from_raw(digest_from_hex(&validation)?),
            supersedes: supersedes.map(WindowCalibrationId::new),
        });
    }
    Ok(events)
}

/// Whether a later per-kind activation names this per-kind result as its
/// predecessor. A scalar activation never does: the store refuses one over an
/// active per-kind calibration.
pub fn is_multivariate_superseded(
    conn: &Connection,
    id: &WindowCalibrationId,
) -> Result<bool, Error> {
    conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM calibration_multivariate_lifecycle
            WHERE supersedes_multivariate_result_id = (
                SELECT id FROM window_calibration_multivariate_result WHERE calibration_id = ?1
            )
        )",
        params![id.as_str()],
        |row| row.get(0),
    )
    .map_err(store_error("cannot query per-kind supersession"))
}

/// Records `id` becoming the active calibration for its scope at `event_at`,
/// through the same gate a scalar activation passes: the request must judge
/// the evidence the result recorded, disjointly, with no contamination
/// standing, the condition number and the held-out residual within the policy
/// bounds. The predecessor rule is the scalar one, read across both shapes.
///
/// No cost-model completeness check runs here: that rule governs a coefficient
/// priced through a cost model (PLAN.md 23.8), and a per-kind result references
/// none.
pub fn activate_multivariate(
    conn: &mut Connection,
    id: &WindowCalibrationId,
    event_at: UtcTimestamp,
    supersedes: Option<&WindowCalibrationId>,
    request: &ActivationRequest<'_>,
) -> Result<CalibrationLifecycleEventId, Error> {
    use crate::calibration::activation::check_activation;
    use crate::store::calibration::{check_predecessor, instant_before, load_active_at};
    let tx = conn.transaction().map_err(store_error(
        "cannot open the per-kind activation transaction",
    ))?;
    let calibration = load_multivariate_result(&tx, id)?.ok_or_else(|| {
        Error::Store(format!(
            "no per-kind window calibration '{}' to activate",
            id.as_str()
        ))
    })?;
    check_activation(request, &calibration.recorded_validation())
        .map_err(|refusal| refusal.into_error())?;

    let active_before = load_active_at(&tx, &calibration.scope(), instant_before(event_at))?;
    check_predecessor(active_before.as_ref(), id, supersedes)?;
    let (scalar_predecessor, joint_predecessor) = match &active_before {
        None => (None, None),
        Some(ActiveCalibration::Scalar(active)) => {
            (Some(row_id_of_scalar(&tx, active.id())?), None)
        }
        Some(ActiveCalibration::PerKind(active)) => {
            (None, Some(row_id_of_multivariate(&tx, &active.id)?))
        }
    };
    let kind = if active_before.is_some() {
        CalibrationEventKind::Supersession
    } else {
        CalibrationEventKind::Activation
    };
    let event_id = tx
        .query_row(
            "INSERT INTO calibration_multivariate_lifecycle (
                window_calibration_multivariate_result_id, event_kind, event_at,
                supersedes_result_id, supersedes_multivariate_result_id, actor,
                activation_policy_version, fitting_evidence_digest, validation_evidence_digest
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) RETURNING id",
            params![
                row_id_of_multivariate(&tx, id)?,
                kind.as_str(),
                event_at.unix_nanos(),
                scalar_predecessor,
                joint_predecessor,
                request.actor.as_str(),
                request.policy.version(),
                format!("{:016x}", calibration.fitting_evidence.as_u64()),
                format!("{:016x}", calibration.validation_evidence.as_u64()),
            ],
            |row| row.get::<_, i64>(0),
        )
        .map_err(store_error(
            "cannot insert the calibration_multivariate_lifecycle event",
        ))?;
    tx.commit()
        .map_err(store_error("cannot commit the per-kind activation"))?;
    Ok(CalibrationLifecycleEventId::from_raw(event_id))
}

fn row_id_of_scalar(conn: &Connection, id: &WindowCalibrationId) -> Result<i64, Error> {
    conn.query_row(
        "SELECT id FROM window_calibration_result WHERE calibration_id = ?1",
        params![id.as_str()],
        |row| row.get(0),
    )
    .map_err(|e| Error::Store(format!("cannot resolve calibration '{}': {e}", id.as_str())))
}

fn row_id_of_multivariate(conn: &Connection, id: &WindowCalibrationId) -> Result<i64, Error> {
    conn.query_row(
        "SELECT id FROM window_calibration_multivariate_result WHERE calibration_id = ?1",
        params![id.as_str()],
        |row| row.get(0),
    )
    .map_err(|e| Error::Store(format!("cannot resolve calibration '{}': {e}", id.as_str())))
}

/// A joint candidate and its per-kind result in one scope, shared by this
/// module's tests and by the tests of the consumers that must match the
/// per-kind arm of [`ActiveCalibration`].
#[cfg(test)]
pub(crate) mod per_kind_fixture {
    use super::*;
    use std::collections::BTreeSet;

    use crate::calibration::activation::{ActivationActor, ActivationPolicy};
    use crate::calibration::contamination::{ContaminationThresholds, ContaminationVerdict};
    use crate::domain::credits::Credits;
    use crate::domain::provenance::{CostModelId, EvidenceId};
    use crate::domain::quota::QuotaUsed;
    use crate::domain::window::ReportedResolution;
    use crate::store::calibration_controlled::{ControlledExperimentRun, insert_begin};
    use crate::store::calibration_multivariate::{
        MultivariateCandidate, insert_multivariate_candidate,
    };
    use crate::store::meter_evidence::ObservationRowId;

    pub(crate) const RESULT_ID: &str = "promoted-mvcand-1";

    pub(crate) fn ts(nanos: i64) -> UtcTimestamp {
        UtcTimestamp::from_unix_nanos(nanos)
    }

    pub(crate) fn evidence(tag: &str) -> BTreeSet<EvidenceId> {
        [EvidenceId::new(tag)].into_iter().collect()
    }

    fn coefficient(kind: TokenKind, estimate: i64) -> StoredKindCoefficient {
        StoredKindCoefficient {
            kind,
            estimate_micro_ppm_per_token: estimate,
            std_error_micro_ppm_per_token: 1_000,
            interval_low_micro_ppm_per_token: estimate - 2_000,
            interval_high_micro_ppm_per_token: estimate + 2_000,
        }
    }

    fn coefficients() -> Vec<StoredKindCoefficient> {
        vec![
            coefficient(TokenKind::Input, 500_000),
            coefficient(TokenKind::Output, 1_000_000),
        ]
    }

    /// Seeds, in anthropic's `five_hour` window under `plan_tier`, one
    /// controlled run on account `acct` and its joint candidate `mvcand-1`.
    pub(crate) fn seed_run_and_candidate(conn: &mut Connection, plan_tier: &str) {
        crate::store::calibrate_cli_test_ledger::insert_calibrate_cli_meter_chain(
            conn,
            "acct",
            "five_hour",
            ts(1_000),
            10_000,
        );
        let baseline: i64 = conn
            .query_row("SELECT id FROM meter_observation LIMIT 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        let resolution = QuotaFractionPpm::new(10_000).unwrap();
        let run = ControlledExperimentRun {
            id: ControlledExperimentId::new("exp-joint"),
            account: "acct".to_string(),
            provider: ProviderKey::new("anthropic"),
            plan_tier: PlanTier::new(plan_tier),
            window_semantic_key: WindowSemanticKey::new("five_hour"),
            cost_model_id: CostModelId::new("cm"),
            expected_token_kinds: vec![TokenKind::Input, TokenKind::Output],
            baseline_observation_id: ObservationRowId::new(baseline),
            baseline_quota_used: QuotaUsed::new(resolution),
            baseline_resolution: ReportedResolution::new(resolution).unwrap(),
            baseline_observed_at: ts(1_000),
            baseline_plateau_started_at: ts(1_000),
            contamination_thresholds: ContaminationThresholds::conservative_default(),
            started_at: ts(1_000),
            ended_at: None,
            exclusivity_assertion: "reserved".to_string(),
        };
        insert_begin(conn, &run).unwrap();
        insert_multivariate_candidate(
            conn,
            &MultivariateCandidate {
                id: MultivariateCandidateId::new("mvcand-1"),
                experiment: run.id.clone(),
                provider: run.provider.clone(),
                plan_tier: run.plan_tier.clone(),
                window_semantic_key: run.window_semantic_key.clone(),
                coefficients: coefficients(),
                condition_number: ConditionNumber::from_micros(4_000_000),
                condition_number_threshold: ConditionNumber::from_micros(30_000_000),
                fit_residual_ppm: 120,
                sample_count: 8,
                inputs: EvidenceDigest::from_inputs(&evidence("training")),
                statistical_method: "ols".to_string(),
                statistical_parameters: "{}".to_string(),
                phase_design: "design".to_string(),
                validity: ValidityInterval::new(ts(1_000), ts(2_000)).unwrap(),
                knowledge_time: ts(2_000),
            },
        )
        .unwrap();
    }

    /// A per-kind result of `mvcand-1` under `plan_tier`, named `id`.
    pub(crate) fn result(id: &str, plan_tier: &str) -> MultivariateCalibration {
        MultivariateCalibration {
            id: WindowCalibrationId::new(id),
            candidate: MultivariateCandidateId::new("mvcand-1"),
            experiment: ControlledExperimentId::new("exp-joint"),
            provider: ProviderKey::new("anthropic"),
            plan_tier: PlanTier::new(plan_tier),
            window_semantic_key: WindowSemanticKey::new("five_hour"),
            coefficients: coefficients(),
            condition_number: ConditionNumber::from_micros(4_000_000),
            condition_number_threshold: ConditionNumber::from_micros(30_000_000),
            fit_residual: QuotaFractionPpm::new(120).unwrap(),
            held_out_residual: QuotaFractionPpm::new(340).unwrap(),
            validation_observations: 2,
            sample_count: 8,
            inputs: EvidenceDigest::from_inputs(&evidence("training")),
            fitting_evidence: EvidenceFingerprint::from_inputs(&evidence("training")),
            validation_evidence: EvidenceFingerprint::from_inputs(&evidence("validation")),
            validation_method: "held-out-block-residual".to_string(),
            validation_version: "v1".to_string(),
            statistical_method: "ols".to_string(),
            statistical_parameters: "{}".to_string(),
            phase_design: "design".to_string(),
            activation_policy_version: "promote-v1".to_string(),
            aub_version: "0.1.0".to_string(),
            source_revision: "abc1234".to_string(),
            validity: ValidityInterval::new(ts(1_000), ts(2_000)).unwrap(),
            fit_timestamp: ts(2_000),
            knowledge_time: ts(2_500),
        }
    }

    /// Activates [`RESULT_ID`] at `at` through the real gate.
    pub(crate) fn activate(
        conn: &mut Connection,
        at: i64,
        supersedes: Option<&WindowCalibrationId>,
    ) -> Result<CalibrationLifecycleEventId, Error> {
        let actor = ActivationActor::new("operator").unwrap();
        let policy = ActivationPolicy::new(
            "promote-v1",
            Credits::from_micros(0),
            ConditionNumber::from_micros(30_000_000),
        )
        .unwrap()
        .with_max_quota_residual(QuotaFractionPpm::new(10_000).unwrap());
        let verdict = ContaminationVerdict::clean();
        activate_multivariate(
            conn,
            &WindowCalibrationId::new(RESULT_ID),
            ts(at),
            supersedes,
            &ActivationRequest {
                actor: &actor,
                policy: &policy,
                training: &evidence("training"),
                validation: &evidence("validation"),
                contamination: &verdict,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::per_kind_fixture::{activate, evidence, result, seed_run_and_candidate, ts};
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::calibration::activation::{ActivationActor, ActivationPolicy};
    use crate::calibration::contamination::ContaminationVerdict;
    use crate::domain::credits::{Credits, CreditsPerPercentagePoint};
    use crate::domain::ids::{BillingSemanticsId, MeterSemanticsId};
    use crate::domain::provenance::CostModelId;
    use crate::domain::time::MonotonicDuration;
    use crate::store::calibration::{
        insert_experiment, insert_result, is_superseded, lifecycle_event_count, load_active_at,
        minimal_experiment, minimal_fixture,
    };
    use crate::store::connection::PragmaPolicy;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scope() -> CalibrationScope {
        CalibrationScope {
            provider: ProviderKey::new("anthropic"),
            plan_tier: PlanTier::new("pro"),
            window_semantic_key: WindowSemanticKey::new("five_hour"),
        }
    }

    /// A migrated ledger holding one ended controlled run, its joint
    /// candidate `mvcand-1`, and one promoted-but-inactive scalar result
    /// `scalar-1` in the same scope.
    fn ledger() -> (Scratch, Connection) {
        let dir = std::env::temp_dir().join(format!(
            "aub-per-kind-result-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let mut conn = crate::store::test_schema::open_migrated(
            &dir.join("ledger.db"),
            &PragmaPolicy {
                busy_timeout: MonotonicDuration::from_millis(1_000),
            },
        );
        seed_run_and_candidate(&mut conn, "pro");
        let validity = ValidityInterval::new(ts(1_000), ts(2_000)).unwrap();

        let meter = MeterSemanticsId::new("meter-v1");
        let billing = BillingSemanticsId::new("billing-v1");
        insert_experiment(
            &conn,
            &minimal_experiment(
                "exp-scalar",
                &scope(),
                &meter,
                &billing,
                validity,
                ts(2_000),
            ),
        )
        .unwrap();
        let scalar = minimal_fixture(
            "scalar-1",
            &scope(),
            &meter,
            &billing,
            &CostModelId::new("cm-v1"),
            CreditsPerPercentagePoint::from_micros_per_point(100_000),
            validity,
            ts(2_000),
        );
        insert_result(
            &mut conn,
            &scalar,
            &[crate::store::calibration::ExperimentId::new("exp-scalar")],
        )
        .unwrap();
        (Scratch(dir), conn)
    }

    fn per_kind(id: &str) -> MultivariateCalibration {
        result(id, "pro")
    }

    fn activate_scalar(
        conn: &mut Connection,
        at: i64,
        supersedes: Option<&WindowCalibrationId>,
    ) -> Result<CalibrationLifecycleEventId, Error> {
        let actor = ActivationActor::new("fixture").unwrap();
        let policy = ActivationPolicy::new(
            "fixture",
            Credits::from_micros(0),
            ConditionNumber::from_micros(30_000_000),
        )
        .unwrap();
        let verdict = ContaminationVerdict::clean();
        crate::store::calibration::activate(
            conn,
            &WindowCalibrationId::new("scalar-1"),
            ts(at),
            supersedes,
            &ActivationRequest {
                actor: &actor,
                policy: &policy,
                training: &evidence("fixture:fitting"),
                validation: &evidence("fixture:validation"),
                contamination: &verdict,
            },
        )
    }

    fn activate_per_kind(
        conn: &mut Connection,
        at: i64,
        supersedes: Option<&WindowCalibrationId>,
    ) -> Result<CalibrationLifecycleEventId, Error> {
        activate(conn, at, supersedes)
    }

    fn per_kind_events(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM calibration_multivariate_lifecycle",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// Every figure written reads back, the coefficients in insertion order.
    #[test]
    fn a_per_kind_result_round_trips_with_its_coefficients() {
        let (_scratch, mut conn) = ledger();
        let result = per_kind("promoted-mvcand-1");
        insert_multivariate_result(&mut conn, &result).unwrap();
        let loaded = load_multivariate_result(&conn, &result.id)
            .unwrap()
            .unwrap();
        assert_eq!(loaded, result);
        assert_eq!(list_all_multivariate_results(&conn).unwrap(), vec![result]);
        assert!(
            load_multivariate_result(&conn, &WindowCalibrationId::new("scalar-1"))
                .unwrap()
                .is_none()
        );
    }

    /// One calibration id names one result: a per-kind result cannot take an
    /// id a scalar one holds, a scalar one cannot take a per-kind id, and a
    /// candidate is promoted once.
    #[test]
    fn one_calibration_id_names_one_result_across_both_shapes() {
        let (_scratch, mut conn) = ledger();
        assert!(insert_multivariate_result(&mut conn, &per_kind("scalar-1")).is_err());
        insert_multivariate_result(&mut conn, &per_kind("promoted-mvcand-1")).unwrap();
        assert!(
            insert_multivariate_result(&mut conn, &per_kind("promoted-mvcand-1-again")).is_err(),
            "a second result for one candidate is refused"
        );
        let meter = MeterSemanticsId::new("meter-v1");
        let billing = BillingSemanticsId::new("billing-v1");
        let clash = minimal_fixture(
            "promoted-mvcand-1",
            &scope(),
            &meter,
            &billing,
            &CostModelId::new("cm-v1"),
            CreditsPerPercentagePoint::from_micros_per_point(100_000),
            ValidityInterval::new(ts(1_000), ts(2_000)).unwrap(),
            ts(2_000),
        );
        let refused = insert_result(
            &mut conn,
            &clash,
            &[crate::store::calibration::ExperimentId::new("exp-scalar")],
        );
        assert!(
            refused.is_err(),
            "a scalar result cannot take a per-kind id"
        );
    }

    /// A per-kind activation supersedes the active scalar calibration: the
    /// scope's answer changes shape at the event and not before, the scalar
    /// reads as superseded, and the event names it. The same activation that
    /// names no predecessor is refused.
    #[test]
    fn a_per_kind_activation_supersedes_the_active_scalar_calibration() {
        let (_scratch, mut conn) = ledger();
        insert_multivariate_result(&mut conn, &per_kind("promoted-mvcand-1")).unwrap();
        activate_scalar(&mut conn, 3_000, None).unwrap();

        assert!(activate_per_kind(&mut conn, 4_000, None).is_err());
        assert_eq!(per_kind_events(&conn), 0);

        let scalar_id = WindowCalibrationId::new("scalar-1");
        activate_per_kind(&mut conn, 4_000, Some(&scalar_id)).unwrap();
        assert_eq!(per_kind_events(&conn), 1);
        assert!(matches!(
            load_active_at(&conn, &scope(), ts(3_999)).unwrap(),
            Some(ActiveCalibration::Scalar(_))
        ));
        match load_active_at(&conn, &scope(), ts(4_000)).unwrap() {
            Some(ActiveCalibration::PerKind(active)) => {
                assert_eq!(active.id.as_str(), "promoted-mvcand-1");
            }
            other => panic!("the per-kind result must be active: {other:?}"),
        }
        assert!(is_superseded(&conn, &scalar_id).unwrap());
        let events = multivariate_activation_events_for(
            &conn,
            &WindowCalibrationId::new("promoted-mvcand-1"),
        )
        .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, CalibrationEventKind::Supersession);
        assert_eq!(events[0].supersedes, Some(scalar_id));
    }

    /// An activation that names a per-kind row the lookup cannot return is a
    /// broken ledger and errors, like the scalar arm: answering "no
    /// calibration" would let spend fall back to a rate-card estimate in
    /// silence. The planted negative is the same ledger with the controlled
    /// run intact, which resolves to the per-kind result.
    #[test]
    fn an_unreadable_active_per_kind_row_errors_rather_than_reading_as_uncalibrated() {
        let (_scratch, mut conn) = ledger();
        insert_multivariate_result(&mut conn, &per_kind("promoted-mvcand-1")).unwrap();
        activate_per_kind(&mut conn, 3_000, None).unwrap();
        assert!(matches!(
            load_active_at(&conn, &scope(), ts(3_000)).unwrap(),
            Some(ActiveCalibration::PerKind(_))
        ));
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF; DROP TRIGGER calibration_controlled_run_no_delete; DELETE FROM calibration_controlled_run;",
        )
        .unwrap();
        let refusal = load_active_at(&conn, &scope(), ts(3_000)).unwrap_err();
        assert!(refusal.to_string().contains("cannot be read"), "{refusal}");
    }

    /// A scalar activation over an active per-kind calibration is refused
    /// naming the bead that owns it, and writes nothing.
    #[test]
    fn a_scalar_activation_over_an_active_per_kind_calibration_is_refused() {
        let (_scratch, mut conn) = ledger();
        insert_multivariate_result(&mut conn, &per_kind("promoted-mvcand-1")).unwrap();
        activate_per_kind(&mut conn, 3_000, None).unwrap();
        let per_kind_id = WindowCalibrationId::new("promoted-mvcand-1");
        let refusal = activate_scalar(&mut conn, 4_000, Some(&per_kind_id)).unwrap_err();
        assert!(
            refusal
                .to_string()
                .contains("aub-scalar-supersedes-per-kind-ufpq"),
            "{refusal}"
        );
        assert_eq!(lifecycle_event_count(&conn).unwrap(), 0);
        assert!(!is_multivariate_superseded(&conn, &per_kind_id).unwrap());
    }
}
