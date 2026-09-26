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

const ADAPTER_VERSION: &str = "quota-axi-seed-archive-v1";
/// The provider contract every seed-archive row carries. Public because the
/// legacy meter importer must recognise it as legacy rather than native
/// sampling when it computes its cutoff, and a second spelling of the same
/// string would be silently wrong.
pub const PROVIDER_CONTRACT: &str = "quota-axi-seed-archive-v1";
const METER_SEMANTICS: &str = "legacy-account-windows-v1";
const MARKER_SOURCE: &str = "seed_capture";
const SESSION_NAMESPACE: &str = "seed-capture";
const POLICY_ALGORITHM_VERSION: &str = "seed-archive-cadence-v1";

/// The nominal seed timer cadence: 6 minutes in nanoseconds (PLAN.md sections 15, 33; aub-d41.3).
pub const SEED_NOMINAL_CADENCE_NANOS: u64 = 6 * 60 * 1_000_000_000;

/// The response classification of a reading whose account is an operator
/// assertion rather than a measurement, distinguished from `seed_capture` in
/// the evidence row so a later reader can tell the two apart without knowing
/// this date.
pub const OPERATOR_ASSERTED_ACCOUNT_CLASSIFICATION: &str = "seed_capture_operator_asserted_account";

/// From this instant a codex reading's account is an operator assertion. The
/// second codex account did not exist before it, so every earlier codex
/// reading can only be the first one; afterwards the weekly window is rolling
/// and the two accounts cannot be told apart from the capture alone.
const CODEX_ACCOUNT_ASSERTED_FROM: &str = "2026-08-31T00:00:00Z";

/// The vendor label a codex reading carries.
const CODEX_VENDOR: &str = "codex";

/// Where one vendor's readings are written: the configured account, and the
/// provider that account is declared under. Built by the caller from
/// `[[accounts]]` and the operator's explicit choice, never guessed here: one
/// provider commonly has several configured accounts, and the capture cannot
/// say which of them it read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedVendorAccount {
    pub provider: String,
    pub account: String,
}

/// The vendor-to-account decision for one import, keyed by canonical vendor
/// (`crate::seed_archive::canonical_vendor`). A vendor absent from it is
/// discarded: an account the operator never declared is not invented from a
/// label a capture script happened to write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeedVendorAccountMap {
    entries: std::collections::BTreeMap<String, SeedVendorAccount>,
}

impl SeedVendorAccountMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, vendor: &str, target: SeedVendorAccount) {
        self.entries.insert(vendor.to_string(), target);
    }

    pub fn get(&self, vendor: &str) -> Option<&SeedVendorAccount> {
        self.entries.get(vendor)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// What one seed import did. Deliberately not the legacy importer's summary:
/// this one counts a vendor entry nothing maps, which that source cannot carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SeedArchiveImportSummary {
    pub imported: u64,
    pub unchanged: u64,
    /// Readings at or after their account's native cutoff. Classified on every
    /// run, so the figure is a property of the source and the ledger rather
    /// than of how often the import has been attempted.
    pub superseded_by_native: u64,
    /// Readings whose vendor the caller mapped to no configured account.
    pub discarded_unmapped_vendor: u64,
    pub quarantined: u64,
}

/// Atomically imports the parsed seed archive readings native sampling does not
/// already cover.
///
/// Three rules decide what a reading becomes, because the seed series overlaps
/// the live one in time and carries several vendors per line:
///
/// - **Vendor mapping.** `accounts` says which configured account each vendor's
///   readings belong to. A vendor it does not name is discarded and counted.
/// - **Cutoff.** Per target account, the earliest native `meter_attempt` start.
///   A reading at or after it is already measured by the sampler, and importing
///   it would lay a second, disagreeing series over the first. Both importer
///   contracts are excluded, so an earlier import never becomes its own cutoff.
/// - **Provenance.** A reading whose account is an operator assertion rather
///   than a measurement says so in its evidence row and in the rank of its
///   marker.
pub fn import(
    conn: &mut Connection,
    source: &ParsedSeedArchiveSource,
    accounts: &SeedVendorAccountMap,
    verified_backup_id: &str,
    imported_at: UtcTimestamp,
) -> Result<SeedArchiveImportSummary, Error> {
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

    let asserted_from = UtcTimestamp::parse_rfc3339(CODEX_ACCOUNT_ASSERTED_FROM)
        .ok_or_else(|| Error::Internal("the codex assertion date must parse".into()))?;
    let mut account_cache: HashMap<String, (AccountId, SamplingPolicySnapshotId)> = HashMap::new();
    let mut cutoffs: HashMap<String, Option<UtcTimestamp>> = HashMap::new();
    let mut run = None;
    let mut imported = 0u64;
    let mut unchanged = 0u64;
    let mut superseded_by_native = 0u64;
    let mut discarded_unmapped_vendor = 0u64;

    for record in &source.records {
        let Some(target) = accounts.get(record.vendor()) else {
            discarded_unmapped_vendor += 1;
            continue;
        };
        let account_name = target.account.as_str();
        let (account_id, policy_snapshot_id) = match account_cache.get(account_name) {
            Some(&pair) => pair,
            None => {
                let acc_id = account::observe_account(
                    &tx,
                    &target.provider,
                    account_name,
                    record.received_at(),
                )?;
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

        let cutoff = match cutoffs.get(account_name) {
            Some(cutoff) => *cutoff,
            None => {
                let cutoff = native_cutoff_for_account(&tx, account_id)?;
                cutoffs.insert(account_name.to_string(), cutoff);
                cutoff
            }
        };
        if cutoff.is_some_and(|cutoff| record.received_at() >= cutoff) {
            superseded_by_native += 1;
            continue;
        }

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

        let account_is_operator_asserted =
            record.vendor() == CODEX_VENDOR && record.received_at() >= asserted_from;
        import_single_record(
            &tx,
            run_id,
            account_id,
            policy_snapshot_id,
            &source.content_digest,
            record,
            &target.provider,
            account_is_operator_asserted,
        )?;
        imported += 1;
    }

    if imported > 0 {
        ledger_generation::advance(&tx)?;
    }

    tx.commit()
        .map_err(|error| Error::Store(format!("cannot commit seed archive import: {error}")))?;

    Ok(SeedArchiveImportSummary {
        imported,
        unchanged,
        superseded_by_native,
        discarded_unmapped_vendor,
        quarantined: source.records_quarantined,
    })
}

/// The earliest native meter attempt for an account, or `None` when the sampler
/// has never reached it. Both importer contracts are excluded: a row an importer
/// wrote is history, not coverage, and letting it stand as a cutoff would make a
/// second import of the same source import nothing.
fn native_cutoff_for_account(
    conn: &Connection,
    account_id: AccountId,
) -> Result<Option<UtcTimestamp>, Error> {
    conn.query_row(
        "SELECT MIN(request_started_at) FROM meter_attempt
         WHERE account_id = ?1 AND provider_contract_id NOT IN (?2, ?3)",
        params![
            account_id.value(),
            PROVIDER_CONTRACT,
            crate::store::legacy_meter_import::PROVIDER_CONTRACT,
        ],
        |row| row.get::<_, Option<i64>>(0),
    )
    .map(|nanos| nanos.map(UtcTimestamp::from_unix_nanos))
    .map_err(|error| Error::Store(format!("cannot read the native meter cutoff: {error}")))
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

#[allow(clippy::too_many_arguments)]
fn import_single_record(
    conn: &Connection,
    run_id: sample_run::SampleRunId,
    account_id: AccountId,
    policy_snapshot_id: SamplingPolicySnapshotId,
    source_digest: &str,
    record: &SeedArchiveRecord,
    provider: &str,
    account_is_operator_asserted: bool,
) -> Result<(), Error> {
    // The classification travels into the evidence row and the marker's rank
    // together: one says what the capture was, the other how far the account
    // attribution can be trusted.
    let classification = if account_is_operator_asserted {
        OPERATOR_ASSERTED_ACCOUNT_CLASSIFICATION
    } else {
        MARKER_SOURCE
    };
    let designation = if account_is_operator_asserted {
        EvidenceDesignation::ConservativeTemporalInference
    } else {
        EvidenceDesignation::ExplicitLauncherOrHook
    };
    let attempt_id = meter_attempt::start_meter_attempt(
        conn,
        &NewMeterAttempt {
            run_id,
            account_id,
            provider: provider.to_string(),
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
                    sanitized_error_classification: Some(classification.to_string()),
                    retry_index: None,
                    clock_anomaly: false,
                },
            )?;

            let evidence_id = meter_evidence::insert_response_evidence(
                conn,
                &NewMeterResponseEvidence {
                    attempt_id,
                    response_classification: classification.to_string(),
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
                    provider: provider.to_string(),
                    provider_observed_at: Some(success.generated_at),
                    received_at: success.received_at,
                    measurement_basis: MeasurementBasis::ProviderObserved,
                    observed_plan: success.plan.clone(),
                    observed_tier: success.plan.clone(),
                    adapter_version: AdapterVersion::new(ADAPTER_VERSION),
                    provider_contract_id: ProviderContractId::new(PROVIDER_CONTRACT),
                    meter_semantics_id: MeterSemanticsId::new(METER_SEMANTICS),
                    // The vendor is part of the identity: one source line carries
                    // a reading per vendor, so the digest, file and line alone
                    // would give several observations one fingerprint.
                    normalized_fingerprint: format!(
                        "seed:{}:{}:{}:{}",
                        source_digest, success.source_file, success.source_line, success.vendor
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
                        NativeSessionId::new(format!(
                            "seed-{}-{}",
                            success.vendor,
                            success.received_at.unix_nanos()
                        )),
                    ),
                    observed_at: success.received_at,
                    source_ordering_key: Some(SourceOrderingKey::new(success.source_line as i64)),
                    logical_account: success.account.clone(),
                    resolved_account_id: Some(account_id),
                    marker_source: MarkerSource::new(MARKER_SOURCE),
                    run_id: None,
                    evidence_designation: designation,
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
