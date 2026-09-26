//! Atomic persistence for the legacy quota-ledger import.
//!
//! Legacy rows are deliberately kept outside ordinary sampling coverage: they
//! establish a historical meter timeline, not evidence that `aub`'s scheduler
//! attempted every opportunity in that interval.

use rusqlite::{Connection, OptionalExtension, params};

use crate::domain::attempt::AttemptOutcome;
use crate::domain::ids::{AdapterVersion, MeterSemanticsId, ProviderContractId};
use crate::domain::time::{MeasurementBasis, MonotonicDuration, UtcTimestamp};
use crate::domain::window::{
    NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
    WindowSemanticKey,
};
use crate::error::Error;
use crate::legacy_meter::{LegacyMeterRecord, ParsedLegacyMeterSource};

use super::account;
use super::ingest_quarantine::{self, NewQuarantineItem};
use super::ledger_generation;
use super::meter_attempt::{self, DueReason, NewMeterAttempt, NewMeterAttemptResult};
use super::meter_evidence::{self, NewMeterObservation, NewMeterResponseEvidence, NewMeterWindow};
use super::sample_run::{self, Trigger};
use super::sampling_policy_snapshot::{self, ResolvedSamplingPolicy};
use super::session_account_marker::{
    self, EvidenceDesignation, MarkerSource, NewSessionAccountMarker, SourceOrderingKey,
};

const PROVIDER: &str = "anthropic";
const MARKER_SOURCE: &str = "legacy_meter_series";
const ADAPTER_VERSION: &str = "legacy-meter-import-v1";
const PROVIDER_CONTRACT: &str = "legacy-quota-ledger-jsonl-v1";
const METER_SEMANTICS: &str = "legacy-account-windows-v1";
const SESSION_NAMESPACE: &str = "legacy-meter";

/// The parser name recorded on every quarantine row this importer writes.
pub const QUARANTINE_PARSER: &str = "legacy-meter";

/// The failure class of a source line naming an account no `[[accounts]]`
/// entry configures. Such a line is neither malformed nor importable: creating
/// an account row for it would invent an account the operator never declared.
pub const UNCONFIGURED_ACCOUNT_FAILURE_CLASS: &str = "unconfigured_account";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportSummary {
    pub imported: u64,
    pub unchanged: u64,
    /// Readings at or after their account's native cutoff. They are classified
    /// on every run, so the figure is a property of the source and the ledger
    /// rather than of how often the import has been attempted.
    pub superseded_by_native: u64,
    pub quarantined: u64,
}

/// Writes the source rows native sampling does not already cover, and every
/// row's marker, in one transaction. The `(source_digest, source_line)` key is
/// the idempotence boundary for a fully imported row; a marker imported past
/// the cutoff has no row there, so its own identity in `session_account_marker`
/// is what a rerun recognises.
///
/// Three rules decide what a row becomes, and all three exist because the
/// legacy series overlaps the live one in time:
///
/// - **Cutoff.** Per account, the earliest native `meter_attempt` start. A
///   reading at or after it is already measured by the sampler, and importing
///   it would lay a second, disagreeing series over the first. Legacy contracts
///   are excluded from the cutoff, so a previous import of either legacy source
///   never becomes its own cutoff.
/// - **Markers are exempt.** A session-to-account marker is evidence native
///   sampling does not produce at all, so it imports for the whole series.
/// - **An unconfigured account is quarantined**, never invented. `configured_accounts`
///   carries the `[[accounts]]` names for this provider.
pub fn import(
    conn: &mut Connection,
    source: &ParsedLegacyMeterSource,
    configured_accounts: &[String],
    verified_backup_id: &str,
    imported_at: UtcTimestamp,
) -> Result<ImportSummary, Error> {
    let configured: std::collections::BTreeSet<&str> =
        configured_accounts.iter().map(String::as_str).collect();
    let unconfigured_records = source
        .records
        .iter()
        .filter(|record| !configured.contains(record.account.as_str()))
        .count() as u64;
    let quarantined = source.records_quarantined + unconfigured_records;
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| Error::Store(format!("cannot open legacy import transaction: {error}")))?;
    tx.execute(
        "INSERT OR IGNORE INTO legacy_meter_import (
            source_digest, verified_backup_id, imported_at, records_read, records_quarantined
         ) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            source.content_digest,
            verified_backup_id,
            imported_at.unix_nanos(),
            source.records_read as i64,
            quarantined as i64,
        ],
    )
    .map_err(|error| Error::Store(format!("cannot record legacy import provenance: {error}")))?;

    // Do not record a new invocation on a no-op rerun.  The source-row key is
    // the idempotence boundary for the complete import footprint, not merely
    // the observation and marker tables.
    let mut run = None;
    let mut imported = 0;
    let mut unchanged = 0;
    let mut superseded_by_native = 0;
    let mut rows_written = 0;
    let mut cutoffs: std::collections::BTreeMap<&str, Option<UtcTimestamp>> =
        std::collections::BTreeMap::new();
    for record in &source.records {
        if !configured.contains(record.account.as_str()) {
            quarantine_unconfigured_account(&tx, &source.content_digest, record, imported_at)?;
            continue;
        }
        let cutoff = match cutoffs.get(record.account.as_str()) {
            Some(cutoff) => *cutoff,
            None => {
                let cutoff = native_cutoff_for_account(&tx, &record.account)?;
                cutoffs.insert(record.account.as_str(), cutoff);
                cutoff
            }
        };
        if cutoff.is_some_and(|cutoff| record.timestamp >= cutoff) {
            superseded_by_native += 1;
            if !marker_exists(&tx, record)? {
                let account_id =
                    account::observe_account(&tx, PROVIDER, &record.account, record.timestamp)?;
                insert_record_marker(&tx, record, account_id)?;
                rows_written += 1;
            }
            continue;
        }
        if imported_record_exists(&tx, &source.content_digest, record.source_line)? {
            unchanged += 1;
            continue;
        }
        let run_id = match run {
            Some(run_id) => run_id,
            None => {
                let run_id = sample_run::start_sample_run(
                    &tx,
                    Trigger::Hook,
                    imported_at,
                    "legacy-meter-import-v1",
                )?;
                run = Some(run_id);
                run_id
            }
        };
        import_record(&tx, run_id, &source.content_digest, record)?;
        imported += 1;
        rows_written += 1;
    }
    if rows_written > 0 {
        ledger_generation::advance(&tx)?;
    }
    tx.commit()
        .map_err(|error| Error::Store(format!("cannot commit legacy meter import: {error}")))?;
    Ok(ImportSummary {
        imported,
        unchanged,
        superseded_by_native,
        quarantined,
    })
}

/// The earliest native meter attempt for an account, or `None` when the
/// sampler has never reached it. Both legacy contracts are excluded: a row an
/// importer wrote is history, not coverage, and letting it stand as a cutoff
/// would make a second import of the same source import nothing.
fn native_cutoff_for_account(
    conn: &Connection,
    account_name: &str,
) -> Result<Option<UtcTimestamp>, Error> {
    let Some(account_id) = account::account_id_by_identity(conn, PROVIDER, account_name)? else {
        return Ok(None);
    };
    conn.query_row(
        "SELECT MIN(request_started_at) FROM meter_attempt
         WHERE account_id = ?1 AND provider_contract_id NOT IN (?2, ?3)",
        params![
            account_id.value(),
            PROVIDER_CONTRACT,
            super::seed_archive_import::PROVIDER_CONTRACT,
        ],
        |row| row.get::<_, Option<i64>>(0),
    )
    .map(|nanos| nanos.map(UtcTimestamp::from_unix_nanos))
    .map_err(|error| Error::Store(format!("cannot read the native meter cutoff: {error}")))
}

/// Whether this source line's marker is already stored. A marker imported past
/// the cutoff has no `legacy_meter_import_record` row to key on, so its own
/// columns carry its identity.
fn marker_exists(conn: &Connection, record: &LegacyMeterRecord) -> Result<bool, Error> {
    conn.query_row(
        "SELECT 1 FROM session_account_marker
         WHERE marker_source = ?1 AND session_source = ?2 AND session_native = ?3
           AND observed_at = ?4 AND source_ordering_key = ?5 AND logical_account = ?6",
        params![
            MARKER_SOURCE,
            SESSION_NAMESPACE,
            record.session_id,
            record.timestamp.unix_nanos(),
            record.source_line as i64,
            record.account,
        ],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(|error| Error::Store(format!("cannot read legacy marker identity: {error}")))
}

/// Records a source line whose account no `[[accounts]]` entry names. The
/// quarantine is keyed by digest and line, so the same line recurring on a
/// rerun updates one row rather than adding a second.
fn quarantine_unconfigured_account(
    conn: &Connection,
    source_digest: &str,
    record: &LegacyMeterRecord,
    observed_at: UtcTimestamp,
) -> Result<(), Error> {
    use sha2::{Digest, Sha256};
    let identity = format!("{source_digest}|{}|{}", record.source_line, record.account);
    ingest_quarantine::record_quarantine(
        conn,
        &NewQuarantineItem {
            // The source is named by digest, never by path: the importer's
            // output and its durable rows must not carry a filesystem location.
            source_file: format!("legacy-meter:{source_digest}"),
            byte_offset: None,
            line_number: Some(record.source_line),
            parser: QUARANTINE_PARSER.to_owned(),
            failure_class: UNCONFIGURED_ACCOUNT_FAILURE_CLASS.to_owned(),
            excerpt_hash: format!("{:x}", Sha256::digest(identity.as_bytes())),
            excerpt: None,
            observed_at,
        },
    )?;
    Ok(())
}

fn imported_record_exists(
    conn: &Connection,
    source_digest: &str,
    source_line: u64,
) -> Result<bool, Error> {
    conn.query_row(
        "SELECT 1 FROM legacy_meter_import_record WHERE source_digest = ?1 AND source_line = ?2",
        params![source_digest, source_line as i64],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(|error| Error::Store(format!("cannot read legacy import identity: {error}")))
}

fn import_record(
    conn: &Connection,
    run: sample_run::SampleRunId,
    source_digest: &str,
    record: &LegacyMeterRecord,
) -> Result<(), Error> {
    let account_id = account::observe_account(conn, PROVIDER, &record.account, record.timestamp)?;
    let policy = sampling_policy_snapshot::resolve_policy_snapshot(
        conn,
        account_id,
        record.timestamp,
        &legacy_policy(),
    )?;
    let attempt = meter_attempt::start_meter_attempt(
        conn,
        &NewMeterAttempt {
            run_id: run,
            account_id,
            provider: PROVIDER.to_owned(),
            request_started_at: record.timestamp,
            credential_context_id: None,
            policy_snapshot_id: policy,
            due_at: record.timestamp,
            due_reason: DueReason::ForcedOrManual,
            due_basis: None,
            provider_contract_id: PROVIDER_CONTRACT.to_owned(),
            meter_semantics_id: METER_SEMANTICS.to_owned(),
        },
    )?;
    meter_attempt::record_meter_attempt_result(
        conn,
        &NewMeterAttemptResult {
            attempt_id: attempt,
            completed_at: record.timestamp,
            elapsed: MonotonicDuration::from_nanos(0),
            outcome: AttemptOutcome::Success,
            sanitized_error_classification: Some("legacy_import".to_owned()),
            retry_index: None,
            clock_anomaly: false,
        },
    )?;
    let evidence_id = meter_evidence::insert_response_evidence(
        conn,
        &NewMeterResponseEvidence {
            attempt_id: attempt,
            response_classification: "legacy_import".to_owned(),
            received_at: record.timestamp,
            provider_observed_at_original: None,
            evidence_capsule: record.evidence_capsule.clone(),
            capsule_schema_version: "legacy-meter-jsonl-v1".to_owned(),
            sanitizer_version: "legacy-source-fields-v1".to_owned(),
            capture_truncated: false,
        },
    )?;
    let observation_id = meter_evidence::insert_observation(
        conn,
        &NewMeterObservation {
            attempt_id: attempt,
            evidence_id,
            account_id,
            provider: PROVIDER.to_owned(),
            provider_observed_at: match record.measurement_basis {
                MeasurementBasis::ProviderObserved => Some(record.timestamp),
                MeasurementBasis::LocallyReceived | MeasurementBasis::OlderOfTheTwo => None,
            },
            received_at: record.timestamp,
            measurement_basis: record.measurement_basis,
            observed_plan: record.tier.clone(),
            observed_tier: record.tier.clone(),
            adapter_version: AdapterVersion::new(ADAPTER_VERSION),
            provider_contract_id: ProviderContractId::new(PROVIDER_CONTRACT),
            meter_semantics_id: MeterSemanticsId::new(METER_SEMANTICS),
            normalized_fingerprint: format!("legacy:{}:{}", source_digest, record.source_line),
        },
    )?;
    let resolution = ReportedResolution::new(
        crate::domain::quota::QuotaFractionPpm::new(10_000).expect("one percent is valid"),
    )
    .expect("one percent is non-zero");
    for window in &record.windows {
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
                nominal_duration: NominalWindowDuration::from_nanos(window.nominal_duration_nanos),
            },
        )?;
    }
    let marker_id = insert_record_marker(conn, record, account_id)?;
    conn.execute(
        "INSERT INTO legacy_meter_import_record (source_digest, source_line, observation_id, marker_id)
         VALUES (?1, ?2, ?3, ?4)",
        params![source_digest, record.source_line as i64, observation_id.value(), marker_id.value()],
    )
    .map_err(|error| Error::Store(format!("cannot record legacy import row identity: {error}")))?;
    Ok(())
}

/// The session-to-account marker a source line carries. Written for every
/// configured line, on both sides of the cutoff, because it is evidence native
/// sampling does not produce: it names the account a session ran under, and
/// nothing later can reconstruct that.
fn insert_record_marker(
    conn: &Connection,
    record: &LegacyMeterRecord,
    account_id: account::AccountId,
) -> Result<session_account_marker::MarkerId, Error> {
    let session_id = crate::domain::ids::SessionId::new(
        crate::domain::ids::SourceNamespace::new(SESSION_NAMESPACE),
        crate::domain::ids::NativeSessionId::new(record.session_id.clone()),
    );
    session_account_marker::insert_marker(
        conn,
        &NewSessionAccountMarker {
            session_id,
            observed_at: record.timestamp,
            source_ordering_key: Some(SourceOrderingKey::new(record.source_line as i64)),
            logical_account: record.account.clone(),
            resolved_account_id: Some(account_id),
            marker_source: MarkerSource::new(MARKER_SOURCE),
            run_id: None,
            evidence_designation: EvidenceDesignation::ExplicitLauncherOrHook,
        },
    )
}

fn legacy_policy() -> ResolvedSamplingPolicy {
    ResolvedSamplingPolicy {
        ordinary_cadence: MonotonicDuration::from_seconds(60 * 60),
        freshness_horizon: MonotonicDuration::from_seconds(60 * 60),
        reset_edge_policy: "legacy-import-not-a-schedule".to_owned(),
        retry_backoff_policy: "legacy-import-none".to_owned(),
        command_budget: MonotonicDuration::from_seconds(1),
        policy_algorithm_version: "legacy-meter-import-v1".to_owned(),
    }
}

pub fn legacy_observation_count_between(
    conn: &Connection,
    account_id: account::AccountId,
    start: UtcTimestamp,
    end: UtcTimestamp,
) -> Result<u64, Error> {
    conn.query_row(
        "SELECT count(*) FROM legacy_meter_import_record lir
         JOIN meter_observation mo ON mo.id = lir.observation_id
         WHERE mo.account_id = ?1 AND mo.received_at >= ?2 AND mo.received_at < ?3",
        params![account_id.value(), start.unix_nanos(), end.unix_nanos()],
        |row| row.get(0),
    )
    .map_err(|error| Error::Store(format!("cannot count legacy observations: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};

    use crate::legacy_meter::LegacyWindow;

    use crate::store::sample_run::count_sample_runs;

    fn source() -> ParsedLegacyMeterSource {
        let timestamp = UtcTimestamp::parse_rfc3339("2026-08-15T18:40:38Z")
            .expect("fixture timestamp must parse");
        let quota = QuotaUsed::new(QuotaFractionPpm::new(280_000).expect("valid quota"));
        ParsedLegacyMeterSource {
            content_digest: "a".repeat(64),
            records_read: 1,
            records_quarantined: 0,
            records: vec![LegacyMeterRecord {
                source_line: 1,
                timestamp,
                measurement_basis: MeasurementBasis::LocallyReceived,
                session_id: "legacy-session".to_owned(),
                account: "primary".to_owned(),
                tier: Some("pro".to_owned()),
                windows: vec![
                    LegacyWindow {
                        semantic_key: "five_hour",
                        quota_used: quota,
                        resets_at: UtcTimestamp::parse_rfc3339("2026-08-15T20:00:00Z")
                            .expect("fixture reset must parse"),
                        nominal_duration_nanos: 5 * 60 * 60 * 1_000_000_000,
                    },
                    LegacyWindow {
                        semantic_key: "seven_day",
                        quota_used: quota,
                        resets_at: UtcTimestamp::parse_rfc3339("2026-08-22T00:00:00Z")
                            .expect("fixture reset must parse"),
                        nominal_duration_nanos: 7 * 24 * 60 * 60 * 1_000_000_000,
                    },
                ],
                evidence_capsule: "{\"format\":\"fixture\"}".to_owned(),
            }],
        }
    }

    struct ScratchDir(std::path::PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "aub-legacy-import-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            ));
            std::fs::create_dir_all(&path).expect("scratch dir must be creatable");
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    #[test]
    fn import_is_idempotent_and_preserves_legacy_provenance() {
        let scratch = ScratchDir::new();
        let db_path = scratch.path().join("legacy-import.db");
        let mut conn = crate::store::test_schema::open_migrated(
            &db_path,
            &crate::store::connection::PragmaPolicy {
                busy_timeout: crate::domain::time::MonotonicDuration::from_millis(1_000),
            },
        );
        let imported_at = UtcTimestamp::from_unix_nanos(1_000);

        let configured = vec!["primary".to_owned()];
        let first = import(
            &mut conn,
            &source(),
            &configured,
            "backup-verified-1",
            imported_at,
        )
        .expect("first import must succeed");
        let repeated = import(
            &mut conn,
            &source(),
            &configured,
            "backup-verified-1",
            imported_at,
        )
        .expect("identical import must succeed");

        assert_eq!(first.imported, 1);
        assert_eq!(repeated.imported, 0);
        assert_eq!(repeated.unchanged, 1);
        assert_eq!(meter_evidence::count_observations(&conn).unwrap(), 1);
        assert_eq!(count_sample_runs(&conn).unwrap(), 1);
        let (all_attempts, terminal_attempts) = meter_attempt::count_attempts(&conn).unwrap();
        assert_eq!((all_attempts, terminal_attempts), (1, 1));
        assert_eq!(
            conn.query_row(
                "SELECT marker_source FROM session_account_marker",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            MARKER_SOURCE,
        );
        assert_eq!(
            conn.query_row(
                "SELECT measurement_basis FROM meter_observation",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            "locally_received",
        );
        assert_eq!(
            conn.query_row("SELECT observed_plan FROM meter_observation", [], |row| row
                .get::<_, String>(0),)
                .unwrap(),
            "pro",
        );
        let resets = conn
            .prepare("SELECT resets_at FROM meter_window ORDER BY semantic_key")
            .unwrap()
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            resets,
            vec![
                UtcTimestamp::parse_rfc3339("2026-08-15T20:00:00Z")
                    .unwrap()
                    .unix_nanos(),
                UtcTimestamp::parse_rfc3339("2026-08-22T00:00:00Z")
                    .unwrap()
                    .unix_nanos(),
            ],
        );
        let account_id = account::account_id_by_identity(&conn, PROVIDER, "primary")
            .unwrap()
            .expect("legacy account must exist");
        assert!(
            meter_attempt::attempts_with_outcomes_for_account_between(
                &conn,
                account_id,
                UtcTimestamp::from_unix_nanos(0),
                UtcTimestamp::from_unix_nanos(i64::MAX),
            )
            .unwrap()
            .is_empty(),
            "legacy evidence must not raise ordinary attempt coverage",
        );
    }
}
