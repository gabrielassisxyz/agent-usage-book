//! The `authoritative_surface_comparison` and `adapter_semantics_annotation`
//! tables: the recorded bookkeeping of comparing an adapter's reading against
//! the provider's own authoritative usage surface (`aub-eun.12`, PLAN.md
//! sections 34.8, 34.30, 45).
//!
//! This module records how existing interpretation compares against the
//! provider's authoritative surface. It adds no interpretation logic: the
//! adapter reading it stores is copied from a `meter_window` that was written
//! elsewhere, and the authoritative reading is a value a human read from a
//! human-facing page and passed in.
//!
//! Both tables are irreplaceable validation evidence. Their triggers reject
//! every `UPDATE` and `DELETE`; a wrong comparison is corrected by recording
//! another comparison and a linked correction annotation, never by rewriting a
//! row. An unresolved mismatch stays open until a correction annotation
//! references it.
//!
//! May not depend on:
//! - HTTP or provider semantics
//! - presentation

use rusqlite::{OptionalExtension, params};

use crate::domain::authoritative_comparison::{
    AuthoritativeComparisonVerdict, DocumentedGranularity,
};
use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
use crate::domain::rows::RowCount;
use crate::domain::time::UtcTimestamp;
use crate::domain::window::WindowSemanticKey;
use crate::error::Error;
use crate::store::meter_evidence::{ObservationRowId, WindowRowId};

/// An `authoritative_surface_comparison` row's identity: its SQLite rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ComparisonRowId(i64);

impl ComparisonRowId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

/// An `adapter_semantics_annotation` row's identity: its SQLite rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AnnotationRowId(i64);

impl AnnotationRowId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

/// The kind of one immutable annotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnotationKind {
    /// An unresolved mismatch: the comparison disagreed by more than the
    /// surface's documented granularity and no explanation exists yet.
    Mismatch,
    /// An explanation of, or a fix for, an earlier annotation. Links to the
    /// annotation it corrects and leaves it stored.
    Correction,
    /// A window or observation to hold out of calibration eligibility because
    /// of a known semantic discrepancy. Consumed by `aub-c0b.7`.
    Exclusion,
}

impl AnnotationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mismatch => "mismatch",
            Self::Correction => "correction",
            Self::Exclusion => "exclusion",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "mismatch" => Some(Self::Mismatch),
            "correction" => Some(Self::Correction),
            "exclusion" => Some(Self::Exclusion),
            _ => None,
        }
    }
}

/// One comparison to record: an adapter reading of one window, the value read
/// from the authoritative surface for the same window, the surface's
/// documented granularity, when the surface was read, and the verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAuthoritativeSurfaceComparison {
    pub observation_id: ObservationRowId,
    pub window_id: WindowRowId,
    pub semantic_key: WindowSemanticKey,
    pub authoritative_surface: String,
    pub documented_granularity: DocumentedGranularity,
    pub adapter_quota_used: QuotaUsed,
    pub authoritative_quota_used: QuotaUsed,
    pub read_at: UtcTimestamp,
    pub verdict: AuthoritativeComparisonVerdict,
}

/// One stored comparison row, read back exactly as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAuthoritativeSurfaceComparison {
    pub row_id: ComparisonRowId,
    pub observation_id: ObservationRowId,
    pub window_id: WindowRowId,
    pub semantic_key: WindowSemanticKey,
    pub authoritative_surface: String,
    pub documented_granularity: DocumentedGranularity,
    pub adapter_quota_used: QuotaUsed,
    pub authoritative_quota_used: QuotaUsed,
    pub read_at: UtcTimestamp,
    pub verdict: AuthoritativeComparisonVerdict,
}

/// One annotation to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAdapterSemanticsAnnotation {
    pub kind: AnnotationKind,
    pub comparison_id: ComparisonRowId,
    pub observation_id: ObservationRowId,
    pub semantic_key: WindowSemanticKey,
    pub adapter_quota_used: QuotaUsed,
    pub authoritative_quota_used: QuotaUsed,
    /// The annotation this one corrects. Required for [`AnnotationKind::Correction`]
    /// and rejected for every other kind.
    pub corrects: Option<AnnotationRowId>,
    pub detail: String,
    pub created_at: UtcTimestamp,
}

/// One stored annotation row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAdapterSemanticsAnnotation {
    pub row_id: AnnotationRowId,
    pub kind: AnnotationKind,
    pub comparison_id: ComparisonRowId,
    pub observation_id: ObservationRowId,
    pub semantic_key: WindowSemanticKey,
    pub adapter_quota_used: QuotaUsed,
    pub authoritative_quota_used: QuotaUsed,
    pub corrects: Option<AnnotationRowId>,
    pub detail: String,
    pub created_at: UtcTimestamp,
}

/// One open unresolved mismatch, projected for a health consumer. It names the
/// window, both values and the observation, and it is a typed record a caller
/// enumerates and counts rather than a free-text note. `aub-n27.7` owns
/// turning a non-empty list into a doctor finding, its age and its rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticMismatchFinding {
    pub annotation_id: AnnotationRowId,
    pub comparison_id: ComparisonRowId,
    pub observation_id: ObservationRowId,
    pub semantic_key: WindowSemanticKey,
    pub adapter_quota_used: QuotaUsed,
    pub authoritative_quota_used: QuotaUsed,
    pub detail: String,
}

fn quota_used_from_ppm(ppm: i64) -> Result<QuotaUsed, Error> {
    let value = i32::try_from(ppm)
        .ok()
        .and_then(QuotaFractionPpm::new)
        .ok_or_else(|| {
            Error::Store(format!("quota fraction {ppm} out of range in the database"))
        })?;
    Ok(QuotaUsed::new(value))
}

const INSERT_COMPARISON: &str = "
INSERT INTO authoritative_surface_comparison (
    observation_id, window_id, semantic_key, authoritative_surface,
    documented_granularity_ppm, adapter_quota_used_ppm, authoritative_quota_used_ppm,
    read_at, verdict
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) RETURNING id";

/// Records one comparison. The verdict is the caller's, computed by
/// [`crate::domain::authoritative_comparison::compare_against_authoritative_surface`]
/// from the same three inputs stored here, so a reader can recheck it.
pub fn insert_comparison(
    conn: &rusqlite::Connection,
    comparison: &NewAuthoritativeSurfaceComparison,
) -> Result<ComparisonRowId, Error> {
    conn.query_row(
        INSERT_COMPARISON,
        params![
            comparison.observation_id.value(),
            comparison.window_id.value(),
            comparison.semantic_key.as_str(),
            comparison.authoritative_surface,
            i64::from(comparison.documented_granularity.as_ppm().get()),
            i64::from(comparison.adapter_quota_used.as_ppm().get()),
            i64::from(comparison.authoritative_quota_used.as_ppm().get()),
            comparison.read_at.unix_nanos(),
            comparison.verdict.as_str(),
        ],
        |row| row.get(0),
    )
    .map(ComparisonRowId::new)
    .map_err(|e| {
        Error::Store(format!(
            "cannot record the authoritative surface comparison: {e}"
        ))
    })
}

const SELECT_COMPARISON_COLUMNS: &str = "
    id, observation_id, window_id, semantic_key, authoritative_surface,
    documented_granularity_ppm, adapter_quota_used_ppm, authoritative_quota_used_ppm,
    read_at, verdict";

fn row_to_comparison(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<StoredAuthoritativeSurfaceComparison> {
    let store_error = |e: Error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    };
    let granularity_ppm: i64 = row.get("documented_granularity_ppm")?;
    let granularity_value = i32::try_from(granularity_ppm)
        .ok()
        .and_then(QuotaFractionPpm::new)
        .ok_or_else(|| {
            store_error(Error::Store(format!(
                "documented granularity {granularity_ppm} out of range in the database"
            )))
        })?;
    let verdict_code: String = row.get("verdict")?;
    let verdict = AuthoritativeComparisonVerdict::from_code(&verdict_code).ok_or_else(|| {
        store_error(Error::Store(format!(
            "unknown comparison verdict stored in the database: {verdict_code:?}"
        )))
    })?;
    Ok(StoredAuthoritativeSurfaceComparison {
        row_id: ComparisonRowId::new(row.get("id")?),
        observation_id: ObservationRowId::new(row.get("observation_id")?),
        window_id: WindowRowId::new(row.get("window_id")?),
        semantic_key: WindowSemanticKey::new(row.get::<_, String>("semantic_key")?),
        authoritative_surface: row.get("authoritative_surface")?,
        documented_granularity: DocumentedGranularity::new(granularity_value),
        adapter_quota_used: quota_used_from_ppm(row.get("adapter_quota_used_ppm")?)
            .map_err(store_error)?,
        authoritative_quota_used: quota_used_from_ppm(row.get("authoritative_quota_used_ppm")?)
            .map_err(store_error)?,
        read_at: UtcTimestamp::from_unix_nanos(row.get("read_at")?),
        verdict,
    })
}

/// Reads one comparison row by its rowid, or `None` when there is no such row.
pub fn comparison_by_row_id(
    conn: &rusqlite::Connection,
    row_id: ComparisonRowId,
) -> Result<Option<StoredAuthoritativeSurfaceComparison>, Error> {
    conn.query_row(
        &format!(
            "SELECT {SELECT_COMPARISON_COLUMNS} FROM authoritative_surface_comparison WHERE id = ?1"
        ),
        params![row_id.value()],
        row_to_comparison,
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot read the comparison {row_id:?}: {e}")))
}

/// Every comparison recorded for one observation, in insertion order.
pub fn comparisons_for_observation(
    conn: &rusqlite::Connection,
    observation_id: ObservationRowId,
) -> Result<Vec<StoredAuthoritativeSurfaceComparison>, Error> {
    let mut statement = conn
        .prepare(&format!(
            "SELECT {SELECT_COMPARISON_COLUMNS} FROM authoritative_surface_comparison
             WHERE observation_id = ?1 ORDER BY id"
        ))
        .map_err(|e| Error::Store(format!("cannot list comparisons: {e}")))?;
    let rows = statement
        .query_map([observation_id.value()], row_to_comparison)
        .map_err(|e| Error::Store(format!("cannot list comparisons: {e}")))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| Error::Store(format!("cannot read comparisons: {e}")))
}

/// The window rows of one observation that carry no comparison yet. A
/// completed validation of an observation compares every semantic window the
/// adapter reported for it, not only the one a status line displays, so an
/// empty result is the completeness signal.
pub fn uncompared_window_ids(
    conn: &rusqlite::Connection,
    observation_id: ObservationRowId,
) -> Result<Vec<WindowRowId>, Error> {
    let mut statement = conn
        .prepare(
            "SELECT mw.id FROM meter_window mw
             WHERE mw.observation_id = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM authoritative_surface_comparison asc_row
                   WHERE asc_row.window_id = mw.id
               )
             ORDER BY mw.id",
        )
        .map_err(|e| Error::Store(format!("cannot list uncompared windows: {e}")))?;
    let rows = statement
        .query_map([observation_id.value()], |row| {
            row.get::<_, i64>(0).map(WindowRowId::new)
        })
        .map_err(|e| Error::Store(format!("cannot list uncompared windows: {e}")))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| Error::Store(format!("cannot read uncompared windows: {e}")))
}

const INSERT_ANNOTATION: &str = "
INSERT INTO adapter_semantics_annotation (
    kind, comparison_id, observation_id, semantic_key, adapter_quota_used_ppm,
    authoritative_quota_used_ppm, corrects_annotation_id, detail, created_at
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) RETURNING id";

/// Records one immutable annotation. A correction must name the annotation it
/// corrects, and every other kind must not: the same rule the table's CHECK
/// enforces, refused here first with a clearer message.
pub fn insert_annotation(
    conn: &rusqlite::Connection,
    annotation: &NewAdapterSemanticsAnnotation,
) -> Result<AnnotationRowId, Error> {
    let is_correction = annotation.kind == AnnotationKind::Correction;
    if is_correction && annotation.corrects.is_none() {
        return Err(Error::Store(
            "a correction annotation must name the annotation it corrects".to_string(),
        ));
    }
    if !is_correction && annotation.corrects.is_some() {
        return Err(Error::Store(format!(
            "a {} annotation cannot name an annotation to correct",
            annotation.kind.as_str()
        )));
    }
    conn.query_row(
        INSERT_ANNOTATION,
        params![
            annotation.kind.as_str(),
            annotation.comparison_id.value(),
            annotation.observation_id.value(),
            annotation.semantic_key.as_str(),
            i64::from(annotation.adapter_quota_used.as_ppm().get()),
            i64::from(annotation.authoritative_quota_used.as_ppm().get()),
            annotation.corrects.map(AnnotationRowId::value),
            annotation.detail,
            annotation.created_at.unix_nanos(),
        ],
        |row| row.get(0),
    )
    .map(AnnotationRowId::new)
    .map_err(|e| {
        Error::Store(format!(
            "cannot record the adapter semantics annotation: {e}"
        ))
    })
}

const SELECT_ANNOTATION_COLUMNS: &str = "
    id, kind, comparison_id, observation_id, semantic_key, adapter_quota_used_ppm,
    authoritative_quota_used_ppm, corrects_annotation_id, detail, created_at";

fn row_to_annotation(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<StoredAdapterSemanticsAnnotation> {
    let store_error = |e: Error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    };
    let kind_code: String = row.get("kind")?;
    let kind = AnnotationKind::from_code(&kind_code).ok_or_else(|| {
        store_error(Error::Store(format!(
            "unknown annotation kind stored in the database: {kind_code:?}"
        )))
    })?;
    Ok(StoredAdapterSemanticsAnnotation {
        row_id: AnnotationRowId::new(row.get("id")?),
        kind,
        comparison_id: ComparisonRowId::new(row.get("comparison_id")?),
        observation_id: ObservationRowId::new(row.get("observation_id")?),
        semantic_key: WindowSemanticKey::new(row.get::<_, String>("semantic_key")?),
        adapter_quota_used: quota_used_from_ppm(row.get("adapter_quota_used_ppm")?)
            .map_err(store_error)?,
        authoritative_quota_used: quota_used_from_ppm(row.get("authoritative_quota_used_ppm")?)
            .map_err(store_error)?,
        corrects: row
            .get::<_, Option<i64>>("corrects_annotation_id")?
            .map(AnnotationRowId::new),
        detail: row.get("detail")?,
        created_at: UtcTimestamp::from_unix_nanos(row.get("created_at")?),
    })
}

/// Reads one annotation row by its rowid, or `None` when there is no such row.
pub fn annotation_by_row_id(
    conn: &rusqlite::Connection,
    row_id: AnnotationRowId,
) -> Result<Option<StoredAdapterSemanticsAnnotation>, Error> {
    conn.query_row(
        &format!(
            "SELECT {SELECT_ANNOTATION_COLUMNS} FROM adapter_semantics_annotation WHERE id = ?1"
        ),
        params![row_id.value()],
        row_to_annotation,
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot read the annotation {row_id:?}: {e}")))
}

/// Every mismatch annotation that no correction annotation references yet: the
/// open findings. A mismatch leaves this set only when a correction that names
/// it is recorded, and the mismatch row itself stays stored either way.
pub fn open_semantic_mismatch_findings(
    conn: &rusqlite::Connection,
) -> Result<Vec<SemanticMismatchFinding>, Error> {
    let mut statement = conn
        .prepare(
            "SELECT a.id, a.comparison_id, a.observation_id, a.semantic_key,
                    a.adapter_quota_used_ppm, a.authoritative_quota_used_ppm, a.detail
             FROM adapter_semantics_annotation a
             WHERE a.kind = 'mismatch'
               AND NOT EXISTS (
                   SELECT 1 FROM adapter_semantics_annotation c
                   WHERE c.kind = 'correction' AND c.corrects_annotation_id = a.id
               )
             ORDER BY a.id",
        )
        .map_err(|e| Error::Store(format!("cannot list open mismatch findings: {e}")))?;
    let rows = statement
        .query_map([], |row| {
            let store_error = |e: Error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Integer,
                    Box::new(e),
                )
            };
            Ok(SemanticMismatchFinding {
                annotation_id: AnnotationRowId::new(row.get("id")?),
                comparison_id: ComparisonRowId::new(row.get("comparison_id")?),
                observation_id: ObservationRowId::new(row.get("observation_id")?),
                semantic_key: WindowSemanticKey::new(row.get::<_, String>("semantic_key")?),
                adapter_quota_used: quota_used_from_ppm(row.get("adapter_quota_used_ppm")?)
                    .map_err(store_error)?,
                authoritative_quota_used: quota_used_from_ppm(
                    row.get("authoritative_quota_used_ppm")?,
                )
                .map_err(store_error)?,
                detail: row.get("detail")?,
            })
        })
        .map_err(|e| Error::Store(format!("cannot list open mismatch findings: {e}")))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| Error::Store(format!("cannot read open mismatch findings: {e}")))
}

/// The local timestamp of the most recently read authoritative surface, or
/// `None` when no comparison has been recorded. Doctor turns the gap between
/// this and now into the age of the last comparison (`aub-n27.7`).
pub fn latest_comparison_read_at(
    conn: &rusqlite::Connection,
) -> Result<Option<UtcTimestamp>, Error> {
    conn.query_row(
        "SELECT MAX(read_at) FROM authoritative_surface_comparison",
        [],
        |row| row.get::<_, Option<i64>>(0),
    )
    .map(|opt| opt.map(UtcTimestamp::from_unix_nanos))
    .map_err(|e| Error::Store(format!("cannot read the latest comparison time: {e}")))
}

/// How many comparison rows the ledger holds: a backup-restore round trip
/// asserts this survives exactly.
pub fn comparison_row_count(conn: &rusqlite::Connection) -> Result<RowCount, Error> {
    count_rows(conn, "authoritative_surface_comparison")
}

/// How many annotation rows the ledger holds.
pub fn annotation_row_count(conn: &rusqlite::Connection) -> Result<RowCount, Error> {
    count_rows(conn, "adapter_semantics_annotation")
}

fn count_rows(conn: &rusqlite::Connection, table: &str) -> Result<RowCount, Error> {
    let count: i64 = conn
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .map_err(|e| Error::Store(format!("cannot count {table} rows: {e}")))?;
    Ok(RowCount::new(u64::try_from(count).map_err(|_| {
        Error::Internal(format!("{table} count {count} is negative"))
    })?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::authoritative_comparison::compare_against_authoritative_surface;
    use crate::domain::ids::{AdapterVersion, MeterSemanticsId, ProviderContractId};
    use crate::domain::quota::QuotaFractionPpm;
    use crate::domain::time::MeasurementBasis;
    use crate::domain::time::{FakeClock, MonotonicDuration};
    use crate::domain::window::{
        NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
    };
    use crate::store::account::observe_account;
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::store::meter_attempt::{DueReason, NewMeterAttempt, start_meter_attempt};
    use crate::store::meter_evidence::{
        NewMeterObservation, NewMeterResponseEvidence, NewMeterWindow, insert_observation,
        insert_response_evidence, insert_window,
    };
    use crate::store::migrate::run_migrations;
    use crate::store::migrations::registry;
    use crate::store::sample_run::{Trigger, start_sample_run};
    use crate::store::sampling_policy_snapshot::{ResolvedSamplingPolicy, resolve_policy_snapshot};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-adapter-semantics-test-{}-{suffix}",
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

    /// A migrated connection holding one observation with two account-wide
    /// windows the comparisons can reference.
    fn fixture() -> (
        ScratchDir,
        rusqlite::Connection,
        ObservationRowId,
        Vec<WindowRowId>,
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
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(9_000));
        run_migrations(&mut conn, &registry(), None, &clock)
            .expect("fixture migrations must apply");
        let account = observe_account(
            &conn,
            "anthropic",
            "primary",
            UtcTimestamp::from_unix_nanos(10_000),
        )
        .expect("fixture account must insert");
        let run = start_sample_run(
            &conn,
            Trigger::Manual,
            UtcTimestamp::from_unix_nanos(10_000),
            "t",
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
                provider: "anthropic".into(),
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
        let evidence_id = insert_response_evidence(
            &conn,
            &NewMeterResponseEvidence {
                attempt_id: attempt,
                response_classification: "200".into(),
                received_at: UtcTimestamp::from_unix_nanos(30_000),
                provider_observed_at_original: Some("2026-08-25T12:00:00Z".into()),
                evidence_capsule: r#"{"windows":[]}"#.into(),
                capsule_schema_version: "capsule-v1".into(),
                sanitizer_version: "sanitizer-v1".into(),
                capture_truncated: false,
            },
        )
        .expect("fixture evidence must insert");
        let observation_id = insert_observation(
            &conn,
            &NewMeterObservation {
                attempt_id: attempt,
                evidence_id,
                account_id: account,
                provider: "anthropic".into(),
                provider_observed_at: Some(UtcTimestamp::from_unix_nanos(30_000)),
                received_at: UtcTimestamp::from_unix_nanos(31_000),
                measurement_basis: MeasurementBasis::ProviderObserved,
                observed_plan: Some("max".into()),
                observed_tier: Some("pro".into()),
                adapter_version: AdapterVersion::new("adapter-v1"),
                provider_contract_id: ProviderContractId::new("endpoint-schema-v3"),
                meter_semantics_id: MeterSemanticsId::new("account-5h-v2"),
                normalized_fingerprint: "fp-1".into(),
            },
        )
        .expect("fixture observation must insert");
        let mut windows = Vec::new();
        for (key, used_ppm) in [("five_hour", 410_000), ("seven_day", 910_000)] {
            let window_id = insert_window(
                &conn,
                &NewMeterWindow {
                    observation_id,
                    semantic_key: WindowSemanticKey::new(key),
                    scope: WindowScope::AccountWide,
                    quota_used: QuotaUsed::new(QuotaFractionPpm::new(used_ppm).unwrap()),
                    reported_resolution: ReportedResolution::new(
                        QuotaFractionPpm::new(10_000).unwrap(),
                    )
                    .unwrap(),
                    quantization: QuantizationSemantics::RoundedToNearest,
                    resets_at: UtcTimestamp::from_unix_nanos(100_000).into(),
                    nominal_duration: NominalWindowDuration::from_nanos(18_000_000_000_000),
                },
            )
            .expect("fixture window must insert");
            windows.push(window_id);
        }
        (scratch, conn, observation_id, windows)
    }

    fn used(ppm: i32) -> QuotaUsed {
        QuotaUsed::new(QuotaFractionPpm::new(ppm).unwrap())
    }

    /// The validation procedure states, for the one adapter this release ships,
    /// which authoritative surface is the comparison target, the granularity
    /// that surface documents, the two verdict outcomes, and every semantic
    /// window the adapter reports.
    #[test]
    fn the_documented_procedure_names_the_surface_the_granularity_and_the_windows() {
        let procedure = include_str!("../../docs/adapter-semantics-validation.md");
        for required in [
            "Anthropic",
            "Anthropic Console",
            "whole percentage points",
            "10000 parts per million",
            "agrees within granularity",
            "unresolved mismatch",
            "no configurable tolerance",
            "`five_hour`",
            "`seven_day`",
            "`seven_day_<model>`",
            "Anthropic idle / not-started",
        ] {
            assert!(
                procedure.contains(required),
                "the validation procedure must state {required:?}"
            );
        }
    }

    fn granularity(ppm: i32) -> DocumentedGranularity {
        DocumentedGranularity::new(QuotaFractionPpm::new(ppm).unwrap())
    }

    fn comparison(
        observation_id: ObservationRowId,
        window_id: WindowRowId,
        key: &str,
        adapter_ppm: i32,
        surface_ppm: i32,
    ) -> NewAuthoritativeSurfaceComparison {
        let g = granularity(10_000);
        NewAuthoritativeSurfaceComparison {
            observation_id,
            window_id,
            semantic_key: WindowSemanticKey::new(key),
            authoritative_surface: "console usage page".into(),
            documented_granularity: g,
            adapter_quota_used: used(adapter_ppm),
            authoritative_quota_used: used(surface_ppm),
            read_at: UtcTimestamp::from_unix_nanos(200_000),
            verdict: compare_against_authoritative_surface(used(adapter_ppm), used(surface_ppm), g),
        }
    }

    /// A comparison round trips exactly, including its verdict and both
    /// quantities. The planted negative: the read path resolves the verdict
    /// column, so a row written with the mismatch verdict never reads back as
    /// agreement.
    #[test]
    fn a_comparison_round_trips_with_its_verdict() {
        let (_scratch, conn, observation_id, windows) = fixture();
        let id = insert_comparison(
            &conn,
            &comparison(observation_id, windows[0], "five_hour", 700_000, 410_000),
        )
        .expect("the comparison must insert");
        let stored = comparison_by_row_id(&conn, id)
            .expect("the comparison must read")
            .expect("the comparison must exist");
        assert_eq!(
            stored.verdict,
            AuthoritativeComparisonVerdict::UnresolvedMismatch
        );
        assert_eq!(stored.adapter_quota_used, used(700_000));
        assert_eq!(stored.authoritative_quota_used, used(410_000));
        assert_eq!(stored.semantic_key.as_str(), "five_hour");
    }

    /// The verdict has exactly two spellings the column accepts: a third is
    /// refused by the CHECK, not by this module.
    #[test]
    fn the_verdict_column_admits_no_third_outcome() {
        let (_scratch, conn, observation_id, windows) = fixture();
        let err = conn
            .execute(
                "INSERT INTO authoritative_surface_comparison (
                    observation_id, window_id, semantic_key, authoritative_surface,
                    documented_granularity_ppm, adapter_quota_used_ppm,
                    authoritative_quota_used_ppm, read_at, verdict
                ) VALUES (?1, ?2, 'five_hour', 'surface', 10000, 410000, 410000, 1, 'within_tolerance')",
                params![observation_id.value(), windows[0].value()],
            )
            .expect_err("a third verdict spelling must be refused");
        assert!(
            err.to_string().contains("CHECK"),
            "the refusal must come from the constraint: {err}"
        );
    }

    /// The immutability triggers reject every direct UPDATE and DELETE on both
    /// tables.
    #[test]
    fn the_triggers_refuse_update_and_delete() {
        let (_scratch, conn, observation_id, windows) = fixture();
        let comparison_id = insert_comparison(
            &conn,
            &comparison(observation_id, windows[0], "five_hour", 700_000, 410_000),
        )
        .expect("the comparison must insert");
        insert_annotation(
            &conn,
            &NewAdapterSemanticsAnnotation {
                kind: AnnotationKind::Mismatch,
                comparison_id,
                observation_id,
                semantic_key: WindowSemanticKey::new("five_hour"),
                adapter_quota_used: used(700_000),
                authoritative_quota_used: used(410_000),
                corrects: None,
                detail: "seeded".into(),
                created_at: UtcTimestamp::from_unix_nanos(210_000),
            },
        )
        .expect("the annotation must insert");
        for sql in [
            "UPDATE authoritative_surface_comparison SET verdict = 'agrees_within_granularity' WHERE id = 1",
            "DELETE FROM authoritative_surface_comparison WHERE id = 1",
            "UPDATE adapter_semantics_annotation SET detail = 'rewritten' WHERE id = 1",
            "DELETE FROM adapter_semantics_annotation WHERE id = 1",
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

    /// A seeded mismatch produces one open finding that names the window, both
    /// values and the observation. A correction then appends a linked record
    /// and the finding closes, while the mismatch annotation stays stored.
    #[test]
    fn a_seeded_mismatch_is_a_finding_and_a_correction_appends_a_linked_record() {
        let (_scratch, conn, observation_id, windows) = fixture();
        let comparison_id = insert_comparison(
            &conn,
            &comparison(observation_id, windows[0], "five_hour", 700_000, 410_000),
        )
        .expect("the comparison must insert");
        let mismatch = insert_annotation(
            &conn,
            &NewAdapterSemanticsAnnotation {
                kind: AnnotationKind::Mismatch,
                comparison_id,
                observation_id,
                semantic_key: WindowSemanticKey::new("five_hour"),
                adapter_quota_used: used(700_000),
                authoritative_quota_used: used(410_000),
                corrects: None,
                detail: "five_hour read 70 percent, surface showed 41 percent".into(),
                created_at: UtcTimestamp::from_unix_nanos(210_000),
            },
        )
        .expect("the mismatch must insert");

        let findings = open_semantic_mismatch_findings(&conn).expect("the findings must read");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].annotation_id, mismatch);
        assert_eq!(findings[0].observation_id, observation_id);
        assert_eq!(findings[0].semantic_key.as_str(), "five_hour");
        assert_eq!(findings[0].adapter_quota_used, used(700_000));
        assert_eq!(findings[0].authoritative_quota_used, used(410_000));

        let correction = insert_annotation(
            &conn,
            &NewAdapterSemanticsAnnotation {
                kind: AnnotationKind::Correction,
                comparison_id,
                observation_id,
                semantic_key: WindowSemanticKey::new("five_hour"),
                adapter_quota_used: used(700_000),
                authoritative_quota_used: used(410_000),
                corrects: Some(mismatch),
                detail: "adapter was reading the wrong field; fixed in adapter-v2".into(),
                created_at: UtcTimestamp::from_unix_nanos(220_000),
            },
        )
        .expect("the correction must insert");

        assert!(
            open_semantic_mismatch_findings(&conn)
                .expect("the findings must read")
                .is_empty(),
            "an explained mismatch is no longer an open finding"
        );
        // Both annotations are still stored.
        assert!(annotation_by_row_id(&conn, mismatch).unwrap().is_some());
        let stored_correction = annotation_by_row_id(&conn, correction)
            .unwrap()
            .expect("the correction must exist");
        assert_eq!(stored_correction.corrects, Some(mismatch));
    }

    /// A correction must name the annotation it corrects, and a non-correction
    /// must not.
    #[test]
    fn the_correction_link_rule_is_enforced() {
        let (_scratch, conn, observation_id, windows) = fixture();
        let comparison_id = insert_comparison(
            &conn,
            &comparison(observation_id, windows[0], "five_hour", 410_000, 410_000),
        )
        .expect("the comparison must insert");
        let base = NewAdapterSemanticsAnnotation {
            kind: AnnotationKind::Correction,
            comparison_id,
            observation_id,
            semantic_key: WindowSemanticKey::new("five_hour"),
            adapter_quota_used: used(410_000),
            authoritative_quota_used: used(410_000),
            corrects: None,
            detail: "d".into(),
            created_at: UtcTimestamp::from_unix_nanos(210_000),
        };
        let err =
            insert_annotation(&conn, &base).expect_err("a correction with no link is refused");
        assert!(
            err.to_string()
                .contains("must name the annotation it corrects")
        );

        let err = insert_annotation(
            &conn,
            &NewAdapterSemanticsAnnotation {
                kind: AnnotationKind::Exclusion,
                corrects: Some(AnnotationRowId::new(1)),
                ..base
            },
        )
        .expect_err("an exclusion with a link is refused");
        assert!(
            err.to_string()
                .contains("cannot name an annotation to correct")
        );
    }

    /// The validation of an observation covers every window it reported, not
    /// only one: `uncompared_window_ids` names the windows still missing a
    /// comparison and empties only when all are compared.
    #[test]
    fn uncompared_windows_track_full_coverage() {
        let (_scratch, conn, observation_id, windows) = fixture();
        assert_eq!(
            uncompared_window_ids(&conn, observation_id).expect("the read must succeed"),
            windows,
            "before any comparison every window is uncompared"
        );

        insert_comparison(
            &conn,
            &comparison(observation_id, windows[0], "five_hour", 410_000, 410_000),
        )
        .expect("the first comparison must insert");
        assert_eq!(
            uncompared_window_ids(&conn, observation_id).expect("the read must succeed"),
            vec![windows[1]],
            "the status-line window is compared; the other is not"
        );

        insert_comparison(
            &conn,
            &comparison(observation_id, windows[1], "seven_day", 910_000, 910_000),
        )
        .expect("the second comparison must insert");
        assert!(
            uncompared_window_ids(&conn, observation_id)
                .expect("the read must succeed")
                .is_empty(),
            "every window the adapter reported is now compared"
        );
    }

    /// The latest comparison time is the newest `read_at`, and `None` before
    /// any comparison.
    #[test]
    fn latest_comparison_time_tracks_the_newest_reading() {
        let (_scratch, conn, observation_id, windows) = fixture();
        assert_eq!(
            latest_comparison_read_at(&conn).expect("the read must succeed"),
            None
        );
        let mut first = comparison(observation_id, windows[0], "five_hour", 410_000, 410_000);
        first.read_at = UtcTimestamp::from_unix_nanos(500_000);
        insert_comparison(&conn, &first).expect("the first comparison must insert");
        let mut second = comparison(observation_id, windows[1], "seven_day", 910_000, 910_000);
        second.read_at = UtcTimestamp::from_unix_nanos(900_000);
        insert_comparison(&conn, &second).expect("the second comparison must insert");
        assert_eq!(
            latest_comparison_read_at(&conn).expect("the read must succeed"),
            Some(UtcTimestamp::from_unix_nanos(900_000))
        );
    }
}
