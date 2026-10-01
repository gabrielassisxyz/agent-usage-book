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
use crate::seed_archive::{
    ParsedSeedArchiveSource, SeedArchiveRecord, SeedArchiveSuccessRecord, SeedArchiveWindow,
};
use crate::store::account::{self, AccountId};
use crate::store::meter_attempt::{
    self, DueReason, MeterAttemptRowId, NewMeterAttempt, NewMeterAttemptResult,
};
use crate::store::meter_evidence::{
    self, EvidenceRowId, NewMeterObservation, NewMeterResponseEvidence, NewMeterWindow,
    ObservationRowId,
};
use crate::store::sample_run::Trigger;
use crate::store::sampling_policy_snapshot::{
    self, ResolvedSamplingPolicy, SamplingPolicySnapshotId,
};
use crate::store::session_account_marker::{
    self, EvidenceDesignation, MarkerSource, NewSessionAccountMarker, SourceOrderingKey,
};
use crate::store::{ledger_generation, sample_run};

/// The interpretation this importer writes today. Bumped from `v1`, which
/// filled a window the capture reported with no `resetsAt` with
/// `generated_at` plus the nominal length, inventing a reset instant the
/// source never carried.
const ADAPTER_VERSION: &str = "quota-axi-seed-archive-v2";
/// The interpretation `v2` corrects. An observation still carrying it is the
/// one a re-run reinterprets against the same evidence row.
const SUPERSEDED_ADAPTER_VERSION: &str = "quota-axi-seed-archive-v1";
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
    /// Readings an earlier run imported under a superseded adapter version
    /// whose windows this one reads differently. Each is a new observation
    /// against the evidence row already stored, with the preference selector
    /// moved to it; nothing is updated or deleted.
    pub reinterpreted: u64,
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
    let mut reinterpreted = 0u64;

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

        if let Some(attempt_id) =
            existing_attempt(&tx, account_id, record.received_at(), PROVIDER_CONTRACT)?
        {
            let corrected = match record {
                SeedArchiveRecord::Success(success) => reinterpret_existing_attempt(
                    &tx,
                    attempt_id,
                    account_id,
                    &source.content_digest,
                    success,
                    &target.provider,
                )?,
                SeedArchiveRecord::Failure(_) => false,
            };
            if corrected {
                reinterpreted += 1;
            } else {
                unchanged += 1;
            }
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

    if imported > 0 || reinterpreted > 0 {
        ledger_generation::advance(&tx)?;
    }

    tx.commit()
        .map_err(|error| Error::Store(format!("cannot commit seed archive import: {error}")))?;

    Ok(SeedArchiveImportSummary {
        imported,
        unchanged,
        superseded_by_native,
        discarded_unmapped_vendor,
        reinterpreted,
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

fn existing_attempt(
    conn: &Connection,
    account_id: AccountId,
    received_at: UtcTimestamp,
    provider_contract_id: &str,
) -> Result<Option<MeterAttemptRowId>, Error> {
    conn.query_row(
        "SELECT id FROM meter_attempt WHERE account_id = ?1 AND request_started_at = ?2 AND provider_contract_id = ?3",
        params![account_id.value(), received_at.unix_nanos(), provider_contract_id],
        |row| row.get::<_, i64>(0).map(MeterAttemptRowId::new),
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot check attempt existence: {e}")))
}

/// The reset state a seed window carries. An absent `resetsAt` is the
/// capture's way of saying the window has not started, which is what the
/// native adapters store as a NULL reset.
fn seed_reset_state(window: &SeedArchiveWindow) -> crate::domain::window::WindowResetState {
    match window.resets_at {
        Some(instant) => crate::domain::window::WindowResetState::Known(instant),
        None => crate::domain::window::WindowResetState::NotStarted,
    }
}

/// Corrects one reading an earlier run imported under a superseded adapter
/// version, and answers whether it wrote anything.
///
/// The correction is the one the schema prescribes: a new interpretation of
/// the evidence row already stored, with the preference selector moved to it.
/// No row is updated or deleted, so the earlier reading stays readable exactly
/// as the earlier binary wrote it.
fn reinterpret_existing_attempt(
    conn: &Connection,
    attempt_id: MeterAttemptRowId,
    account_id: AccountId,
    source_digest: &str,
    success: &SeedArchiveSuccessRecord,
    provider: &str,
) -> Result<bool, Error> {
    let Some(evidence_id) = evidence_for_attempt(conn, attempt_id)? else {
        return Ok(false);
    };
    let semantics = MeterSemanticsId::new(METER_SEMANTICS);
    let Some(current) = meter_evidence::current_observation_id(conn, evidence_id, &semantics)?
    else {
        return Ok(false);
    };
    let Some(observation) = meter_evidence::observation_by_row_id(conn, current)? else {
        return Ok(false);
    };
    if observation.adapter_version.as_str() != SUPERSEDED_ADAPTER_VERSION {
        return Ok(false);
    }
    if stored_windows_match(conn, current, &success.windows)? {
        return Ok(false);
    }

    let observation_id = insert_seed_observation(
        conn,
        attempt_id,
        evidence_id,
        account_id,
        source_digest,
        success,
        provider,
    )?;
    insert_seed_windows(conn, observation_id, &success.windows)?;
    meter_evidence::switch_current_observation(conn, evidence_id, &semantics, observation_id)?;
    Ok(true)
}

fn evidence_for_attempt(
    conn: &Connection,
    attempt_id: MeterAttemptRowId,
) -> Result<Option<EvidenceRowId>, Error> {
    conn.query_row(
        "SELECT id FROM meter_response_evidence WHERE attempt_id = ?1 ORDER BY id LIMIT 1",
        params![attempt_id.value()],
        |row| row.get::<_, i64>(0).map(EvidenceRowId::new),
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot read the evidence of an attempt: {e}")))
}

/// Whether the stored interpretation of an evidence row already says what this
/// importer would say about it. Every stored fact a seed window decides is
/// compared, so a difference in any of them is a correction rather than only a
/// difference in the reset.
fn stored_windows_match(
    conn: &Connection,
    observation_id: ObservationRowId,
    windows: &[SeedArchiveWindow],
) -> Result<bool, Error> {
    let stored = meter_evidence::windows_by_observation(conn, observation_id)?;
    if stored.len() != windows.len() {
        return Ok(false);
    }
    Ok(stored.iter().zip(windows).all(|(stored, window)| {
        stored.semantic_key.as_str() == window.semantic_key
            && stored.quota_used == window.quota_used
            && stored.resets_at == seed_reset_state(window)
            && stored.nominal_duration.as_nanos() == window.nominal_duration_nanos
    }))
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

            let observation_id = insert_seed_observation(
                conn,
                attempt_id,
                evidence_id,
                account_id,
                source_digest,
                success,
                provider,
            )?;
            insert_seed_windows(conn, observation_id, &success.windows)?;

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

/// Writes one interpretation of a seed reading's evidence row. Shared by the
/// first import of a reading and by a later correction of it, so both spell the
/// same observation identity and only the adapter version distinguishes them.
fn insert_seed_observation(
    conn: &Connection,
    attempt_id: MeterAttemptRowId,
    evidence_id: EvidenceRowId,
    account_id: AccountId,
    source_digest: &str,
    success: &SeedArchiveSuccessRecord,
    provider: &str,
) -> Result<ObservationRowId, Error> {
    meter_evidence::insert_observation(
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
    )
}

fn insert_seed_windows(
    conn: &Connection,
    observation_id: ObservationRowId,
    windows: &[SeedArchiveWindow],
) -> Result<(), Error> {
    let resolution =
        ReportedResolution::new(QuotaFractionPpm::new(10_000).expect("one percent is valid"))
            .expect("one percent is non-zero");
    for window in windows {
        meter_evidence::insert_window(
            conn,
            &NewMeterWindow {
                observation_id,
                semantic_key: WindowSemanticKey::new(window.semantic_key),
                scope: WindowScope::AccountWide,
                quota_used: window.quota_used,
                reported_resolution: resolution,
                quantization: QuantizationSemantics::Unknown,
                resets_at: seed_reset_state(window),
                nominal_duration: NominalWindowDuration::from_nanos(window.nominal_duration_nanos),
            },
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_schema::open_migrated;
    use test_support::StateDir;

    /// Two readings of one account six minutes apart: the first carries a
    /// `five_hour` window the capture reported with no `resetsAt`, the second
    /// carries only started windows. The pair is what separates a reading that
    /// must be reinterpreted from one that must not.
    const SOURCE: &str = concat!(
        r#"{"received_at":"2026-08-29T13:35:03Z","account":"claude","tool":"aub-meter","tool_version":"0.1.0","plan":"pro","reading":{"generatedAt":"2026-08-29T13:35:03Z","providers":[{"provider":"claude","windows":[{"id":"five_hour","percentUsed":0,"windowSeconds":18000},{"id":"seven_day","percentUsed":20,"resetsAt":"2026-09-05T00:00:00Z","windowSeconds":604800}]}]}}"#,
        "\n",
        r#"{"received_at":"2026-08-29T13:41:03Z","account":"claude","tool":"aub-meter","tool_version":"0.1.0","plan":"pro","reading":{"generatedAt":"2026-08-29T13:41:03Z","providers":[{"provider":"claude","windows":[{"id":"five_hour","percentUsed":4,"resetsAt":"2026-08-29T18:00:00Z","windowSeconds":18000},{"id":"seven_day","percentUsed":20,"resetsAt":"2026-09-05T00:00:00Z","windowSeconds":604800}]}]}}"#,
        "\n",
    );

    struct Fixture {
        _scratch: StateDir,
        conn: Connection,
        source_path: std::path::PathBuf,
    }

    fn fixture() -> Fixture {
        let scratch = StateDir::new();
        let conn = open_migrated(
            &scratch.path().join("ledger.db"),
            &crate::store::connection::PragmaPolicy {
                busy_timeout: MonotonicDuration::from_millis(1000),
            },
        );
        let source_path = scratch.path().join("seed.jsonl");
        std::fs::write(&source_path, SOURCE).expect("the fixture source must be writable");
        Fixture {
            _scratch: scratch,
            conn,
            source_path,
        }
    }

    fn claude_to_gmail() -> SeedVendorAccountMap {
        let mut map = SeedVendorAccountMap::new();
        map.insert(
            "claude",
            SeedVendorAccount {
                provider: "anthropic".to_string(),
                account: "gmail".to_string(),
            },
        );
        map
    }

    fn at(text: &str) -> UtcTimestamp {
        UtcTimestamp::parse_rfc3339(text).expect("the fixture instant must parse")
    }

    fn run_import(
        conn: &mut Connection,
        source_path: &std::path::Path,
    ) -> SeedArchiveImportSummary {
        let source = crate::seed_archive::read_source(source_path).expect("the source must parse");
        import(
            conn,
            &source,
            &claude_to_gmail(),
            "archive-v1-g1",
            at("2026-09-30T12:00:00Z"),
        )
        .expect("the import must succeed")
    }

    /// Writes the rows the superseded importer wrote for one reading: the same
    /// attempt, evidence and observation identity, with a window whose absent
    /// reset was filled with `generated_at` plus the nominal length. Built here
    /// rather than by running the old binary, because the invented instant is
    /// the only property of that interpretation this correction reads.
    fn plant_v1_reading(conn: &Connection, success: &SeedArchiveSuccessRecord, digest: &str) {
        let account_id = account::observe_account(conn, "anthropic", "gmail", success.received_at)
            .expect("the account must be observable");
        let policy = ResolvedSamplingPolicy {
            ordinary_cadence: MonotonicDuration::from_nanos(SEED_NOMINAL_CADENCE_NANOS),
            freshness_horizon: MonotonicDuration::from_nanos(SEED_NOMINAL_CADENCE_NANOS),
            reset_edge_policy: "none".to_string(),
            retry_backoff_policy: "none".to_string(),
            command_budget: MonotonicDuration::from_nanos(60 * 1_000_000_000),
            policy_algorithm_version: POLICY_ALGORITHM_VERSION.to_string(),
        };
        let policy_snapshot_id = sampling_policy_snapshot::resolve_policy_snapshot(
            conn,
            account_id,
            success.received_at,
            &policy,
        )
        .expect("the policy snapshot must resolve");
        let run_id = sample_run::start_sample_run(
            conn,
            Trigger::Timer,
            success.received_at,
            "seed-archive-import-v1",
        )
        .expect("the sample run must start");
        let attempt_id = meter_attempt::start_meter_attempt(
            conn,
            &NewMeterAttempt {
                run_id,
                account_id,
                provider: "anthropic".to_string(),
                request_started_at: success.received_at,
                credential_context_id: None,
                policy_snapshot_id,
                due_at: success.received_at,
                due_reason: DueReason::OrdinaryCadence,
                due_basis: None,
                provider_contract_id: PROVIDER_CONTRACT.to_string(),
                meter_semantics_id: METER_SEMANTICS.to_string(),
            },
        )
        .expect("the attempt must start");
        meter_attempt::record_meter_attempt_result(
            conn,
            &NewMeterAttemptResult {
                attempt_id,
                completed_at: success.received_at,
                elapsed: MonotonicDuration::from_nanos(0),
                outcome: AttemptOutcome::Success,
                sanitized_error_classification: Some(MARKER_SOURCE.to_string()),
                retry_index: None,
                clock_anomaly: false,
            },
        )
        .expect("the attempt result must record");
        let evidence_id = meter_evidence::insert_response_evidence(
            conn,
            &NewMeterResponseEvidence {
                attempt_id,
                response_classification: MARKER_SOURCE.to_string(),
                received_at: success.received_at,
                provider_observed_at_original: Some(success.generated_at_original.clone()),
                evidence_capsule: success.raw_reading.clone(),
                capsule_schema_version: "seed-archive-reading-v1".to_string(),
                sanitizer_version: "seed-archive-sanitizer-v1".to_string(),
                capture_truncated: false,
            },
        )
        .expect("the evidence must insert");
        let observation_id = meter_evidence::insert_observation(
            conn,
            &NewMeterObservation {
                attempt_id,
                evidence_id,
                account_id,
                provider: "anthropic".to_string(),
                provider_observed_at: Some(success.generated_at),
                received_at: success.received_at,
                measurement_basis: MeasurementBasis::ProviderObserved,
                observed_plan: success.plan.clone(),
                observed_tier: success.plan.clone(),
                adapter_version: AdapterVersion::new(SUPERSEDED_ADAPTER_VERSION),
                provider_contract_id: ProviderContractId::new(PROVIDER_CONTRACT),
                meter_semantics_id: MeterSemanticsId::new(METER_SEMANTICS),
                normalized_fingerprint: format!(
                    "seed:{}:{}:{}:{}",
                    digest, success.source_file, success.source_line, success.vendor
                ),
            },
        )
        .expect("the observation must insert");
        let invented: Vec<SeedArchiveWindow> = success
            .windows
            .iter()
            .map(|window| SeedArchiveWindow {
                resets_at: Some(window.resets_at.unwrap_or_else(|| {
                    UtcTimestamp::from_unix_nanos(
                        success.generated_at.unix_nanos() + window.nominal_duration_nanos as i64,
                    )
                })),
                ..window.clone()
            })
            .collect();
        insert_seed_windows(conn, observation_id, &invented).expect("the windows must insert");
    }

    fn plant_v1_import(conn: &Connection, source_path: &std::path::Path) {
        let source = crate::seed_archive::read_source(source_path).expect("the source must parse");
        for record in &source.records {
            let SeedArchiveRecord::Success(success) = record else {
                continue;
            };
            plant_v1_reading(conn, success, &source.content_digest);
        }
    }

    fn current_windows(
        conn: &Connection,
        received_at: UtcTimestamp,
    ) -> Vec<crate::store::meter_evidence::StoredMeterWindow> {
        let account_id = account::observe_account(conn, "anthropic", "gmail", received_at)
            .expect("the account must exist");
        let attempt_id = existing_attempt(conn, account_id, received_at, PROVIDER_CONTRACT)
            .expect("the attempt lookup must succeed")
            .expect("the reading must have an attempt");
        let evidence_id = evidence_for_attempt(conn, attempt_id)
            .expect("the evidence lookup must succeed")
            .expect("the reading must have evidence");
        let current = meter_evidence::current_observation_id(
            conn,
            evidence_id,
            &MeterSemanticsId::new(METER_SEMANTICS),
        )
        .expect("the selector must be readable")
        .expect("the reading must have a current observation");
        meter_evidence::windows_by_observation(conn, current).expect("the windows must be readable")
    }

    #[test]
    fn a_superseded_interpretation_of_an_unstarted_window_is_reinterpreted_once() {
        let mut fixture = fixture();
        plant_v1_import(&fixture.conn, &fixture.source_path);

        let first = run_import(&mut fixture.conn, &fixture.source_path);
        assert_eq!(first.imported, 0);
        assert_eq!(first.reinterpreted, 1, "the unstarted reading is corrected");
        assert_eq!(first.unchanged, 1, "the started reading is not");

        let corrected = current_windows(&fixture.conn, at("2026-08-29T13:35:03Z"));
        assert_eq!(
            corrected[0].resets_at,
            crate::domain::window::WindowResetState::NotStarted,
            "the unstarted window's reset is no longer an instant"
        );
        assert_eq!(
            corrected[1].resets_at,
            crate::domain::window::WindowResetState::Known(at("2026-09-05T00:00:00Z")),
            "the started window of the same reading keeps its reported reset"
        );

        let superseded_still_present: i64 = fixture
            .conn
            .query_row(
                "SELECT COUNT(*) FROM meter_observation WHERE adapter_version = ?1",
                params![SUPERSEDED_ADAPTER_VERSION],
                |row| row.get(0),
            )
            .expect("the observation count must be readable");
        assert_eq!(
            superseded_still_present, 2,
            "neither superseded interpretation is removed"
        );

        let repeat = run_import(&mut fixture.conn, &fixture.source_path);
        assert_eq!(
            repeat.reinterpreted, 0,
            "a correction is written once, not on every run"
        );
        assert_eq!(repeat.unchanged, 2);
    }

    /// The planted negative: the started reading differs from the unstarted one
    /// only in carrying a `resetsAt`, so an importer that reinterpreted on
    /// adapter version alone would correct it too and count two.
    #[test]
    fn a_started_reading_is_never_reinterpreted() {
        let mut fixture = fixture();
        plant_v1_import(&fixture.conn, &fixture.source_path);
        run_import(&mut fixture.conn, &fixture.source_path);

        let started = current_windows(&fixture.conn, at("2026-08-29T13:41:03Z"));
        assert_eq!(
            started[0].resets_at,
            crate::domain::window::WindowResetState::Known(at("2026-08-29T18:00:00Z"))
        );
        let observations: i64 = fixture
            .conn
            .query_row(
                "SELECT COUNT(*) FROM meter_observation o
                 JOIN meter_response_evidence e ON e.id = o.evidence_id
                 WHERE e.received_at = ?1",
                params![at("2026-08-29T13:41:03Z").unix_nanos()],
                |row| row.get(0),
            )
            .expect("the observation count must be readable");
        assert_eq!(
            observations, 1,
            "the started reading keeps its single interpretation"
        );
    }

    #[test]
    fn a_first_import_stores_an_unstarted_window_with_no_reset() {
        let mut fixture = fixture();
        let summary = run_import(&mut fixture.conn, &fixture.source_path);
        assert_eq!(summary.imported, 2);
        assert_eq!(summary.reinterpreted, 0);

        let windows = current_windows(&fixture.conn, at("2026-08-29T13:35:03Z"));
        assert_eq!(
            windows[0].resets_at,
            crate::domain::window::WindowResetState::NotStarted
        );
        let stored_reset: Option<i64> = fixture
            .conn
            .query_row(
                "SELECT resets_at FROM meter_window WHERE id = ?1",
                params![windows[0].row_id.value()],
                |row| row.get(0),
            )
            .expect("the window row must be readable");
        assert_eq!(stored_reset, None, "the stored column itself is NULL");
    }

    #[test]
    fn the_planted_fixture_carries_the_invented_reset_the_correction_removes() {
        let fixture = fixture();
        plant_v1_import(&fixture.conn, &fixture.source_path);
        let planted = current_windows(&fixture.conn, at("2026-08-29T13:35:03Z"));
        assert_eq!(
            planted[0].resets_at,
            crate::domain::window::WindowResetState::Known(at("2026-08-29T18:35:03Z")),
            "the fixture must reproduce the superseded interpretation, or the correction proves nothing"
        );
    }
}
