//! Atomic persistence for the legacy regression-fit import.
//!
//! May not depend on:
//! - HTTP or terminal-formatting crates
//! - presentation
//! - provider adapters
//!
//! The legacy fit imports as immutable calibration history resting on an
//! incomplete cost model, never as an activatable record. The source content
//! digest is the idempotence boundary: rerunning the same source cannot
//! create a second history record.

use std::collections::BTreeSet;

use rusqlite::{OptionalExtension, params};

use crate::domain::credits::{Credits, CreditsPerPercentagePoint, CreditsPerToken};
use crate::domain::ids::{BillingSemanticsId, MeterSemanticsId};
use crate::domain::provenance::{CostModelId, EvidenceId, WindowCalibrationId};
use crate::domain::quota::QuotaFractionPpm;
use crate::domain::time::UtcTimestamp;
use crate::domain::tokens::TokenKind;
use crate::domain::window::WindowSemanticKey;
use crate::error::Error;
use crate::legacy_calibration::ParsedLegacyCalibrationSource;
use crate::store::calibration::{
    CalibrationExperiment, CoefficientUncertainty, EvidenceDigest, EvidenceFingerprint,
    ExperimentId, LagHandling, PlanTier, WindowCalibration, WindowCalibrationFields,
};
use crate::store::cost_model::{
    CostModel, CostModelScope, CostModelTerm, CostModelVersion, ModelProvenance, ProviderKey,
    TermDerivationMethod, ValidityInterval,
};

pub use crate::store::legacy_meter_import::ImportSummary;

/// The incomplete cost model the legacy fit rests on: the published rates
/// for every kind except cache-write, which the legacy cost model omitted.
pub const LEGACY_INCOMPLETE_COST_MODEL_ID: &str = "legacy-incomplete-cost-model-v1";

const LEGACY_METER_SEMANTICS: &str = "legacy-account-windows-v1";
const LEGACY_BILLING_SEMANTICS: &str = "anthropic-messages-subscription-v1";
const LEGACY_ACTIVATION_POLICY_VERSION: &str = "legacy-import-v1";

/// Imports one parsed legacy-calibration source as non-activatable history.
///
/// A quarantined source (malformed document) records its import provenance
/// and nothing else. A fit-evidence source ensures the incomplete cost
/// model, one experiment and one result exist, then records its import
/// provenance and advances the ledger generation. A repeated import of the
/// same content digest returns `unchanged` without writing history twice.
///
/// Each step checks existence before writing, so a run interrupted between
/// steps resumes to the same state rather than failing on a duplicate row.
pub fn import(
    conn: &mut rusqlite::Connection,
    source: &ParsedLegacyCalibrationSource,
    verified_backup_id: &str,
    imported_at: UtcTimestamp,
) -> Result<ImportSummary, Error> {
    if import_exists(conn, &source.content_digest)? {
        return Ok(ImportSummary {
            imported: 0,
            unchanged: if source.records_read > 0 { 1 } else { 0 },
            quarantined: source.records_quarantined,
        });
    }

    let Some(fit) = &source.record else {
        conn.execute(
            "INSERT INTO legacy_calibration_import (
                source_digest, verified_backup_id, imported_at, calibration_id,
                records_read, records_quarantined
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                source.content_digest,
                verified_backup_id,
                imported_at.unix_nanos(),
                "",
                source.records_read as i64,
                source.records_quarantined as i64,
            ],
        )
        .map_err(|error| {
            Error::Store(format!(
                "cannot record legacy calibration import provenance: {error}"
            ))
        })?;
        return Ok(ImportSummary {
            imported: 0,
            unchanged: 0,
            quarantined: source.records_quarantined,
        });
    };

    ensure_incomplete_cost_model(conn, fit, imported_at)?;
    ensure_experiment(conn, fit, imported_at)?;
    ensure_result(conn, fit, imported_at)?;

    conn.execute(
        "INSERT INTO legacy_calibration_import (
            source_digest, verified_backup_id, imported_at, calibration_id,
            records_read, records_quarantined
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            source.content_digest,
            verified_backup_id,
            imported_at.unix_nanos(),
            fit.calibration_id,
            source.records_read as i64,
            source.records_quarantined as i64,
        ],
    )
    .map_err(|error| {
        Error::Store(format!(
            "cannot record legacy calibration import provenance: {error}"
        ))
    })?;

    crate::store::ledger_generation::advance(conn)?;

    Ok(ImportSummary {
        imported: 1,
        unchanged: 0,
        quarantined: source.records_quarantined,
    })
}

fn import_exists(conn: &rusqlite::Connection, source_digest: &str) -> Result<bool, Error> {
    conn.query_row(
        "SELECT 1 FROM legacy_calibration_import WHERE source_digest = ?1",
        params![source_digest],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(|error| {
        Error::Store(format!(
            "cannot read legacy calibration import identity: {error}"
        ))
    })
}

/// Ensures the incomplete cost model the legacy fit rests on: every
/// published rate except cache-write, which the legacy cost model omitted.
/// Inserted once; a later import reuses the same row.
fn ensure_incomplete_cost_model(
    conn: &mut rusqlite::Connection,
    fit: &crate::legacy_calibration::LegacyCalibrationFit,
    imported_at: UtcTimestamp,
) -> Result<(), Error> {
    let id = CostModelId::new(LEGACY_INCOMPLETE_COST_MODEL_ID);
    if crate::store::cost_model::load_by_semantic_id(conn, &id)?.is_some() {
        return Ok(());
    }
    let validity =
        ValidityInterval::new(fit.fit_timestamp, UtcTimestamp::from_unix_nanos(i64::MAX))?;
    let digest = digest_u64(&source_digest_seed(fit));
    let model = CostModel::new(
        id,
        ProviderKey::new(fit.provider.clone()),
        CostModelScope::ModelClass,
        BillingSemanticsId::new(LEGACY_BILLING_SEMANTICS),
        None,
        CostModelVersion::new("1.0-legacy-incomplete"),
        validity,
        imported_at,
        ModelProvenance::from_parts(digest, fit.experiment.evidence_ids.len()),
        vec![
            CostModelTerm::new(
                TokenKind::Input,
                CreditsPerToken::from_micros_per_million_tokens(3_000_000),
                None,
                TermDerivationMethod::PublishedBillingSemantics,
                None,
            ),
            CostModelTerm::new(
                TokenKind::Output,
                CreditsPerToken::from_micros_per_million_tokens(15_000_000),
                None,
                TermDerivationMethod::PublishedBillingSemantics,
                None,
            ),
            CostModelTerm::new(
                TokenKind::CacheRead,
                CreditsPerToken::from_micros_per_million_tokens(300_000),
                None,
                TermDerivationMethod::PublishedBillingSemantics,
                None,
            ),
        ],
    )?;
    crate::store::cost_model::insert_model(conn, &model)?;
    Ok(())
}

fn source_digest_seed(fit: &crate::legacy_calibration::LegacyCalibrationFit) -> String {
    format!("{}:{}", fit.calibration_id, fit.experiment.experiment_id)
}

fn digest_u64(text: &str) -> u64 {
    use sha2::{Digest, Sha256};
    let bytes = Sha256::digest(text.as_bytes());
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&bytes[..8]);
    u64::from_be_bytes(raw)
}

fn ensure_experiment(
    conn: &rusqlite::Connection,
    fit: &crate::legacy_calibration::LegacyCalibrationFit,
    imported_at: UtcTimestamp,
) -> Result<(), Error> {
    let id = ExperimentId::new(fit.experiment.experiment_id.clone());
    if crate::store::calibration::load_experiment(conn, &id)?.is_some() {
        return Ok(());
    }
    let experiment = build_experiment(fit, imported_at);
    crate::store::calibration::insert_experiment(conn, &experiment)?;
    Ok(())
}

fn ensure_result(
    conn: &mut rusqlite::Connection,
    fit: &crate::legacy_calibration::LegacyCalibrationFit,
    imported_at: UtcTimestamp,
) -> Result<(), Error> {
    let id = WindowCalibrationId::new(fit.calibration_id.clone());
    if crate::store::calibration::load_result(conn, &id)?.is_some() {
        return Ok(());
    }
    let calibration = build_result(fit, imported_at);
    crate::store::calibration::insert_result(
        conn,
        &calibration,
        &[ExperimentId::new(fit.experiment.experiment_id.clone())],
    )?;
    Ok(())
}

fn build_experiment(
    fit: &crate::legacy_calibration::LegacyCalibrationFit,
    imported_at: UtcTimestamp,
) -> CalibrationExperiment {
    use crate::calibration::settlement::SettlementPolicy;
    let validity =
        ValidityInterval::new(fit.fit_timestamp, UtcTimestamp::from_unix_nanos(i64::MAX))
            .expect("legacy fit timestamp precedes the maximum instant");
    let resolution = crate::domain::window::ReportedResolution::new(
        QuotaFractionPpm::new(10_000).expect("valid"),
    )
    .expect("valid");
    CalibrationExperiment {
        id: ExperimentId::new(fit.experiment.experiment_id.clone()),
        provider: ProviderKey::new(fit.provider.clone()),
        plan_tier: PlanTier::new(fit.plan_tier.clone()),
        window_semantic_key: WindowSemanticKey::new(fit.window_semantic_key.clone()),
        meter_semantics_id: MeterSemanticsId::new(LEGACY_METER_SEMANTICS),
        billing_semantics_id: BillingSemanticsId::new(LEGACY_BILLING_SEMANTICS),
        settlement_policy: SettlementPolicy::conservative_default(resolution),
        validity,
        knowledge_time: imported_at,
    }
}

fn build_result(
    fit: &crate::legacy_calibration::LegacyCalibrationFit,
    imported_at: UtcTimestamp,
) -> WindowCalibration {
    let fitted = CreditsPerPercentagePoint::from_micros_per_point(fit.fitted_micros_per_point);
    let evidence: BTreeSet<EvidenceId> = fit
        .experiment
        .evidence_ids
        .iter()
        .map(EvidenceId::new)
        .collect();
    let mut sorted: Vec<EvidenceId> = evidence.iter().cloned().collect();
    sorted.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    let (fitting_set, validation_set) = if sorted.len() >= 2 {
        let mid = sorted.len() / 2;
        (
            sorted[..mid].iter().cloned().collect::<BTreeSet<_>>(),
            sorted[mid..].iter().cloned().collect::<BTreeSet<_>>(),
        )
    } else {
        (evidence.clone(), evidence.clone())
    };
    let validity =
        ValidityInterval::new(fit.fit_timestamp, UtcTimestamp::from_unix_nanos(i64::MAX))
            .expect("legacy fit timestamp precedes the maximum instant");
    WindowCalibration::from_fields(WindowCalibrationFields {
        id: WindowCalibrationId::new(fit.calibration_id.clone()),
        provider: ProviderKey::new(fit.provider.clone()),
        plan_tier: PlanTier::new(fit.plan_tier.clone()),
        window_semantic_key: WindowSemanticKey::new(fit.window_semantic_key.clone()),
        meter_semantics_id: MeterSemanticsId::new(LEGACY_METER_SEMANTICS),
        billing_semantics_id: BillingSemanticsId::new(LEGACY_BILLING_SEMANTICS),
        cost_model_id: CostModelId::new(LEGACY_INCOMPLETE_COST_MODEL_ID),
        fitted,
        equivalent_full_window_capacity: Credits::from_micros(
            fit.fitted_micros_per_point.saturating_mul(100),
        ),
        fit_residual: Credits::from_micros(0),
        uncertainty: CoefficientUncertainty::new(fitted, fitted)
            .expect("identical bounds are a valid point uncertainty"),
        lag_estimate: None,
        lag_handling: LagHandling::new("legacy-unknown"),
        sample_count: fit.experiment.evidence_ids.len() as u32,
        fit_timestamp: fit.fit_timestamp,
        inputs: EvidenceDigest::from_inputs(&evidence),
        fitting_evidence: EvidenceFingerprint::from_inputs(&fitting_set),
        validation_evidence: EvidenceFingerprint::from_inputs(&validation_set),
        validation_method: "legacy-import".to_string(),
        validation_version: "v1".to_string(),
        out_of_sample_residual: Some(Credits::from_micros(0)),
        statistical_method: fit.experiment.method.clone(),
        statistical_parameters: format!(
            "{{\"origin\":\"{}\"}}",
            fit.provenance.origin.replace('"', "'")
        ),
        condition_number: None,
        observation_coverage_requirement: "legacy-unknown".to_string(),
        settling_policy: "legacy-unknown".to_string(),
        excluded_samples: Vec::new(),
        activation_policy_version: LEGACY_ACTIVATION_POLICY_VERSION.to_string(),
        aub_version: crate::build_info::crate_version().to_string(),
        source_revision: crate::build_info::source_revision().to_string(),
        validity,
        knowledge_time: imported_at,
    })
    .expect("legacy result fields satisfy the constructor invariants")
}
