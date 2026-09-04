//! Database repository and query loading for candidate interval reconciliation (aub-dpn.1).
//!
//! May not depend on:
//! - presentation
//! - provider adapters
//! - HTTP transport

use std::collections::BTreeMap;

use rusqlite::{Connection, params};

use crate::attribution::account_segment::AccountEvidenceClass;
use crate::calibration::health::{
    ApplicabilityContext, CalibrationFacts, HealthInputs, LifecycleState,
};
use crate::domain::credits::Credits;
use crate::domain::provenance::{CostModelId, EvidenceId};
use crate::domain::time::UtcTimestamp;
use crate::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, UsageVector,
};
use crate::domain::window::WindowSemanticKey;
use crate::error::Error;
use crate::evidence::{CoverageCompleteness, EvidenceQuality};
use crate::reconciliation::{
    CandidateInterval, CandidateObservation, CandidateUsageEvent, IntervalCoverage,
    IntervalSettlement, IntervalUsage, ReconciliationOutcome, reconcile,
};
use crate::store::account::AccountId;
use crate::store::calibration::{CalibrationScope, PlanTier};
use crate::store::cost_model::ProviderKey;
use crate::store::meter_evidence::ObservationRowId;

/// Loads observation evidence, usage events, and active calibration from SQLite
/// and performs candidate reconciliation for one interval (aub-dpn.1).
///
/// Takes eight arguments because connection, account, boundary observations, window,
/// cost model, knowledge time, and effective time represent independent query boundaries.
#[allow(clippy::too_many_arguments)]
pub fn reconcile_candidate_from_store(
    conn: &Connection,
    account_id: AccountId,
    start_obs_id: ObservationRowId,
    end_obs_id: ObservationRowId,
    window_key: &WindowSemanticKey,
    _cost_model_id: &CostModelId,
    knowledge_time: UtcTimestamp,
    effective_time: UtcTimestamp,
) -> Result<ReconciliationOutcome, Error> {
    let start_obs = crate::store::meter_evidence::observation_by_row_id(conn, start_obs_id)?
        .ok_or_else(|| {
            Error::Store(format!(
                "start observation {} not found",
                start_obs_id.value()
            ))
        })?;
    let end_obs = crate::store::meter_evidence::observation_by_row_id(conn, end_obs_id)?
        .ok_or_else(|| Error::Store(format!("end observation {} not found", end_obs_id.value())))?;

    let start_windows = crate::store::meter_evidence::windows_by_observation(conn, start_obs_id)?;
    let start_window = start_windows
        .into_iter()
        .find(|w| &w.semantic_key == window_key)
        .ok_or_else(|| {
            Error::Store(format!(
                "window key {} not found in start observation",
                window_key.as_str()
            ))
        })?;

    let end_windows = crate::store::meter_evidence::windows_by_observation(conn, end_obs_id)?;
    let end_window = end_windows
        .into_iter()
        .find(|w| &w.semantic_key == window_key)
        .ok_or_else(|| {
            Error::Store(format!(
                "window key {} not found in end observation",
                window_key.as_str()
            ))
        })?;

    let start_cand = CandidateObservation {
        observation_id: EvidenceId::new(format!("observation:{}", start_obs.row_id.value())),
        account_id: start_obs.account_id,
        received_at: start_obs.received_at,
        window_key: start_window.semantic_key.clone(),
        quota_used: start_window.quota_used,
        resets_at: start_window.resets_at,
    };

    let end_cand = CandidateObservation {
        observation_id: EvidenceId::new(format!("observation:{}", end_obs.row_id.value())),
        account_id: end_obs.account_id,
        received_at: end_obs.received_at,
        window_key: end_window.semantic_key.clone(),
        quota_used: end_window.quota_used,
        resets_at: end_window.resets_at,
    };

    let resets = crate::store::meter_evidence::reset_windows_for_account_between(
        conn,
        account_id,
        start_obs.received_at,
        end_obs.received_at,
    )?;
    let resets_in_interval: Vec<UtcTimestamp> = resets.into_iter().map(|r| r.at).collect();

    let plan_tier_str = start_obs
        .observed_tier
        .as_deref()
        .or(start_obs.observed_plan.as_deref())
        .unwrap_or("default");

    let scope = CalibrationScope {
        provider: ProviderKey::new(&start_obs.provider),
        plan_tier: PlanTier::new(plan_tier_str),
        window_semantic_key: window_key.clone(),
    };

    let active_calibration =
        crate::store::calibration::load_active_at(conn, &scope, knowledge_time)?;
    let calibration_health = if let Some(ref cal) = active_calibration {
        let facts = CalibrationFacts {
            plan_tier: cal.plan_tier().clone(),
            meter_semantics_id: cal.meter_semantics_id().clone(),
            billing_semantics_id: cal.billing_semantics_id().clone(),
        };
        let context = ApplicabilityContext {
            plan_tier: cal.plan_tier().clone(),
            meter_semantics_id: cal.meter_semantics_id().clone(),
            billing_semantics_id: cal.billing_semantics_id().clone(),
        };
        let health_inputs = HealthInputs {
            calibration: &facts,
            context: &context,
            lifecycle: LifecycleState::Active,
            cost_model_superseded: false,
            drift: None,
            review_due_at: None,
        };
        Some(crate::calibration::health::compute_health(
            &health_inputs,
            effective_time,
        ))
    } else {
        None
    };

    let cost_model = crate::store::cost_model::load_active_at(conn, knowledge_time)?;

    let mut stmt = conn
        .prepare(
            "SELECT e.id, e.canonical_event_id, e.event_timestamp, e.evidence_kind              FROM usage_event e              WHERE e.event_timestamp >= ?1 AND e.event_timestamp < ?2              ORDER BY e.event_timestamp, e.id",
        )
        .map_err(|e| Error::Store(format!("cannot prepare usage scan: {e}")))?;

    let rows = stmt
        .query_map(
            params![
                start_obs.received_at.unix_nanos(),
                end_obs.received_at.unix_nanos()
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .map_err(|e| Error::Store(format!("cannot query usage events: {e}")))?;

    let mut candidate_events = Vec::new();
    let mut total_credits = Credits::from_micros(0);

    for r in rows {
        let (ev_id, canonical_id, ts_nanos, evidence_kind) =
            r.map_err(|e| Error::Store(format!("cannot read usage row: {e}")))?;

        let mut comp_stmt = conn
            .prepare("SELECT component_name, token_count FROM usage_component WHERE event_id = ?1")
            .map_err(|e| Error::Store(format!("cannot prepare component query: {e}")))?;
        let comp_rows = comp_stmt
            .query_map(params![ev_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|e| Error::Store(format!("cannot query components: {e}")))?;

        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut cache_read = 0u64;
        let mut cache_write = 0u64;

        for cr in comp_rows {
            let (name, count) =
                cr.map_err(|e| Error::Store(format!("cannot read component: {e}")))?;
            let cnt = count.max(0) as u64;
            match name.as_str() {
                "input" => input_tokens += cnt,
                "output" => output_tokens += cnt,
                "cache_read" => cache_read += cnt,
                "cache_write" => cache_write += cnt,
                _ => {}
            }
        }

        let is_measured = evidence_kind == "measured";
        let usage_vec = UsageVector::new(
            KnownTokenVector::new(
                InputTokens::new(input_tokens),
                OutputTokens::new(output_tokens),
                CacheReadTokens::new(cache_read),
                CacheWriteTokens::new(cache_write),
            ),
            BTreeMap::new(),
            CoverageCompleteness::Complete,
            if is_measured {
                EvidenceQuality::Measured
            } else {
                EvidenceQuality::estimated([], None)
            },
        );

        if let Some(ref cm) = cost_model {
            let derivation = crate::cost_model::convert(cm, &usage_vec);
            if let crate::evidence::Derivation::Available(q) = derivation {
                let (credits, _, _, _) = q.into_parts();
                total_credits = total_credits + credits;
            }
        }

        candidate_events.push(CandidateUsageEvent {
            event_id: EvidenceId::new(format!("usage_event:{}", canonical_id)),
            occurred_at: UtcTimestamp::from_unix_nanos(ts_nanos),
            is_measured,
            attribution_class: AccountEvidenceClass::ExplicitLauncherOrHook,
            is_quarantined: false,
        });
    }

    let is_settled = effective_time.unix_nanos() >= end_obs.received_at.unix_nanos();

    let candidate = CandidateInterval {
        account_id,
        window_key: window_key.clone(),
        start_observation: start_cand,
        end_observation: end_cand,
        resets_in_interval,
        coverage: IntervalCoverage::acceptable(),
        active_calibration,
        calibration_health,
        settlement: IntervalSettlement {
            is_settled,
            lag_handling_satisfied: true,
        },
        local_usage: IntervalUsage::new(candidate_events, total_credits),
    };

    Ok(reconcile(&candidate))
}
