//! Atomic persistence for the seed meter archive (aub-fon.2, PLAN.md sections 15, 32, 33).
//!
//! Reuses the legacy evidence classes (meter_attempt, meter_attempt_result,
//! meter_response_evidence, meter_observation, meter_window, session_account_marker,
//! sampling_policy_snapshot) with honest provenance, including failure records and
//! the nominal 6-minute seed cadence.

use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;

use crate::domain::attempt::AttemptOutcome;
use crate::domain::failure::{FailureClass, HttpStatusClass};
use crate::domain::ids::{
    AdapterVersion, MeterSemanticsId, NativeSessionId, ProviderContractId, SessionId,
    SourceNamespace,
};
use crate::domain::quota::QuotaFractionPpm;
use crate::domain::time::{MeasurementBasis, MonotonicDuration, UtcTimestamp};
use crate::domain::window::{
    NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
    WindowSemanticKey,
};
use crate::error::Error;
use crate::seed_archive::{ParsedSeedArchiveSource, SeedArchiveRecord};
use crate::store::account::{self, AccountId};
pub use crate::store::legacy_meter_import::ImportSummary;
use crate::store::meter_attempt::{self, DueReason, NewMeterAttempt, NewMeterAttemptResult};
use crate::store::meter_evidence::{
    self, NewMeterObservation, NewMeterResponseEvidence, NewMeterWindow,
};
use crate::store::sample_run::Trigger;
use crate::store::sampling_policy_snapshot::{
    self, ResolvedSamplingPolicy, SamplingPolicySnapshotId,
};
use crate::store::session_account_marker::{
    self, EvidenceDesignation, MarkerSource, NewSessionAccountMarker, SourceOrderingKey,
};
use crate::store::{ledger_generation, sample_run};

const PROVIDER: &str = "anthropic";
const ADAPTER_VERSION: &str = "quota-axi-seed-archive-v1";
const PROVIDER_CONTRACT: &str = "quota-axi-seed-archive-v1";
const METER_SEMANTICS: &str = "legacy-account-windows-v1";
const MARKER_SOURCE: &str = "seed_capture";
const SESSION_NAMESPACE: &str = "seed-capture";
const POLICY_ALGORITHM_VERSION: &str = "seed-archive-cadence-v1";

/// The nominal seed timer cadence: 6 minutes in nanoseconds (PLAN.md sections 15, 33; aub-d41.3).
pub const SEED_NOMINAL_CADENCE_NANOS: u64 = 6 * 60 * 1_000_000_000;

/// Atomically imports parsed seed archive records into the ledger.
pub fn import(
    conn: &mut Connection,
    source: &ParsedSeedArchiveSource,
    verified_backup_id: &str,
    imported_at: UtcTimestamp,
) -> Result<ImportSummary, Error> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| {
            Error::Store(format!(
                "cannot open seed archive import transaction: {error}"
            ))
        })?;

    tx.execute(
        "INSERT OR IGNORE INTO legacy_meter_import (
            source_digest, verified_backup_id, imported_at, records_read, records_quarantined
         ) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            source.content_digest,
            verified_backup_id,
            imported_at.unix_nanos(),
            source.records_read as i64,
            source.records_quarantined as i64,
        ],
    )
    .map_err(|error| Error::Store(format!("cannot record seed import provenance: {error}")))?;

    let mut account_cache: HashMap<String, (AccountId, SamplingPolicySnapshotId)> = HashMap::new();
    let mut run = None;
    let mut imported = 0u64;
    let mut unchanged = 0u64;

    for record in &source.records {
        let account_name = record.account();
        let (account_id, policy_snapshot_id) = match account_cache.get(account_name) {
            Some(&pair) => pair,
            None => {
                let acc_id =
                    account::observe_account(&tx, PROVIDER, account_name, record.received_at())?;
                let policy = ResolvedSamplingPolicy {
                    ordinary_cadence: MonotonicDuration::from_nanos(SEED_NOMINAL_CADENCE_NANOS),
                    freshness_horizon: MonotonicDuration::from_nanos(SEED_NOMINAL_CADENCE_NANOS),
                    reset_edge_policy: "none".to_string(),
                    retry_backoff_policy: "none".to_string(),
                    command_budget: MonotonicDuration::from_nanos(60 * 1_000_000_000),
                    policy_algorithm_version: POLICY_ALGORITHM_VERSION.to_string(),
                };
                let policy_snap_id = sampling_policy_snapshot::resolve_policy_snapshot(
                    &tx,
                    acc_id,
                    record.received_at(),
                    &policy,
                )?;
                account_cache.insert(account_name.to_string(), (acc_id, policy_snap_id));
                (acc_id, policy_snap_id)
            }
        };

        if attempt_exists(&tx, account_id, record.received_at(), PROVIDER_CONTRACT)? {
            unchanged += 1;
            continue;
        }

        let run_id = match run {
            Some(run_id) => run_id,
            None => {
                let run_id = sample_run::start_sample_run(
                    &tx,
                    Trigger::Timer,
                    imported_at,
                    "seed-archive-import-v1",
                )?;
                run = Some(run_id);
                run_id
            }
        };

        import_single_record(
            &tx,
            run_id,
            account_id,
            policy_snapshot_id,
            &source.content_digest,
            record,
        )?;
        imported += 1;
    }

    if imported > 0 {
        ledger_generation::advance(&tx)?;
    }

    tx.commit()
        .map_err(|error| Error::Store(format!("cannot commit seed archive import: {error}")))?;

    Ok(ImportSummary {
        imported,
        unchanged,
        quarantined: source.records_quarantined,
    })
}

fn attempt_exists(
    conn: &Connection,
    account_id: AccountId,
    received_at: UtcTimestamp,
    provider_contract_id: &str,
) -> Result<bool, Error> {
    conn.query_row(
        "SELECT 1 FROM meter_attempt WHERE account_id = ?1 AND request_started_at = ?2 AND provider_contract_id = ?3",
        params![account_id.value(), received_at.unix_nanos(), provider_contract_id],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(|e| Error::Store(format!("cannot check attempt existence: {e}")))
}

fn import_single_record(
    conn: &Connection,
    run_id: sample_run::SampleRunId,
    account_id: AccountId,
    policy_snapshot_id: SamplingPolicySnapshotId,
    source_digest: &str,
    record: &SeedArchiveRecord,
) -> Result<(), Error> {
    let attempt_id = meter_attempt::start_meter_attempt(
        conn,
        &NewMeterAttempt {
            run_id,
            account_id,
            provider: PROVIDER.to_string(),
            request_started_at: record.received_at(),
            credential_context_id: None,
            policy_snapshot_id,
            due_at: record.received_at(),
            due_reason: DueReason::OrdinaryCadence,
            due_basis: None,
            provider_contract_id: PROVIDER_CONTRACT.to_string(),
            meter_semantics_id: METER_SEMANTICS.to_string(),
        },
    )?;

    match record {
        SeedArchiveRecord::Success(success) => {
            meter_attempt::record_meter_attempt_result(
                conn,
                &NewMeterAttemptResult {
                    attempt_id,
                    completed_at: success.received_at,
                    elapsed: MonotonicDuration::from_nanos(0),
                    outcome: AttemptOutcome::Success,
                    sanitized_error_classification: Some("seed_capture".to_string()),
                    retry_index: None,
                    clock_anomaly: false,
                },
            )?;

            let evidence_id = meter_evidence::insert_response_evidence(
                conn,
                &NewMeterResponseEvidence {
                    attempt_id,
                    response_classification: "seed_capture".to_string(),
                    received_at: success.received_at,
                    provider_observed_at_original: Some(success.generated_at_original.clone()),
                    evidence_capsule: success.raw_reading.clone(),
                    capsule_schema_version: "seed-archive-reading-v1".to_string(),
                    sanitizer_version: "seed-archive-sanitizer-v1".to_string(),
                    capture_truncated: false,
                },
            )?;

            let observation_id = meter_evidence::insert_observation(
                conn,
                &NewMeterObservation {
                    attempt_id,
                    evidence_id,
                    account_id,
                    provider: PROVIDER.to_string(),
                    provider_observed_at: Some(success.generated_at),
                    received_at: success.received_at,
                    measurement_basis: MeasurementBasis::ProviderObserved,
                    observed_plan: success.plan.clone(),
                    observed_tier: success.plan.clone(),
                    adapter_version: AdapterVersion::new(ADAPTER_VERSION),
                    provider_contract_id: ProviderContractId::new(PROVIDER_CONTRACT),
                    meter_semantics_id: MeterSemanticsId::new(METER_SEMANTICS),
                    normalized_fingerprint: format!(
                        "seed:{}:{}:{}",
                        source_digest, success.source_file, success.source_line
                    ),
                },
            )?;

            let resolution = ReportedResolution::new(
                QuotaFractionPpm::new(10_000).expect("one percent is valid"),
            )
            .expect("one percent is non-zero");

            for window in &success.windows {
                meter_evidence::insert_window(
                    conn,
                    &NewMeterWindow {
                        observation_id,
                        semantic_key: WindowSemanticKey::new(window.semantic_key),
                        scope: WindowScope::AccountWide,
                        quota_used: window.quota_used,
                        reported_resolution: resolution,
                        quantization: QuantizationSemantics::Unknown,
                        resets_at: window.resets_at.into(),
                        nominal_duration: NominalWindowDuration::from_nanos(
                            window.nominal_duration_nanos,
                        ),
                    },
                )?;
            }

            session_account_marker::insert_marker(
                conn,
                &NewSessionAccountMarker {
                    session_id: SessionId::new(
                        SourceNamespace::new(SESSION_NAMESPACE),
                        NativeSessionId::new(format!("seed-{}", success.received_at.unix_nanos())),
                    ),
                    observed_at: success.received_at,
                    source_ordering_key: Some(SourceOrderingKey::new(success.source_line as i64)),
                    logical_account: success.account.clone(),
                    resolved_account_id: Some(account_id),
                    marker_source: MarkerSource::new(MARKER_SOURCE),
                    run_id: None,
                    evidence_designation: EvidenceDesignation::ExplicitLauncherOrHook,
                },
            )?;
        }
        SeedArchiveRecord::Failure(failure) => {
            let failure_class = match failure.failure_classification.as_str() {
                "spawn_failed" => FailureClass::ConnectTimeout,
                "empty_output" => FailureClass::MalformedBody,
                _ => FailureClass::HttpStatus(HttpStatusClass::ServerError),
            };

            meter_attempt::record_meter_attempt_result(
                conn,
                &NewMeterAttemptResult {
                    attempt_id,
                    completed_at: failure.received_at,
                    elapsed: MonotonicDuration::from_nanos(0),
                    outcome: AttemptOutcome::Unreachable(failure_class),
                    sanitized_error_classification: Some(format!(
                        "seed_{}",
                        failure.failure_classification
                    )),
                    retry_index: None,
                    clock_anomaly: false,
                },
            )?;
        }
    }

    Ok(())
}
