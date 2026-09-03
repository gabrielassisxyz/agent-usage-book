//! The `meter_response_evidence`, `meter_observation`, `meter_window` and
//! `meter_observation_preference` tables: evidence and interpretation as
//! separate immutable records (PLAN.md sections 6, 12.4, 12.5, 13.1,
//! invariant 25).
//!
//! The evidence row holds the sanitized quota evidence capsule exactly as
//! captured, with its content hash; an observation is one adapter's immutable
//! interpretation of that evidence; the window rows hold the provider's
//! reported values exactly as expressed. A corrected adapter writes a new
//! interpretation against the same evidence and never overwrites the earlier
//! one - the recovery guarantee of the whole substrate. The derived
//! preference selector points at the one current interpretation per evidence
//! row and semantics version, and is the only table here that may be updated,
//! through the narrowly scoped `switch_current_observation` operation.
//!
//! This module exposes insert and read only for the three irreplaceable
//! tables; their triggers reject every `UPDATE` and `DELETE`.
//!
//! May not depend on:
//! - HTTP or provider semantics
//! - presentation

use rusqlite::{OptionalExtension, params};

use crate::domain::ids::{AdapterVersion, MeterSemanticsId, ProviderContractId};
use crate::domain::time::{MeasurementBasis, UtcTimestamp};
use crate::domain::window::{
    ModelId, NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
    WindowSemanticKey,
};
use crate::error::Error;
use crate::store::account::AccountId;
use crate::store::meter_attempt::MeterAttemptRowId;

/// A `meter_response_evidence` row's identity: its SQLite rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EvidenceRowId(i64);

impl EvidenceRowId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

/// A `meter_observation` row's identity: its SQLite rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObservationRowId(i64);

impl ObservationRowId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

/// A `meter_window` row's identity: its SQLite rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowRowId(i64);

impl WindowRowId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

/// The durable sanitized evidence of what the remote source returned, written
/// before any semantic normalization. The capsule is the quota-relevant
/// subtree plus the raw source lexemes, exactly as captured; the content hash
/// is computed here over the capsule bytes and stored beside them, so a later
/// reader can prove the capsule was not altered in storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMeterResponseEvidence {
    pub attempt_id: MeterAttemptRowId,
    pub response_classification: String,
    pub received_at: UtcTimestamp,
    /// The provider's observation timestamp as originally represented, where
    /// the source supplied one. Kept in its original spelling because the
    /// evidence record is what a corrected parser reinterprets.
    pub provider_observed_at_original: Option<String>,
    pub evidence_capsule: String,
    pub capsule_schema_version: String,
    pub sanitizer_version: String,
    pub capture_truncated: bool,
}

/// One stored evidence row, read back exactly as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMeterResponseEvidence {
    pub row_id: EvidenceRowId,
    pub attempt_id: MeterAttemptRowId,
    pub response_classification: String,
    pub received_at: UtcTimestamp,
    pub provider_observed_at_original: Option<String>,
    pub evidence_capsule: String,
    pub capsule_schema_version: String,
    pub sanitizer_version: String,
    pub content_hash: String,
    pub capture_truncated: bool,
}

/// One immutable interpretation of one evidence row by one adapter and
/// semantics version. More than one observation may reference the same
/// evidence row; the preference selector names the current one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMeterObservation {
    pub attempt_id: MeterAttemptRowId,
    pub evidence_id: EvidenceRowId,
    pub account_id: AccountId,
    pub provider: String,
    pub provider_observed_at: Option<UtcTimestamp>,
    pub received_at: UtcTimestamp,
    pub measurement_basis: MeasurementBasis,
    pub observed_plan: Option<String>,
    pub observed_tier: Option<String>,
    pub adapter_version: AdapterVersion,
    pub provider_contract_id: ProviderContractId,
    pub meter_semantics_id: MeterSemanticsId,
    pub normalized_fingerprint: String,
}

/// One stored observation row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMeterObservation {
    pub row_id: ObservationRowId,
    pub attempt_id: MeterAttemptRowId,
    pub evidence_id: EvidenceRowId,
    pub account_id: AccountId,
    pub provider: String,
    pub provider_observed_at: Option<UtcTimestamp>,
    pub received_at: UtcTimestamp,
    pub measurement_basis: MeasurementBasis,
    pub observed_plan: Option<String>,
    pub observed_tier: Option<String>,
    pub adapter_version: AdapterVersion,
    pub provider_contract_id: ProviderContractId,
    pub meter_semantics_id: MeterSemanticsId,
    pub normalized_fingerprint: String,
}

/// One provider-reported quota constraint, stored exactly as expressed: the
/// reported value, the resolution it was reported at, and the quantization
/// semantics that resolution claims. No derived "effective remaining" value
/// is persisted anywhere in these tables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMeterWindow {
    pub observation_id: ObservationRowId,
    pub semantic_key: WindowSemanticKey,
    pub scope: WindowScope,
    pub quota_used: crate::domain::quota::QuotaUsed,
    pub reported_resolution: ReportedResolution,
    pub quantization: QuantizationSemantics,
    pub resets_at: UtcTimestamp,
    pub nominal_duration: NominalWindowDuration,
}

/// One stored window row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMeterWindow {
    pub row_id: WindowRowId,
    pub observation_id: ObservationRowId,
    pub semantic_key: WindowSemanticKey,
    pub scope: WindowScope,
    pub quota_used: crate::domain::quota::QuotaUsed,
    pub reported_resolution: ReportedResolution,
    pub quantization: QuantizationSemantics,
    pub resets_at: UtcTimestamp,
    pub nominal_duration: NominalWindowDuration,
}

/// The hex-encoded SHA-256 of the evidence capsule bytes: the stored proof
/// that the capsule survived storage unaltered.
pub fn content_hash_of(capsule: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(capsule.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

const INSERT_EVIDENCE: &str = "
INSERT INTO meter_response_evidence (
    attempt_id, response_classification, received_at, provider_observed_at_original,
    evidence_capsule, capsule_schema_version, sanitizer_version, content_hash, capture_truncated
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) RETURNING id";

/// Writes the durable sanitized evidence of one attempt's response. The
/// content hash is computed here over the capsule bytes, so the stored hash
/// and the stored capsule always agree by construction.
pub fn insert_response_evidence(
    conn: &rusqlite::Connection,
    evidence: &NewMeterResponseEvidence,
) -> Result<EvidenceRowId, Error> {
    let content_hash = content_hash_of(&evidence.evidence_capsule);
    conn.query_row(
        INSERT_EVIDENCE,
        params![
            evidence.attempt_id.value(),
            evidence.response_classification,
            evidence.received_at.unix_nanos(),
            evidence.provider_observed_at_original,
            evidence.evidence_capsule,
            evidence.capsule_schema_version,
            evidence.sanitizer_version,
            content_hash,
            evidence.capture_truncated as i64,
        ],
        |row| row.get(0),
    )
    .map(EvidenceRowId::new)
    .map_err(|e| Error::Store(format!("cannot record the meter response evidence: {e}")))
}

const SELECT_EVIDENCE_COLUMNS: &str = "
    id, attempt_id, response_classification, received_at, provider_observed_at_original,
    evidence_capsule, capsule_schema_version, sanitizer_version, content_hash, capture_truncated";

fn row_to_evidence(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredMeterResponseEvidence> {
    Ok(StoredMeterResponseEvidence {
        row_id: EvidenceRowId::new(row.get("id")?),
        attempt_id: MeterAttemptRowId::new(row.get("attempt_id")?),
        response_classification: row.get("response_classification")?,
        received_at: UtcTimestamp::from_unix_nanos(row.get("received_at")?),
        provider_observed_at_original: row.get("provider_observed_at_original")?,
        evidence_capsule: row.get("evidence_capsule")?,
        capsule_schema_version: row.get("capsule_schema_version")?,
        sanitizer_version: row.get("sanitizer_version")?,
        content_hash: row.get("content_hash")?,
        capture_truncated: row.get::<_, i64>("capture_truncated")? == 1,
    })
}

/// Reads one evidence row by its rowid, or `None` when there is no such row.
pub fn evidence_by_row_id(
    conn: &rusqlite::Connection,
    row_id: EvidenceRowId,
) -> Result<Option<StoredMeterResponseEvidence>, Error> {
    conn.query_row(
        &format!("SELECT {SELECT_EVIDENCE_COLUMNS} FROM meter_response_evidence WHERE id = ?1"),
        params![row_id.value()],
        row_to_evidence,
    )
    .optional()
    .map_err(|e| {
        Error::Store(format!(
            "cannot read meter response evidence {row_id:?}: {e}"
        ))
    })
}

/// Reads the newest evidence row of one attempt, or `None` when the attempt
/// carries no evidence (a failure that never received a response). The rowid
/// order is the insert order; one attempt's evidence arrives in one commit.
pub fn newest_evidence_for_attempt(
    conn: &rusqlite::Connection,
    attempt_id: MeterAttemptRowId,
) -> Result<Option<EvidenceRowId>, Error> {
    conn.query_row(
        "SELECT id FROM meter_response_evidence WHERE attempt_id = ?1 ORDER BY id DESC LIMIT 1",
        params![attempt_id.value()],
        |row| row.get::<_, i64>(0).map(EvidenceRowId::new),
    )
    .optional()
    .map_err(|e| {
        Error::Store(format!(
            "cannot read the evidence of attempt {}: {e}",
            attempt_id.value()
        ))
    })
}

/// The single database spelling of a measurement basis, and back. One
/// definition here and nowhere else.
pub mod measurement_basis_sql {
    use crate::domain::time::MeasurementBasis;
    use crate::error::Error;

    pub fn as_sql(basis: MeasurementBasis) -> &'static str {
        match basis {
            MeasurementBasis::ProviderObserved => "provider_observed",
            MeasurementBasis::LocallyReceived => "locally_received",
            MeasurementBasis::OlderOfTheTwo => "older_of_the_two",
        }
    }

    pub fn from_sql(code: &str) -> Result<MeasurementBasis, Error> {
        match code {
            "provider_observed" => Ok(MeasurementBasis::ProviderObserved),
            "locally_received" => Ok(MeasurementBasis::LocallyReceived),
            "older_of_the_two" => Ok(MeasurementBasis::OlderOfTheTwo),
            other => Err(Error::Store(format!(
                "unknown measurement basis stored in the database: {other:?}"
            ))),
        }
    }
}

const INSERT_OBSERVATION: &str = "
INSERT INTO meter_observation (
    attempt_id, evidence_id, account_id, provider, provider_observed_at, received_at,
    measurement_basis, observed_plan, observed_tier, adapter_version, provider_contract_id,
    meter_semantics_id, normalized_fingerprint
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) RETURNING id";

/// Writes one immutable interpretation of an evidence row. The first
/// observation for an (evidence, semantics) pair becomes the current one: the
/// preference selector row is created here, pointing at this observation,
/// when none exists yet. A later observation for the same pair does not
/// disturb the selector; switching it is `switch_current_observation`'s job.
pub fn insert_observation(
    conn: &rusqlite::Connection,
    observation: &NewMeterObservation,
) -> Result<ObservationRowId, Error> {
    let row_id = conn
        .query_row(
            INSERT_OBSERVATION,
            params![
                observation.attempt_id.value(),
                observation.evidence_id.value(),
                observation.account_id.value(),
                observation.provider,
                observation.provider_observed_at.map(|t| t.unix_nanos()),
                observation.received_at.unix_nanos(),
                measurement_basis_sql::as_sql(observation.measurement_basis),
                observation.observed_plan,
                observation.observed_tier,
                observation.adapter_version.as_str(),
                observation.provider_contract_id.as_str(),
                observation.meter_semantics_id.as_str(),
                observation.normalized_fingerprint,
            ],
            |row| row.get(0),
        )
        .map(ObservationRowId::new)
        .map_err(|e| Error::Store(format!("cannot record the meter observation: {e}")))?;

    conn.execute(
        "INSERT OR IGNORE INTO meter_observation_preference (
            evidence_id, meter_semantics_id, current_observation_id
        ) VALUES (?1, ?2, ?3)",
        params![
            observation.evidence_id.value(),
            observation.meter_semantics_id.as_str(),
            row_id.value(),
        ],
    )
    .map_err(|e| Error::Store(format!("cannot record the observation preference: {e}")))?;

    Ok(row_id)
}

const SELECT_OBSERVATION_COLUMNS: &str = "
    id, attempt_id, evidence_id, account_id, provider, provider_observed_at, received_at,
    measurement_basis, observed_plan, observed_tier, adapter_version, provider_contract_id,
    meter_semantics_id, normalized_fingerprint";

fn row_to_observation(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredMeterObservation> {
    let basis_string: String = row.get("measurement_basis")?;
    let measurement_basis = measurement_basis_sql::from_sql(&basis_string).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            7, // measurement_basis, by position in SELECT_OBSERVATION_COLUMNS
            rusqlite::types::Type::Text,
            Box::new(e),
        )
    })?;
    Ok(StoredMeterObservation {
        row_id: ObservationRowId::new(row.get("id")?),
        attempt_id: MeterAttemptRowId::new(row.get("attempt_id")?),
        evidence_id: EvidenceRowId::new(row.get("evidence_id")?),
        account_id: AccountId::new(row.get("account_id")?),
        provider: row.get("provider")?,
        provider_observed_at: row
            .get::<_, Option<i64>>("provider_observed_at")?
            .map(UtcTimestamp::from_unix_nanos),
        received_at: UtcTimestamp::from_unix_nanos(row.get("received_at")?),
        measurement_basis,
        observed_plan: row.get("observed_plan")?,
        observed_tier: row.get("observed_tier")?,
        adapter_version: AdapterVersion::new(row.get::<_, String>("adapter_version")?),
        provider_contract_id: ProviderContractId::new(
            row.get::<_, String>("provider_contract_id")?,
        ),
        meter_semantics_id: MeterSemanticsId::new(row.get::<_, String>("meter_semantics_id")?),
        normalized_fingerprint: row.get("normalized_fingerprint")?,
    })
}

/// Reads one observation row by its rowid, or `None` when there is no such row.
pub fn observation_by_row_id(
    conn: &rusqlite::Connection,
    row_id: ObservationRowId,
) -> Result<Option<StoredMeterObservation>, Error> {
    conn.query_row(
        &format!("SELECT {SELECT_OBSERVATION_COLUMNS} FROM meter_observation WHERE id = ?1"),
        params![row_id.value()],
        row_to_observation,
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot read meter observation {row_id:?}: {e}")))
}

const INSERT_WINDOW: &str = "
INSERT INTO meter_window (
    observation_id, semantic_key, scope_kind, scoped_model, quota_used_ppm,
    reported_resolution_ppm, quantization, resets_at, nominal_duration_nanos
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) RETURNING id";

/// Writes one provider-reported quota constraint, stored exactly as the
/// provider expressed it. No derived remaining value is computed here.
pub fn insert_window(
    conn: &rusqlite::Connection,
    window: &NewMeterWindow,
) -> Result<WindowRowId, Error> {
    let (scope_kind, scoped_model) = match &window.scope {
        WindowScope::AccountWide => ("account_wide", None),
        WindowScope::ModelSpecific(model) => ("model_specific", Some(model.as_str())),
    };
    conn.query_row(
        INSERT_WINDOW,
        params![
            window.observation_id.value(),
            window.semantic_key.as_str(),
            scope_kind,
            scoped_model,
            window.quota_used.as_ppm().get() as i64,
            window.reported_resolution.as_ppm().get() as i64,
            quantization_sql::as_sql(window.quantization),
            window.resets_at.unix_nanos(),
            window.nominal_duration.as_nanos() as i64,
        ],
        |row| row.get(0),
    )
    .map(WindowRowId::new)
    .map_err(|e| Error::Store(format!("cannot record the meter window: {e}")))
}

/// The single database spelling of a quantization semantics, and back.
pub mod quantization_sql {
    use crate::domain::window::QuantizationSemantics;
    use crate::error::Error;

    pub fn as_sql(quantization: QuantizationSemantics) -> &'static str {
        match quantization {
            QuantizationSemantics::Exact => "exact",
            QuantizationSemantics::RoundedToNearest => "rounded_to_nearest",
            QuantizationSemantics::RoundedDown => "rounded_down",
            QuantizationSemantics::RoundedUp => "rounded_up",
            QuantizationSemantics::Unknown => "unknown",
        }
    }

    pub fn from_sql(code: &str) -> Result<QuantizationSemantics, Error> {
        match code {
            "exact" => Ok(QuantizationSemantics::Exact),
            "rounded_to_nearest" => Ok(QuantizationSemantics::RoundedToNearest),
            "rounded_down" => Ok(QuantizationSemantics::RoundedDown),
            "rounded_up" => Ok(QuantizationSemantics::RoundedUp),
            "unknown" => Ok(QuantizationSemantics::Unknown),
            other => Err(Error::Store(format!(
                "unknown quantization semantics stored in the database: {other:?}"
            ))),
        }
    }
}

const SELECT_WINDOW_COLUMNS: &str = "
    id, observation_id, semantic_key, scope_kind, scoped_model, quota_used_ppm,
    reported_resolution_ppm, quantization, resets_at, nominal_duration_nanos";

fn row_to_window(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredMeterWindow> {
    let scope_kind: String = row.get("scope_kind")?;
    let scoped_model: Option<String> = row.get("scoped_model")?;
    let scope = match (scope_kind.as_str(), scoped_model) {
        ("account_wide", None) => WindowScope::AccountWide,
        ("model_specific", Some(model)) => WindowScope::ModelSpecific(ModelId::new(model)),
        (kind, model) => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                3, // scope_kind, by position in SELECT_WINDOW_COLUMNS
                rusqlite::types::Type::Text,
                Box::new(crate::error::Error::Store(format!(
                    "inconsistent scope row in the database: kind {kind:?} with model {model:?}"
                ))),
            ));
        }
    };
    let quantization_string: String = row.get("quantization")?;
    let quantization = quantization_sql::from_sql(&quantization_string).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            7, // quantization, by position in SELECT_WINDOW_COLUMNS
            rusqlite::types::Type::Text,
            Box::new(e),
        )
    })?;
    Ok(StoredMeterWindow {
        row_id: WindowRowId::new(row.get("id")?),
        observation_id: ObservationRowId::new(row.get("observation_id")?),
        semantic_key: WindowSemanticKey::new(row.get::<_, String>("semantic_key")?),
        scope,
        quota_used:
            crate::domain::quota::QuotaUsed::new(
                crate::domain::quota::QuotaFractionPpm::new(
                    row.get::<_, i64>("quota_used_ppm")? as i32
                )
                .expect("the table CHECK keeps quota_used_ppm in range"),
            ),
        reported_resolution: ReportedResolution::new(
            crate::domain::quota::QuotaFractionPpm::new(
                row.get::<_, i64>("reported_resolution_ppm")? as i32,
            )
            .expect("the table CHECK keeps reported_resolution_ppm in range"),
        )
        .expect("the table CHECK keeps reported_resolution_ppm non-zero"),
        quantization,
        resets_at: UtcTimestamp::from_unix_nanos(row.get("resets_at")?),
        nominal_duration: NominalWindowDuration::from_nanos(
            row.get::<_, i64>("nominal_duration_nanos")? as u64,
        ),
    })
}

/// Reads every window row of one observation, in insertion order.
pub fn windows_by_observation(
    conn: &rusqlite::Connection,
    observation_id: ObservationRowId,
) -> Result<Vec<StoredMeterWindow>, Error> {
    let mut statement = conn
        .prepare(&format!(
            "SELECT {SELECT_WINDOW_COLUMNS} FROM meter_window WHERE observation_id = ?1 ORDER BY id"
        ))
        .map_err(|e| Error::Store(format!("cannot list meter windows: {e}")))?;
    let rows = statement
        .query_map([observation_id.value()], row_to_window)
        .map_err(|e| Error::Store(format!("cannot list meter windows: {e}")))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| Error::Store(format!("cannot read meter windows: {e}")))
}

/// The one current interpretation of an evidence row under a semantics
/// version. `None` means no observation has been recorded for that pair yet.
pub fn current_observation_id(
    conn: &rusqlite::Connection,
    evidence_id: EvidenceRowId,
    meter_semantics_id: &MeterSemanticsId,
) -> Result<Option<ObservationRowId>, Error> {
    conn.query_row(
        "SELECT current_observation_id FROM meter_observation_preference
         WHERE evidence_id = ?1 AND meter_semantics_id = ?2",
        params![evidence_id.value(), meter_semantics_id.as_str()],
        |row| row.get::<_, i64>(0).map(ObservationRowId::new),
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot read the observation preference: {e}")))
}

/// Switches the current interpretation of an evidence row under a semantics
/// version to `current_observation_id`. The only write this table's contract
/// permits: a general update or delete API deliberately does not exist.
pub fn switch_current_observation(
    conn: &rusqlite::Connection,
    evidence_id: EvidenceRowId,
    meter_semantics_id: &MeterSemanticsId,
    current_observation_id: ObservationRowId,
) -> Result<(), Error> {
    conn.execute(
        "UPDATE meter_observation_preference
         SET current_observation_id = ?3
         WHERE evidence_id = ?1 AND meter_semantics_id = ?2",
        params![
            evidence_id.value(),
            meter_semantics_id.as_str(),
            current_observation_id.value(),
        ],
    )
    .map_err(|e| Error::Store(format!("cannot switch the observation preference: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
    use crate::domain::time::{FakeClock, MonotonicDuration};
    use crate::store::account::observe_account;
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::store::meter_attempt::{DueReason, NewMeterAttempt, start_meter_attempt};
    use crate::store::migrate::run_migrations;
    use crate::store::migrations::registry;
    use crate::store::sample_run::{Trigger, start_sample_run};
    use crate::store::sampling_policy_snapshot::{
        ResolvedSamplingPolicy, SamplingPolicySnapshotId, resolve_policy_snapshot,
    };
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-meter-evidence-test-{}-{suffix}",
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

    const POLICY: ResolvedSamplingPolicy = ResolvedSamplingPolicy {
        ordinary_cadence: MonotonicDuration::from_millis(300_000),
        freshness_horizon: MonotonicDuration::from_millis(900_000),
        reset_edge_policy: String::new(),
        retry_backoff_policy: String::new(),
        command_budget: MonotonicDuration::from_millis(60_000),
        policy_algorithm_version: String::new(),
    };

    /// A connection migrated through the full registry, holding one account,
    /// one sample run, one policy snapshot and one started attempt the
    /// evidence rows can reference.
    fn fixture() -> (
        ScratchDir,
        rusqlite::Connection,
        crate::store::sample_run::SampleRunId,
        AccountId,
        SamplingPolicySnapshotId,
        MeterAttemptRowId,
    ) {
        let scratch = ScratchDir::new();
        let policy = PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1000),
        };
        let mut conn = open(
            &scratch.path().join("meter.db"),
            AccessMode::ReadWrite,
            &policy,
        )
        .expect("fixture connection must open");
        let clock_at = |nanos: i64| FakeClock::new(UtcTimestamp::from_unix_nanos(nanos));
        run_migrations(&mut conn, &registry(), None, &clock_at(9_000))
            .expect("fixture migrations must apply");
        let account = observe_account(
            &conn,
            "test-provider",
            "test-account",
            UtcTimestamp::from_unix_nanos(10_000),
        )
        .expect("fixture account must insert");
        let run = start_sample_run(
            &conn,
            Trigger::Manual,
            UtcTimestamp::from_unix_nanos(10_000),
            "test",
        )
        .expect("fixture sample run must insert");
        let snapshot = resolve_policy_snapshot(
            &conn,
            account,
            UtcTimestamp::from_unix_nanos(10_000),
            &POLICY,
        )
        .expect("fixture policy snapshot must insert");
        let attempt = start_meter_attempt(
            &conn,
            &NewMeterAttempt {
                run_id: run,
                account_id: account,
                provider: "test-provider".into(),
                request_started_at: UtcTimestamp::from_unix_nanos(20_000),
                credential_context_id: Some("ctx-1".into()),
                policy_snapshot_id: snapshot,
                due_at: UtcTimestamp::from_unix_nanos(19_000),
                due_reason: DueReason::OrdinaryCadence,
                due_basis: None,
                provider_contract_id: "endpoint-schema-v3".into(),
                meter_semantics_id: "account-5h-v2".into(),
            },
        )
        .expect("fixture attempt must insert");
        (scratch, conn, run, account, snapshot, attempt)
    }

    fn evidence(attempt: MeterAttemptRowId) -> NewMeterResponseEvidence {
        NewMeterResponseEvidence {
            attempt_id: attempt,
            response_classification: "200".into(),
            received_at: UtcTimestamp::from_unix_nanos(30_000),
            provider_observed_at_original: Some("2026-08-25T12:00:00Z".into()),
            evidence_capsule: r#"{"windows":[{"key":"5h","used":"41%"}]}"#.into(),
            capsule_schema_version: "capsule-v1".into(),
            sanitizer_version: "sanitizer-v1".into(),
            capture_truncated: false,
        }
    }

    fn observation(
        attempt: MeterAttemptRowId,
        evidence_id: EvidenceRowId,
        account: AccountId,
        semantics: &str,
        fingerprint: &str,
    ) -> NewMeterObservation {
        NewMeterObservation {
            attempt_id: attempt,
            evidence_id,
            account_id: account,
            provider: "test-provider".into(),
            provider_observed_at: Some(UtcTimestamp::from_unix_nanos(30_000)),
            received_at: UtcTimestamp::from_unix_nanos(31_000),
            measurement_basis: MeasurementBasis::ProviderObserved,
            observed_plan: Some("max".into()),
            observed_tier: Some("pro".into()),
            adapter_version: AdapterVersion::new("adapter-v1"),
            provider_contract_id: ProviderContractId::new("endpoint-schema-v3"),
            meter_semantics_id: MeterSemanticsId::new(semantics),
            normalized_fingerprint: fingerprint.into(),
        }
    }

    fn window(observation_id: ObservationRowId, used_ppm: i32) -> NewMeterWindow {
        NewMeterWindow {
            observation_id,
            semantic_key: WindowSemanticKey::new("5h"),
            scope: WindowScope::AccountWide,
            quota_used: QuotaUsed::new(QuotaFractionPpm::new(used_ppm).unwrap()),
            reported_resolution: ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap())
                .unwrap(),
            quantization: QuantizationSemantics::RoundedToNearest,
            resets_at: UtcTimestamp::from_unix_nanos(100_000),
            nominal_duration: NominalWindowDuration::from_nanos(3_600_000_000_000),
        }
    }

    /// The Done-when: v1 and v2 interpretations against the same evidence,
    /// the selector switched to v2, both interpretations unchanged, exactly
    /// one selector row.
    #[test]
    fn switching_the_preference_keeps_both_interpretations_immutable() {
        let (_scratch, conn, _run, account, _snapshot, attempt) = fixture();
        let evidence_id =
            insert_response_evidence(&conn, &evidence(attempt)).expect("the evidence must insert");
        let v1 = insert_observation(
            &conn,
            &observation(attempt, evidence_id, account, "semantics-v1", "fp-v1"),
        )
        .expect("the v1 interpretation must insert");
        let v2 = insert_observation(
            &conn,
            &observation(attempt, evidence_id, account, "semantics-v1", "fp-v2"),
        )
        .expect("the v2 interpretation must insert");

        // The first interpretation is current by construction.
        let semantics = MeterSemanticsId::new("semantics-v1");
        assert_eq!(
            current_observation_id(&conn, evidence_id, &semantics)
                .expect("the preference must read"),
            Some(v1)
        );

        switch_current_observation(&conn, evidence_id, &semantics, v2)
            .expect("switching the preference must succeed");

        // Both interpretations are unchanged and exactly one selector exists.
        let stored_v1 = observation_by_row_id(&conn, v1)
            .expect("v1 must read")
            .expect("v1 must exist");
        let stored_v2 = observation_by_row_id(&conn, v2)
            .expect("v2 must read")
            .expect("v2 must exist");
        assert_eq!(stored_v1.normalized_fingerprint, "fp-v1");
        assert_eq!(stored_v2.normalized_fingerprint, "fp-v2");
        assert_eq!(
            current_observation_id(&conn, evidence_id, &semantics)
                .expect("the preference must read"),
            Some(v2)
        );
        let selector_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM meter_observation_preference WHERE evidence_id = ?1",
                [evidence_id.value()],
                |row| row.get(0),
            )
            .expect("the selector count must read");
        assert_eq!(selector_count, 1);
    }

    /// The planted negative for the one-current-per-pair invariant: a second
    /// selector row for the same (evidence, semantics) pair is refused by the
    /// database's primary key, not by this module's own code.
    #[test]
    fn a_second_selector_for_one_evidence_and_semantics_pair_fails_at_the_database() {
        let (_scratch, conn, _run, account, _snapshot, attempt) = fixture();
        let evidence_id =
            insert_response_evidence(&conn, &evidence(attempt)).expect("the evidence must insert");
        let v1 = insert_observation(
            &conn,
            &observation(attempt, evidence_id, account, "semantics-v1", "fp-v1"),
        )
        .expect("the v1 interpretation must insert");
        let v2 = insert_observation(
            &conn,
            &observation(attempt, evidence_id, account, "semantics-v1", "fp-v2"),
        )
        .expect("the v2 interpretation must insert");

        let err = conn
            .execute(
                "INSERT INTO meter_observation_preference (evidence_id, meter_semantics_id, current_observation_id)
                 VALUES (?1, 'semantics-v1', ?2)",
                params![evidence_id.value(), v2.value()],
            )
            .expect_err("a second selector row must violate the primary key");
        assert!(
            err.to_string().contains("UNIQUE constraint failed"),
            "the refusal must name the uniqueness constraint: {err}"
        );
        let _ = v1;
    }

    /// The planted negatives for the quantity constraints: an out-of-range
    /// quota fraction and a negative duration are refused by the database.
    #[test]
    fn out_of_range_quota_and_negative_duration_are_refused_by_the_database() {
        let (_scratch, conn, _run, account, _snapshot, attempt) = fixture();
        let evidence_id =
            insert_response_evidence(&conn, &evidence(attempt)).expect("the evidence must insert");
        let observation_id = insert_observation(
            &conn,
            &observation(attempt, evidence_id, account, "semantics-v1", "fp-v1"),
        )
        .expect("the observation must insert");

        let out_of_range = conn
            .execute(
                "INSERT INTO meter_window (
                    observation_id, semantic_key, scope_kind, scoped_model, quota_used_ppm,
                    reported_resolution_ppm, quantization, resets_at, nominal_duration_nanos
                ) VALUES (?1, '5h', 'account_wide', NULL, 1500000, 10000, 'exact', 100000, 1000)",
                [observation_id.value()],
            )
            .expect_err("a quota fraction above one million must be refused");
        assert!(
            out_of_range.to_string().contains("CHECK"),
            "the refusal must come from the constraint: {out_of_range}"
        );

        let negative_duration = conn
            .execute(
                "INSERT INTO meter_window (
                    observation_id, semantic_key, scope_kind, scoped_model, quota_used_ppm,
                    reported_resolution_ppm, quantization, resets_at, nominal_duration_nanos
                ) VALUES (?1, '5h', 'account_wide', NULL, 410000, 10000, 'exact', 100000, -1)",
                [observation_id.value()],
            )
            .expect_err("a negative duration must be refused");
        assert!(
            negative_duration.to_string().contains("CHECK"),
            "the refusal must come from the constraint: {negative_duration}"
        );
    }

    /// Direct SQL `UPDATE` and `DELETE` against every irreplaceable table
    /// owned here fail through the database triggers.
    #[test]
    fn triggers_refuse_every_update_and_delete_on_the_irreplaceable_tables() {
        let (_scratch, conn, _run, account, _snapshot, attempt) = fixture();
        let evidence_id =
            insert_response_evidence(&conn, &evidence(attempt)).expect("the evidence must insert");
        let observation_id = insert_observation(
            &conn,
            &observation(attempt, evidence_id, account, "semantics-v1", "fp-v1"),
        )
        .expect("the observation must insert");
        let _window_id =
            insert_window(&conn, &window(observation_id, 410_000)).expect("the window must insert");

        for sql in [
            "UPDATE meter_response_evidence SET response_classification = 'rewritten' WHERE id = 1",
            "DELETE FROM meter_response_evidence WHERE id = 1",
            "UPDATE meter_observation SET provider = 'rewritten' WHERE id = 1",
            "DELETE FROM meter_observation WHERE id = 1",
            "UPDATE meter_window SET semantic_key = 'rewritten' WHERE id = 1",
            "DELETE FROM meter_window WHERE id = 1",
        ] {
            let err = conn
                .execute(sql, [])
                .err()
                .unwrap_or_else(|| panic!("direct statement must be refused: {sql}"));
            assert!(
                err.to_string().contains("irreplaceable evidence"),
                "the trigger must name the reason: {err}"
            );
        }
    }

    /// No effective-remaining value is stored anywhere in these tables: the
    /// column set of each table is inspected and none carries a remaining or
    /// effective column.
    #[test]
    fn no_effective_remaining_value_is_persisted_in_any_of_these_tables() {
        let (_scratch, conn, _run, _account, _snapshot, _attempt) = fixture();
        for table in [
            "meter_response_evidence",
            "meter_observation",
            "meter_window",
            "meter_observation_preference",
        ] {
            let mut statement = conn
                .prepare(&format!("PRAGMA table_info({table})"))
                .expect("the pragma must prepare");
            let columns = statement
                .query_map([], |row| row.get::<_, String>(1))
                .expect("the pragma must run")
                .collect::<Result<Vec<_>, _>>()
                .expect("the pragma must read");
            for column in &columns {
                let lower = column.to_lowercase();
                assert!(
                    !lower.contains("remaining") && !lower.contains("effective"),
                    "table {table} persists a derived value in column {column}"
                );
            }
        }
    }

    /// The exact-lexeme guarantee: a capsule whose quota lexemes carry
    /// exponent spelling, trailing fractional zeros and escaped characters
    /// round-trips byte-exact, its stored content hash matches a recomputed
    /// hash, and the normalized window values written from the same evidence
    /// remain equivalent.
    #[test]
    fn quota_lexemes_round_trip_byte_exact_with_a_matching_content_hash() {
        let (_scratch, conn, _run, account, _snapshot, attempt) = fixture();
        let capsule = r#"{"windows":[{"key":"5h","used":"4.1e1"},{"key":"7d","used":"41.00"},{"key":"model","used":"4\u002e1"}]}"#;
        let mut evidence = evidence(attempt);
        evidence.evidence_capsule = capsule.into();
        let evidence_id =
            insert_response_evidence(&conn, &evidence).expect("the evidence must insert");

        let stored = evidence_by_row_id(&conn, evidence_id)
            .expect("the evidence must read")
            .expect("the evidence must exist");
        assert_eq!(
            stored.evidence_capsule, capsule,
            "the capsule must round-trip byte-exact"
        );
        assert_eq!(
            stored.content_hash,
            content_hash_of(capsule),
            "the stored hash must match a recomputed hash over the capsule"
        );

        // The normalized values written from the same evidence are equivalent
        // to what the lexemes express: 41% under round-to-nearest.
        let observation_id = insert_observation(
            &conn,
            &observation(attempt, evidence_id, account, "semantics-v1", "fp-v1"),
        )
        .expect("the observation must insert");
        let window_id =
            insert_window(&conn, &window(observation_id, 410_000)).expect("the window must insert");
        let windows = windows_by_observation(&conn, observation_id).expect("the windows must read");
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].row_id, window_id);
        assert_eq!(windows[0].quota_used.as_ppm().get(), 410_000);
        assert_eq!(
            windows[0].quantization,
            QuantizationSemantics::RoundedToNearest
        );
    }
}
