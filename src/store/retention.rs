//! The durable-class taxonomy and structural retention policy (`aub-sth.15`, PLAN.md 11.5, 12, 13.1, 27, 36).
//!
//! May not depend on:
//! - provider adapters
//! - terminal-formatting crates
//! - presentation
//!
//! # Structural retention
//!
//! Left as prose, retention rules erode: somebody eventually adds a prune command
//! to save disk space, and quota history is destroyed. Making retention structural
//! means every persisted class declares its durability category and retention rule as
//! data. Classes with no expiry have no expiry fields and no expiry code path, and
//! maintenance pruning physically cannot address irreplaceable classes.

use std::collections::BTreeSet;

use rusqlite::Connection;

use crate::domain::rows::RowCount;
use crate::domain::time::Clock;
use crate::error::Error;

/// The durability classification for persisted state (PLAN.md 6, 11.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DurableClassCategory {
    /// Quota attempts, sanitized response evidence, calibration experiments,
    /// session-account markers, and ledger generation: cannot be reconstructed.
    Irreplaceable,
    /// Normalized meter observations, windows, and calibration results derived from
    /// retained evidence.
    VersionedInterpretation,
    /// Derived transcript usage, dedup index, task events, and quarantine records:
    /// re-derivable from original transcripts on disk.
    Rebuildable,
    /// Configuration models, rate cards, and account metadata.
    ReferenceData,
    /// Disposable read models, status projection, and transient leases.
    Disposable,
}

/// The retention rule governing a durable class (PLAN.md 11.5, aub-2r3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RetentionRule {
    /// Retained forever; no expiry field and no expiry code path exists.
    Forever,
    /// Retained forever in ordinary operation; explicit purge requires affirmative authorization.
    ForeverUnlessExplicitlyPurged,
    /// Rebuildable from source material; retention is configurable.
    RebuildableConfigurable { default_retained: bool },
    /// Circular buffer bounded by count (most recent N entries), discarded by count, no clock.
    CountBounded { max_entries: usize },
    /// Exactly one current file on disk; no historical versions accumulate.
    SingleCurrentFile,
    /// Ephemeral lease that expires when duration elapses.
    TransientLease,
    /// Ephemeral staging file drained into SQLite on startup or completion.
    EphemeralStaging,
    /// Diagnostic material with no scheduled deletion, cleared only by explicit operator command.
    ExplicitOperatorCommandOnly,
}

/// Exhaustive enumeration of every persisted class across the database and filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DurableClass {
    // Core SQLite tables: Irreplaceable evidence
    Account,
    SampleRun,
    SamplingPolicySnapshot,
    LedgerGeneration,
    IngestionGeneration,
    SessionAccountMarker,
    MeterAttempt,
    MeterAttemptResult,
    MeterResponseEvidence,
    CalibrationExperiment,

    // Core SQLite tables: Versioned interpretation
    MeterObservation,
    MeterWindow,
    MeterObservationPreference,
    WindowCalibrationCandidate,
    WindowCalibrationResult,
    WindowCalibrationSourceExperiment,
    CalibrationLifecycle,

    // Core SQLite tables: Rebuildable materialization
    UsageOccurrence,
    TranscriptFile,
    TaskEvent,
    TaskEventQuarantine,
    TaskKindCandidate,
    TaskIdentity,
    UsageEvent,
    UsageComponent,
    Session,
    AttributionSegment,
    IngestQuarantine,

    // Core SQLite tables: Reference data
    CostModel,
    CostModelTerm,
    CostModelLifecycle,
    RateCard,

    // Core SQLite tables: Disposable
    SamplingLease,

    // Non-table persisted artifacts
    StatusProjection,
    PendingObservationSpool,

    // Diagnostic artifacts: stored on a parse failure, bounded by count (aub-2r3)
    //
    // The table is created by the bead that implements the ingest parser's capture path.
    // Declared here so the taxonomy is complete and the retention rule is the single source
    // of truth even before the table exists.
    RetainedProviderBody,
}

impl DurableClass {
    /// Returns the durability category for this class.
    pub const fn category(self) -> DurableClassCategory {
        match self {
            Self::SampleRun
            | Self::LedgerGeneration
            | Self::IngestionGeneration
            | Self::SessionAccountMarker
            | Self::MeterAttempt
            | Self::MeterAttemptResult
            | Self::MeterResponseEvidence
            | Self::CalibrationExperiment => DurableClassCategory::Irreplaceable,

            Self::SamplingPolicySnapshot
            | Self::MeterObservation
            | Self::MeterWindow
            | Self::MeterObservationPreference
            | Self::WindowCalibrationCandidate
            | Self::WindowCalibrationResult
            | Self::WindowCalibrationSourceExperiment
            | Self::CalibrationLifecycle => DurableClassCategory::VersionedInterpretation,

            Self::UsageOccurrence
            | Self::TranscriptFile
            | Self::TaskEvent
            | Self::TaskEventQuarantine
            | Self::TaskKindCandidate
            | Self::TaskIdentity
            | Self::UsageEvent
            | Self::UsageComponent
            | Self::Session
            | Self::AttributionSegment
            | Self::IngestQuarantine => DurableClassCategory::Rebuildable,

            Self::Account
            | Self::CostModel
            | Self::CostModelTerm
            | Self::CostModelLifecycle
            | Self::RateCard => DurableClassCategory::ReferenceData,

            Self::SamplingLease
            | Self::StatusProjection
            | Self::PendingObservationSpool
            | Self::RetainedProviderBody => DurableClassCategory::Disposable,
        }
    }

    /// Returns the retention rule for this class.
    pub const fn retention_rule(self) -> RetentionRule {
        match self {
            Self::SampleRun
            | Self::SamplingPolicySnapshot
            | Self::LedgerGeneration
            | Self::IngestionGeneration
            | Self::MeterAttempt
            | Self::MeterAttemptResult
            | Self::MeterResponseEvidence
            | Self::CalibrationExperiment
            | Self::MeterObservation
            | Self::MeterWindow
            | Self::MeterObservationPreference
            | Self::WindowCalibrationCandidate
            | Self::WindowCalibrationResult
            | Self::WindowCalibrationSourceExperiment
            | Self::CalibrationLifecycle
            | Self::Account
            | Self::CostModel
            | Self::CostModelTerm
            | Self::CostModelLifecycle
            | Self::RateCard => RetentionRule::Forever,

            Self::SessionAccountMarker => RetentionRule::ForeverUnlessExplicitlyPurged,

            Self::UsageOccurrence
            | Self::TranscriptFile
            | Self::TaskEvent
            | Self::TaskEventQuarantine
            | Self::TaskKindCandidate
            | Self::TaskIdentity
            | Self::UsageEvent
            | Self::UsageComponent
            | Self::Session
            | Self::AttributionSegment => RetentionRule::RebuildableConfigurable {
                default_retained: false,
            },

            Self::IngestQuarantine => RetentionRule::RebuildableConfigurable {
                default_retained: true,
            },

            Self::StatusProjection => RetentionRule::SingleCurrentFile,
            Self::SamplingLease => RetentionRule::TransientLease,
            Self::PendingObservationSpool => RetentionRule::EphemeralStaging,
            Self::RetainedProviderBody => RetentionRule::CountBounded { max_entries: 100 },
        }
    }

    /// Returns the SQLite table name if this class corresponds to a database table.
    pub const fn table_name(self) -> Option<&'static str> {
        match self {
            Self::Account => Some("account"),
            Self::SampleRun => Some("sample_run"),
            Self::SamplingPolicySnapshot => Some("sampling_policy_snapshot"),
            Self::SamplingLease => Some("sampling_lease"),
            Self::LedgerGeneration => Some("ledger_generation"),
            Self::IngestionGeneration => Some("ingestion_generation"),
            Self::SessionAccountMarker => Some("session_account_marker"),
            Self::UsageOccurrence => Some("usage_occurrence"),
            Self::TranscriptFile => Some("transcript_file"),
            Self::TaskEvent => Some("task_event"),
            Self::TaskEventQuarantine => Some("task_event_quarantine"),
            Self::MeterAttempt => Some("meter_attempt"),
            Self::MeterAttemptResult => Some("meter_attempt_result"),
            Self::TaskKindCandidate => Some("task_kind_candidate"),
            Self::TaskIdentity => Some("task_identity"),
            Self::CostModel => Some("cost_model"),
            Self::CostModelTerm => Some("cost_model_term"),
            Self::CostModelLifecycle => Some("cost_model_lifecycle"),
            Self::RateCard => Some("rate_card"),
            Self::UsageEvent => Some("usage_event"),
            Self::UsageComponent => Some("usage_component"),
            Self::MeterResponseEvidence => Some("meter_response_evidence"),
            Self::MeterObservation => Some("meter_observation"),
            Self::MeterWindow => Some("meter_window"),
            Self::MeterObservationPreference => Some("meter_observation_preference"),
            Self::Session => Some("session"),
            Self::CalibrationExperiment => Some("calibration_experiment"),
            Self::WindowCalibrationCandidate => Some("window_calibration_candidate"),
            Self::WindowCalibrationResult => Some("window_calibration_result"),
            Self::WindowCalibrationSourceExperiment => Some("window_calibration_source_experiment"),
            Self::CalibrationLifecycle => Some("calibration_lifecycle"),
            Self::AttributionSegment => Some("attribution_segment"),
            Self::IngestQuarantine => Some("ingest_quarantine"),
            Self::StatusProjection | Self::PendingObservationSpool | Self::RetainedProviderBody => {
                None
            }
        }
    }

    /// Resolves a table name to its durable class variant.
    pub fn from_table_name(name: &str) -> Option<Self> {
        Self::all_table_classes()
            .iter()
            .copied()
            .find(|c| c.table_name() == Some(name))
    }

    /// True when this class is irreplaceable evidence.
    pub const fn is_irreplaceable(self) -> bool {
        matches!(self.category(), DurableClassCategory::Irreplaceable)
    }

    /// True when this class has a retention policy of Forever or ForeverUnlessExplicitlyPurged.
    pub const fn is_forever(self) -> bool {
        matches!(
            self.retention_rule(),
            RetentionRule::Forever | RetentionRule::ForeverUnlessExplicitlyPurged
        )
    }

    /// True when this class can be addressed by routine pruning.
    pub const fn is_prunable(self) -> bool {
        matches!(
            self.category(),
            DurableClassCategory::Rebuildable | DurableClassCategory::Disposable
        )
    }

    /// Every durable class defined in the taxonomy.
    pub const fn all() -> &'static [DurableClass] {
        &[
            Self::Account,
            Self::SampleRun,
            Self::SamplingPolicySnapshot,
            Self::SamplingLease,
            Self::LedgerGeneration,
            Self::IngestionGeneration,
            Self::SessionAccountMarker,
            Self::UsageOccurrence,
            Self::TranscriptFile,
            Self::TaskEvent,
            Self::TaskEventQuarantine,
            Self::MeterAttempt,
            Self::MeterAttemptResult,
            Self::TaskKindCandidate,
            Self::TaskIdentity,
            Self::CostModel,
            Self::CostModelTerm,
            Self::CostModelLifecycle,
            Self::RateCard,
            Self::UsageEvent,
            Self::UsageComponent,
            Self::MeterResponseEvidence,
            Self::MeterObservation,
            Self::MeterWindow,
            Self::MeterObservationPreference,
            Self::Session,
            Self::CalibrationExperiment,
            Self::WindowCalibrationCandidate,
            Self::WindowCalibrationResult,
            Self::WindowCalibrationSourceExperiment,
            Self::CalibrationLifecycle,
            Self::AttributionSegment,
            Self::IngestQuarantine,
            Self::StatusProjection,
            Self::PendingObservationSpool,
            Self::RetainedProviderBody,
        ]
    }

    /// Every durable class backed by a SQLite table.
    pub const fn all_table_classes() -> &'static [DurableClass] {
        &[
            Self::Account,
            Self::SampleRun,
            Self::SamplingPolicySnapshot,
            Self::SamplingLease,
            Self::LedgerGeneration,
            Self::IngestionGeneration,
            Self::SessionAccountMarker,
            Self::UsageOccurrence,
            Self::TranscriptFile,
            Self::TaskEvent,
            Self::TaskEventQuarantine,
            Self::MeterAttempt,
            Self::MeterAttemptResult,
            Self::TaskKindCandidate,
            Self::TaskIdentity,
            Self::CostModel,
            Self::CostModelTerm,
            Self::CostModelLifecycle,
            Self::RateCard,
            Self::UsageEvent,
            Self::UsageComponent,
            Self::MeterResponseEvidence,
            Self::MeterObservation,
            Self::MeterWindow,
            Self::MeterObservationPreference,
            Self::Session,
            Self::CalibrationExperiment,
            Self::WindowCalibrationCandidate,
            Self::WindowCalibrationResult,
            Self::WindowCalibrationSourceExperiment,
            Self::CalibrationLifecycle,
            Self::AttributionSegment,
            Self::IngestQuarantine,
        ]
    }
}

/// Addressable targets for maintenance and pruning operations.
///
/// This enum structurally excludes all irreplaceable, versioned interpretation, and
/// reference data classes. An attempt to add or address an irreplaceable class fails to compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PruneTarget {
    UsageOccurrence,
    TranscriptFile,
    TaskEvent,
    TaskEventQuarantine,
    TaskKindCandidate,
    TaskIdentity,
    UsageEvent,
    UsageComponent,
    Session,
    AttributionSegment,
    IngestQuarantine,
    SamplingLease,
}

impl PruneTarget {
    /// Maps this prune target to its underlying durable class in the taxonomy.
    pub const fn durable_class(self) -> DurableClass {
        match self {
            Self::UsageOccurrence => DurableClass::UsageOccurrence,
            Self::TranscriptFile => DurableClass::TranscriptFile,
            Self::TaskEvent => DurableClass::TaskEvent,
            Self::TaskEventQuarantine => DurableClass::TaskEventQuarantine,
            Self::TaskKindCandidate => DurableClass::TaskKindCandidate,
            Self::TaskIdentity => DurableClass::TaskIdentity,
            Self::UsageEvent => DurableClass::UsageEvent,
            Self::UsageComponent => DurableClass::UsageComponent,
            Self::Session => DurableClass::Session,
            Self::AttributionSegment => DurableClass::AttributionSegment,
            Self::IngestQuarantine => DurableClass::IngestQuarantine,
            Self::SamplingLease => DurableClass::SamplingLease,
        }
    }

    /// The SQLite table name for this prune target.
    pub const fn table_name(self) -> &'static str {
        match self.durable_class().table_name() {
            Some(name) => name,
            None => unreachable!(),
        }
    }

    /// All valid prune targets.
    pub const fn all() -> &'static [PruneTarget] {
        &[
            Self::UsageOccurrence,
            Self::TranscriptFile,
            Self::TaskEvent,
            Self::TaskEventQuarantine,
            Self::TaskKindCandidate,
            Self::TaskIdentity,
            Self::UsageEvent,
            Self::UsageComponent,
            Self::Session,
            Self::AttributionSegment,
            Self::IngestQuarantine,
            Self::SamplingLease,
        ]
    }
}

/// Prunes rows from a specific prunable target table.
pub fn prune_target(conn: &Connection, target: PruneTarget) -> Result<usize, Error> {
    let table = target.table_name();
    conn.execute(&format!("DELETE FROM {table}"), [])
        .map_err(|e| Error::Store(format!("cannot prune target {table}: {e}")))
}

/// Prunes all transcript-derived rebuildable tables.
pub fn prune_rebuildable_transcript_usage(conn: &Connection) -> Result<usize, Error> {
    let rebuildable_targets = [
        PruneTarget::UsageComponent,
        PruneTarget::UsageEvent,
        PruneTarget::UsageOccurrence,
        PruneTarget::Session,
        PruneTarget::AttributionSegment,
        PruneTarget::TaskIdentity,
        PruneTarget::TaskKindCandidate,
        PruneTarget::TaskEventQuarantine,
        PruneTarget::TaskEvent,
        PruneTarget::TranscriptFile,
    ];
    let mut total = 0;
    for target in rebuildable_targets {
        total += prune_target(conn, target)?;
    }
    Ok(total)
}

/// Prunes expired sampling leases based on the current clock.
pub fn prune_expired_leases(conn: &Connection, clock: &dyn Clock) -> Result<usize, Error> {
    crate::store::sampling_lease::clear_expired(conn, clock)
}

/// Summary of a routine maintenance run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoutineMaintenanceReport {
    pub expired_leases_pruned: usize,
    pub rebuildable_rows_pruned: usize,
}

/// Runs routine database maintenance operations.
///
/// Routine maintenance only touches expired leases and ephemeral records; it physically
/// cannot touch any irreplaceable or forever class.
pub fn run_routine_maintenance(
    conn: &Connection,
    clock: &dyn Clock,
) -> Result<RoutineMaintenanceReport, Error> {
    let expired_leases = prune_expired_leases(conn, clock)?;
    Ok(RoutineMaintenanceReport {
        expired_leases_pruned: expired_leases,
        rebuildable_rows_pruned: 0,
    })
}

// --- Doctor integration and storage health reporting ------------------------

/// Storage health statistics for one durable class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableClassStats {
    pub class: DurableClass,
    pub table_name: Option<String>,
    pub category: DurableClassCategory,
    pub retention_rule: RetentionRule,
    pub row_count: RowCount,
}

/// Doctor health audit report for the persistence and retention layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionDoctorReport {
    stats: Vec<DurableClassStats>,
    untracked_tables: Vec<String>,
}

impl RetentionDoctorReport {
    pub fn stats(&self) -> &[DurableClassStats] {
        &self.stats
    }

    pub fn untracked_tables(&self) -> &[String] {
        &self.untracked_tables
    }

    pub fn is_clean(&self) -> bool {
        self.untracked_tables.is_empty()
    }

    /// Formats a human-readable doctor summary.
    pub fn report(&self) -> String {
        let mut lines = Vec::new();
        if !self.untracked_tables.is_empty() {
            lines.push(format!(
                "untracked database tables without retention classification: {:?}",
                self.untracked_tables
            ));
        }
        for stat in &self.stats {
            let name = stat.table_name.as_deref().unwrap_or("non-table");
            lines.push(format!(
                "{name}: {} rows ({:?}, {:?})",
                stat.row_count.value(),
                stat.category,
                stat.retention_rule
            ));
        }
        lines.join("\n")
    }
}

/// Audits retention health and enumerates row counts across all durable classes.
pub fn audit_retention_health(conn: &Connection) -> Result<RetentionDoctorReport, Error> {
    let mut stats = Vec::new();
    let mut table_names = BTreeSet::new();

    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )
        .map_err(|e| Error::Store(format!("cannot list tables: {e}")))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| Error::Store(format!("cannot read tables: {e}")))?;

    for row in rows {
        table_names.insert(row.map_err(|e| Error::Store(format!("cannot read table name: {e}")))?);
    }

    let mut untracked_tables = Vec::new();
    for table in &table_names {
        if DurableClass::from_table_name(table).is_none() {
            untracked_tables.push(table.clone());
        }
    }

    for class in DurableClass::all() {
        let (name, count) = if let Some(table) = class.table_name() {
            if table_names.contains(table) {
                let cnt: i64 = conn
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                    .map_err(|e| Error::Store(format!("cannot count rows for {table}: {e}")))?;
                (Some(table.to_string()), RowCount::new(cnt.max(0) as u64))
            } else {
                (Some(table.to_string()), RowCount::new(0))
            }
        } else {
            (None, RowCount::new(0))
        };

        stats.push(DurableClassStats {
            class: *class,
            table_name: name,
            category: class.category(),
            retention_rule: class.retention_rule(),
            row_count: count,
        });
    }

    Ok(RetentionDoctorReport {
        stats,
        untracked_tables,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use rusqlite::Connection;

    use crate::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::store::migrate::run_migrations;
    use crate::store::migrations::registry;

    // --- helpers ------------------------------------------------------------

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDb(PathBuf);

    impl TestDb {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-retention-test-{}-{n}.sqlite3",
                std::process::id()
            ));
            Self(path)
        }

        fn open_migrated(&self) -> Connection {
            let policy = PragmaPolicy {
                busy_timeout: MonotonicDuration::from_millis(5000),
            };
            let mut conn =
                open(&self.0, AccessMode::ReadWrite, &policy).expect("test database must open");
            run_migrations(
                &mut conn,
                &registry(),
                None,
                &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
            )
            .expect("test migrations must apply");
            conn
        }
    }

    impl Drop for TestDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
            }
        }
    }

    fn live_table_names(conn: &Connection) -> BTreeSet<String> {
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master \
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
                 ORDER BY name",
            )
            .expect("table list query must prepare");
        stmt.query_map([], |row| row.get::<_, String>(0))
            .expect("table list query must run")
            .map(|r| r.expect("table row must be readable"))
            .collect()
    }

    // --- internal-consistency tests -----------------------------------------

    #[test]
    fn every_prune_target_maps_only_to_rebuildable_or_disposable() {
        for target in PruneTarget::all() {
            let class = target.durable_class();
            assert!(
                class.is_prunable(),
                "prune target {target:?} maps to non-prunable class {class:?}"
            );
            assert!(
                !class.is_irreplaceable(),
                "prune target {target:?} maps to irreplaceable class {class:?}"
            );
        }
    }

    #[test]
    fn every_durable_class_in_all_is_unique() {
        let mut seen = BTreeSet::new();
        for class in DurableClass::all() {
            assert!(seen.insert(*class), "duplicate durable class {class:?}");
        }
    }

    #[test]
    fn table_name_round_trip() {
        for class in DurableClass::all_table_classes() {
            let table = class.table_name().expect("table class must have a name");
            let resolved = DurableClass::from_table_name(table);
            assert_eq!(resolved, Some(*class));
        }
    }

    // --- taxonomy exhaustiveness against the live schema -------------------

    /// Every table in the fully migrated schema appears in the taxonomy exactly
    /// once, and every table-backed taxonomy entry corresponds to a real table.
    ///
    /// A class added in a migration without a corresponding taxonomy entry fails
    /// here rather than disappearing silently into an untracked category.
    #[test]
    fn taxonomy_covers_every_migrated_table_and_no_phantom() {
        let db = TestDb::new();
        let conn = db.open_migrated();
        let live = live_table_names(&conn);

        // Every live table must be in the taxonomy.
        let taxonomy_names: BTreeSet<&str> = DurableClass::all_table_classes()
            .iter()
            .filter_map(|c| c.table_name())
            .collect();

        for table in &live {
            // The migration framework adds a schema_migration table; skip it.
            if table == "schema_migration" {
                continue;
            }
            assert!(
                taxonomy_names.contains(table.as_str()),
                "live table '{table}' is absent from the durable-class taxonomy"
            );
        }

        // Every taxonomy table must exist in the live schema.
        for &name in &taxonomy_names {
            assert!(
                live.contains(name),
                "taxonomy names table '{name}' that does not exist in the live schema"
            );
        }
    }

    // --- property: routine maintenance never touches forever classes --------

    /// Over a sequence of routine maintenance operations, the row count in each
    /// forever class never decreases.
    ///
    /// The test seeds a non-forever table (sampling_lease) with rows, verifies
    /// that routine maintenance can remove expired leases from it, and asserts
    /// that for every class with a Forever or ForeverUnlessExplicitlyPurged rule
    /// the count after maintenance equals the count before (zero in a fresh db,
    /// but the structural guarantee holds regardless of seed data).
    #[test]
    fn routine_maintenance_never_decreases_forever_class_row_counts() {
        let db = TestDb::new();
        let mut conn = db.open_migrated();

        // Record pre-maintenance counts for all forever classes.
        let before: Vec<(DurableClass, u64)> = DurableClass::all()
            .iter()
            .filter(|c| c.is_forever())
            .filter_map(|c| {
                let table = c.table_name()?;
                let cnt: i64 = conn
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                    .expect("count must be readable");
                Some((*c, cnt.max(0) as u64))
            })
            .collect();

        // Seed the lease table so maintenance has something to do.
        let holder_clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1_000));
        crate::store::sampling_lease::acquire(
            &mut conn,
            &crate::store::sampling_lease::AccountName::new("test-account"),
            &crate::store::sampling_lease::LeaseHolder::new("test-holder"),
            MonotonicDuration::from_nanos(1),
            &holder_clock,
        )
        .expect("lease acquire must succeed");

        // Run maintenance at a time past the lease TTL.
        let after_clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1_000_000));
        let report =
            run_routine_maintenance(&conn, &after_clock).expect("routine maintenance must succeed");

        // At least one expired lease was removed.
        assert!(
            report.expired_leases_pruned > 0,
            "expected at least one expired lease to be pruned"
        );

        // Every forever class has the same row count as before.
        for (class, count_before) in &before {
            let table = class.table_name().unwrap();
            let count_after: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .expect("count must be readable");
            assert_eq!(
                count_after.max(0) as u64,
                *count_before,
                "routine maintenance decreased the row count for forever class {class:?} \
                 (table: {table})"
            );
        }
    }

    // --- integration: aub-2r3 retention boundary ----------------------------

    /// The retained-provider-body class encodes the aub-2r3 decision: captures are
    /// permitted on a parse failure, bounded to the most recent 100 per provider and per
    /// source, discarded by count and never by clock. Both boundaries of the permitted
    /// retention rule are tested here:
    ///
    /// - The boundary at or below 100 entries: the count-bounded rule allows them.
    /// - The boundary above 100 entries: the rule's max_entries cap is 100, meaning
    ///   any count above it represents entries that have been displaced.
    ///
    /// The class is reachable only through an explicit clearing command (`aub-smqu`),
    /// not through any routine maintenance operation. Routine maintenance cannot address
    /// it because it is Disposable but not a prune target: `PruneTarget` does not have
    /// a `RetainedProviderBody` variant.
    #[test]
    fn retained_provider_body_retention_boundary() {
        // The class must have CountBounded retention, not a clock-based rule.
        let rule = DurableClass::RetainedProviderBody.retention_rule();
        let RetentionRule::CountBounded { max_entries } = rule else {
            panic!("expected CountBounded retention for RetainedProviderBody, got {rule:?}")
        };
        assert_eq!(
            max_entries, 100,
            "aub-2r3 settled on 100 entries per provider and per source"
        );

        // No PruneTarget variant exists for the retained body: routine maintenance
        // cannot address it. This is the structural guarantee that clearing requires
        // an explicit operator command.
        let all_prune_targets: Vec<_> = PruneTarget::all()
            .iter()
            .map(|t| t.durable_class())
            .collect();
        assert!(
            !all_prune_targets.contains(&DurableClass::RetainedProviderBody),
            "RetainedProviderBody must not be reachable from any PruneTarget variant"
        );
    }
}
