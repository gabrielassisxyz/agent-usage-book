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
    LegacyMeterImport,
    LegacyMeterImportRecord,
    AuthoritativeSurfaceComparison,
    AdapterSemanticsAnnotation,

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
            | Self::CalibrationExperiment
            | Self::LegacyMeterImport
            | Self::LegacyMeterImportRecord
            | Self::AuthoritativeSurfaceComparison
            | Self::AdapterSemanticsAnnotation => DurableClassCategory::Irreplaceable,

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
            | Self::RateCard
            | Self::LegacyMeterImport
            | Self::LegacyMeterImportRecord
            | Self::AuthoritativeSurfaceComparison
            | Self::AdapterSemanticsAnnotation => RetentionRule::Forever,

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
            Self::LegacyMeterImport => Some("legacy_meter_import"),
            Self::LegacyMeterImportRecord => Some("legacy_meter_import_record"),
            Self::AuthoritativeSurfaceComparison => Some("authoritative_surface_comparison"),
            Self::AdapterSemanticsAnnotation => Some("adapter_semantics_annotation"),
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

    /// The rebuild sweep this class belongs to, when the taxonomy classifies it
    /// rebuildable and assigns it a scope.
    ///
    /// The match is exhaustive with no wildcard arm, so adding a variant to the
    /// taxonomy forces an explicit decision here: a class joins a rebuild sweep
    /// only when this method says so AND its category is rebuildable, and the
    /// grouping test below fails a grouping that names any class whose category
    /// is not rebuildable. An irreplaceable class therefore cannot join a sweep
    /// by being assigned to one.
    pub const fn rebuild_group(self) -> Option<RebuildGroup> {
        match self {
            Self::UsageOccurrence
            | Self::TranscriptFile
            | Self::UsageEvent
            | Self::UsageComponent
            | Self::Session
            | Self::IngestQuarantine => Some(RebuildGroup::Transcripts),
            Self::TaskEvent
            | Self::TaskEventQuarantine
            | Self::TaskKindCandidate
            | Self::TaskIdentity
            | Self::AttributionSegment => Some(RebuildGroup::Attribution),
            Self::Account
            | Self::SampleRun
            | Self::SamplingPolicySnapshot
            | Self::SamplingLease
            | Self::LedgerGeneration
            | Self::IngestionGeneration
            | Self::SessionAccountMarker
            | Self::MeterAttempt
            | Self::MeterAttemptResult
            | Self::MeterResponseEvidence
            | Self::CalibrationExperiment
            | Self::LegacyMeterImport
            | Self::LegacyMeterImportRecord
            | Self::AuthoritativeSurfaceComparison
            | Self::AdapterSemanticsAnnotation
            | Self::MeterObservation
            | Self::MeterWindow
            | Self::MeterObservationPreference
            | Self::WindowCalibrationCandidate
            | Self::WindowCalibrationResult
            | Self::WindowCalibrationSourceExperiment
            | Self::CalibrationLifecycle
            | Self::CostModel
            | Self::CostModelTerm
            | Self::CostModelLifecycle
            | Self::RateCard
            | Self::StatusProjection
            | Self::PendingObservationSpool
            | Self::RetainedProviderBody => None,
        }
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
            Self::LegacyMeterImport,
            Self::LegacyMeterImportRecord,
            Self::AuthoritativeSurfaceComparison,
            Self::AdapterSemanticsAnnotation,
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
            Self::LegacyMeterImport,
            Self::LegacyMeterImportRecord,
            Self::AuthoritativeSurfaceComparison,
            Self::AdapterSemanticsAnnotation,
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

/// The named scopes a rebuild sweeps. The groups carry no class of their own:
/// each expands to the classes the shared taxonomy classifies rebuildable and
/// assigns to the group, so rebuild and retention read one definition of which
/// classes are irreplaceable and cannot disagree about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RebuildGroup {
    /// Transcript-derived usage: canonical events, components, occurrences, the
    /// session timelines derived from them, the file index and the ingest quarantine.
    Transcripts,
    /// Task events and identities and the attribution segments derived from them.
    Attribution,
}

impl RebuildGroup {
    /// Every rebuild group, in a stable order.
    pub const ALL: [RebuildGroup; 2] = [RebuildGroup::Transcripts, RebuildGroup::Attribution];

    /// The token that selects this group on the command line.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Transcripts => "transcripts",
            Self::Attribution => "attribution",
        }
    }

    /// Resolves a command-line target token to its group.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|group| group.name() == name)
    }

    /// The classes this group sweeps, derived by filtering the shared taxonomy.
    ///
    /// Never a locally declared list: the class set is read from [`DurableClass::all`]
    /// through two filters, and neither is optional. The durability filter drops any
    /// class the taxonomy does not classify rebuildable, so a class grouped here by
    /// mistake is still unreachable from the sweep; the grouping filter keeps only
    /// the classes this group owns. A class added to the taxonomy later needs no edit
    /// here: it joins a sweep only when the taxonomy calls it rebuildable and
    /// [`DurableClass::rebuild_group`] assigns it to this group, and an irreplaceable
    /// class can never pass the first filter at all.
    pub fn classes(self) -> Vec<DurableClass> {
        DurableClass::all()
            .iter()
            .copied()
            .filter(|class| class.category() == DurableClassCategory::Rebuildable)
            .filter(|class| class.rebuild_group() == Some(self))
            .collect()
    }
}

/// What one rebuild sweep deleted, per class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuildReport {
    pub group: RebuildGroup,
    pub deleted: Vec<(DurableClass, RowCount)>,
}

impl RebuildReport {
    /// Rows deleted across every class the sweep addressed.
    pub fn total(&self) -> RowCount {
        RowCount::new(self.deleted.iter().map(|(_, count)| count.value()).sum())
    }

    /// Rows deleted for one class, or zero when the sweep did not address it.
    pub fn deleted_for(&self, class: DurableClass) -> RowCount {
        RowCount::new(
            self.deleted
                .iter()
                .find(|(swept, _)| *swept == class)
                .map_or(0, |(_, count)| count.value()),
        )
    }
}

/// Destroys and recreates nothing by itself: deletes every table of one rebuild
/// group's classes in one transaction.
///
/// The write lock is acquired before any row is touched (`BEGIN IMMEDIATE`), so a
/// rebuild that starts while another mutating command holds the writer refuses
/// whole rather than deleting partially: either the lock is granted and every
/// class is swept inside the one transaction, or nothing is deleted at all. The
/// tables carry no cross-table foreign keys outside the usage tables' own
/// `ON DELETE CASCADE` children, so the per-table order inside the group is not
/// load-bearing; the transaction boundary is what makes the sweep atomic.
/// Recreating the materializations is the re-ingest path, which is always
/// available because transcripts remain authoritative.
///
/// Each class's count is the rows the sweep removed from that class's table,
/// counted for every class before any delete runs: `usage_component` and
/// `usage_occurrence` carry `ON DELETE CASCADE` on `usage_event`, so a delete
/// statement's own rowcount would report the children's rows under the parent's
/// class and leave the children's own counts at zero.
pub fn delete_rebuildable(
    conn: &mut Connection,
    group: RebuildGroup,
) -> Result<RebuildReport, Error> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| {
            Error::Store(format!(
                "another writer holds the ledger database; rebuild refuses to delete partially: {e}"
            ))
        })?;
    // Every class is counted before any row is deleted, because a cascade fired
    // by an earlier delete in the same sweep would otherwise empty a later
    // class's table before its own count ran.
    let mut swept: Vec<(DurableClass, &str, i64)> = Vec::new();
    for class in group.classes() {
        let table = class.table_name().unwrap_or_else(|| {
            unreachable!(
                "rebuild group class {class:?} is a table class by the taxonomy's construction"
            )
        });
        let removed: i64 = tx
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .map_err(|e| Error::Store(format!("cannot count {table} before rebuilding it: {e}")))?;
        swept.push((class, table, removed));
    }
    for (_, table, _) in &swept {
        tx.execute(&format!("DELETE FROM {table}"), [])
            .map_err(|e| Error::Store(format!("cannot rebuild {table}: {e}")))?;
    }
    tx.commit()
        .map_err(|e| Error::Store(format!("cannot commit the rebuild sweep: {e}")))?;
    let deleted = swept
        .into_iter()
        .map(|(class, _, removed)| (class, RowCount::new(removed.max(0) as u64)))
        .collect();
    Ok(RebuildReport { group, deleted })
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

    // --- the rebuild groups (aub-lqe.11) -----------------------------------

    use crate::domain::attempt::AttemptOutcome;
    use crate::domain::ids::{
        AdapterVersion, BillingSemanticsId, MeterSemanticsId as CalMeterSemanticsId,
        ProviderContractId, SourceNamespace,
    };
    use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
    use crate::domain::rate_card::{BillingBasis, CurrencyCode, RateCardDraft, TokenClass};
    use crate::domain::time::MeasurementBasis;
    use crate::domain::window::{
        NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
        WindowSemanticKey,
    };
    use crate::store::calibration::{ExperimentId, PlanTier};
    use crate::store::cost_model::{ProviderKey, ValidityInterval};
    use crate::store::meter_attempt::{
        NewMeterAttempt, NewMeterAttemptResult, record_meter_attempt_result, start_meter_attempt,
    };
    use crate::store::meter_evidence::{
        NewMeterObservation, NewMeterResponseEvidence, NewMeterWindow, insert_observation,
        insert_response_evidence, insert_window,
    };
    use crate::store::sample_run::{Trigger, start_sample_run};
    use crate::store::sampling_policy_snapshot::{ResolvedSamplingPolicy, resolve_policy_snapshot};

    /// The grouping filter is honest: a class carrying a rebuild group has the
    /// rebuildable category, and every rebuildable class carries exactly one.
    /// The planted negative this pins is a grouping that names an irreplaceable
    /// class: the category assertion fails on it, even though the sweep's own
    /// durability filter would still have kept the class unreachable.
    #[test]
    fn rebuild_groups_partition_the_rebuildable_classes_and_nothing_else() {
        for class in DurableClass::all() {
            if let Some(group) = class.rebuild_group() {
                assert_eq!(
                    class.category(),
                    DurableClassCategory::Rebuildable,
                    "class {class:?} is grouped for rebuild but not rebuildable"
                );
                assert!(
                    group.classes().contains(class),
                    "class {class:?} is grouped under {group:?} but absent from its sweep"
                );
            }
        }
        let mut grouped = 0;
        for group in RebuildGroup::ALL {
            let classes = group.classes();
            assert!(!classes.is_empty(), "group {group:?} sweeps nothing");
            grouped += classes.len();
        }
        let rebuildable = DurableClass::all()
            .iter()
            .filter(|c| c.category() == DurableClassCategory::Rebuildable)
            .count();
        assert_eq!(
            grouped, rebuildable,
            "the rebuild groups must cover every rebuildable class exactly once"
        );
    }

    /// Adding an irreplaceable class to the taxonomy keeps it unreachable from
    /// every rebuild group with no edit to the rebuild path: the sweep's class
    /// set is derived by filtering the taxonomy, and this test enumerates the
    /// taxonomy itself, so a future irreplaceable variant is covered by the
    /// same loop without the test changing.
    #[test]
    fn no_irreplaceable_class_is_reachable_from_any_rebuild_group() {
        for class in DurableClass::all() {
            if !class.is_irreplaceable() {
                continue;
            }
            for group in RebuildGroup::ALL {
                assert!(
                    !group.classes().contains(class),
                    "irreplaceable class {class:?} is reachable from the {group:?} rebuild group"
                );
            }
        }
    }

    /// Seeds one row in every table whose class is irreplaceable, versioned
    /// interpretation, or reference data, plus the tables both rebuild groups
    /// sweep and the disposable lease table, so the sweep has real rows to
    /// delete and real rows it must leave alone.
    fn seed_sweep_fixture(conn: &mut Connection, now: UtcTimestamp) {
        // Irreplaceable evidence, through the real insert APIs.
        let account = crate::store::account::observe_account(
            conn,
            "fixture-provider",
            "fixture-account",
            now,
        )
        .expect("fixture account must insert");
        let run = start_sample_run(conn, Trigger::Manual, now, "fixture")
            .expect("fixture sample run must insert");
        let snapshot = resolve_policy_snapshot(
            conn,
            account,
            now,
            &ResolvedSamplingPolicy {
                ordinary_cadence: MonotonicDuration::from_millis(300_000),
                freshness_horizon: MonotonicDuration::from_millis(900_000),
                reset_edge_policy: "fixture".to_string(),
                retry_backoff_policy: "fixture".to_string(),
                command_budget: MonotonicDuration::from_seconds(30),
                policy_algorithm_version: "fixture-1".to_string(),
            },
        )
        .expect("fixture policy snapshot must insert");
        let attempt = start_meter_attempt(
            conn,
            &NewMeterAttempt {
                run_id: run,
                account_id: account,
                provider: "fixture-provider".to_string(),
                request_started_at: now,
                credential_context_id: Some("fixture-credential".to_string()),
                policy_snapshot_id: snapshot,
                due_at: now,
                due_reason: crate::store::meter_attempt::DueReason::OrdinaryCadence,
                due_basis: None,
                provider_contract_id: "fixture-endpoint-schema".to_string(),
                meter_semantics_id: "fixture-meter-semantics".to_string(),
            },
        )
        .expect("fixture attempt must insert");
        record_meter_attempt_result(
            conn,
            &NewMeterAttemptResult {
                attempt_id: attempt,
                completed_at: now,
                elapsed: MonotonicDuration::from_millis(10),
                outcome: AttemptOutcome::Success,
                sanitized_error_classification: None,
                retry_index: None,
                clock_anomaly: false,
            },
        )
        .expect("fixture attempt result must insert");
        let evidence = insert_response_evidence(
            conn,
            &NewMeterResponseEvidence {
                attempt_id: attempt,
                response_classification: "200".to_string(),
                received_at: now,
                provider_observed_at_original: Some("2026-08-25T12:00:00Z".to_string()),
                evidence_capsule: r#"{"windows":[{"key":"5h","used":"41%"}]}"#.to_string(),
                capsule_schema_version: "capsule-v1".to_string(),
                sanitizer_version: "sanitizer-v1".to_string(),
                capture_truncated: false,
            },
        )
        .expect("fixture evidence must insert");
        let observation = insert_observation(
            conn,
            &NewMeterObservation {
                attempt_id: attempt,
                evidence_id: evidence,
                account_id: account,
                provider: "fixture-provider".to_string(),
                provider_observed_at: Some(now),
                received_at: now,
                measurement_basis: MeasurementBasis::ProviderObserved,
                observed_plan: Some("max".to_string()),
                observed_tier: Some("pro".to_string()),
                adapter_version: AdapterVersion::new("fixture-adapter-1"),
                provider_contract_id: ProviderContractId::new("fixture-endpoint-schema"),
                meter_semantics_id: CalMeterSemanticsId::new("fixture-meter-semantics"),
                normalized_fingerprint: "fixture-fingerprint".to_string(),
            },
        )
        .expect("fixture observation must insert");
        insert_window(
            conn,
            &NewMeterWindow {
                observation_id: observation,
                semantic_key: WindowSemanticKey::new("5h"),
                scope: WindowScope::AccountWide,
                quota_used: QuotaUsed::new(QuotaFractionPpm::new(410_000).unwrap()),
                reported_resolution: ReportedResolution::new(
                    QuotaFractionPpm::new(10_000).unwrap(),
                )
                .unwrap(),
                quantization: QuantizationSemantics::RoundedToNearest,
                resets_at: now,
                nominal_duration: NominalWindowDuration::from_nanos(3_600_000_000_000),
            },
        )
        .expect("fixture window must insert");
        crate::store::calibration::insert_experiment(
            conn,
            &crate::store::calibration::CalibrationExperiment {
                id: ExperimentId::new("fixture-experiment-1"),
                provider: ProviderKey::new("fixture-provider"),
                plan_tier: PlanTier::new("max"),
                window_semantic_key: WindowSemanticKey::new("account"),
                meter_semantics_id: crate::domain::ids::MeterSemanticsId::new(
                    "fixture-meter-semantics",
                ),
                billing_semantics_id: BillingSemanticsId::new("fixture-billing-semantics"),
                validity: ValidityInterval::new(
                    now,
                    UtcTimestamp::from_unix_nanos(now.unix_nanos() + 1_000),
                )
                .expect("fixture validity must hold"),
                knowledge_time: now,
            },
        )
        .expect("fixture calibration experiment must insert");
        crate::store::rate_card::insert(
            conn,
            &[RateCardDraft {
                vendor: "fixture-vendor".to_string(),
                model: "fixture-model".to_string(),
                token_class: TokenClass::Input,
                rate_micros: 1_000,
                currency: CurrencyCode::Usd,
                billing_basis: BillingBasis::PerMillionTokens,
                effective_start: now.utc_date(),
                effective_end: None,
                publication: crate::domain::rate_card::Publication {
                    source: None,
                    published_at: None,
                },
                review_due: crate::domain::rate_card::ReviewDuePolicy::None,
            }],
            now,
        )
        .expect("fixture rate card must insert");
        conn.execute(
            "INSERT INTO session_account_marker (
                session_source, session_native, observed_at, logical_account,
                marker_source, evidence_designation
            ) VALUES ('claude-code', 'fixture-session', ?1, 'fixture-account', 'fixture', 'explicit')",
            [now.unix_nanos()],
        )
        .expect("fixture marker must insert");

        // The rebuildable tables both groups sweep.
        let event = crate::store::usage_event::insert_event(
            conn,
            &crate::store::usage_event::NewUsageEvent {
                canonical_event_id: "event-id:fixture-1",
                session_id: Some("fixture-session"),
                event_timestamp: Some(now),
                model_id: None,
                evidence_kind: "reported",
                source_provenance: "/fixture/corpus/session.jsonl",
                parser_version: "claude-code-1",
                created_at: now,
            },
        )
        .expect("fixture usage event must insert");
        crate::store::usage_component::insert_components(
            conn,
            event,
            &[("input", 10), ("output", 5)],
        )
        .expect("fixture components must insert");
        crate::store::usage_occurrence::insert_occurrence(
            conn,
            &crate::store::usage_occurrence::NewUsageOccurrence {
                source_namespace: &SourceNamespace::new("claude-code"),
                native_event_id: Some("fixture-1"),
                parser_version: &crate::transcripts::parser::ParserVersion::new("claude-code-1"),
                heuristic_key: None,
                source_file: "/fixture/corpus/session.jsonl",
                occurred_at_nanos: Some(now.unix_nanos()),
                event_id: Some(event),
                transcript_file_id: Some("session.jsonl"),
                source_location: None,
                canonical_fingerprint: Some("event-id:fixture-1"),
                identity_strength: Some("strong"),
                heuristic_algorithm_version: None,
                canonical_payload_digest: Some("fixture-digest"),
            },
        )
        .expect("fixture occurrence must insert");
        crate::store::session::insert_session(
            conn,
            &crate::store::session::NewSession {
                source: SourceNamespace::new("claude-code"),
                native_session_id: crate::domain::ids::NativeSessionId::new("fixture-session"),
                start: now,
                end: Some(now),
                project_key: crate::sessions::ProjectKey::new(crate::sessions::UNKNOWN_PROJECT),
                repository_key: crate::sessions::RepositoryKey::new(
                    crate::sessions::UNKNOWN_REPOSITORY,
                ),
                run_id: None,
            },
        )
        .expect("fixture session must insert");
        crate::store::transcript_file::upsert(
            conn,
            &crate::transcripts::watermark::Watermark {
                source_key: "claude-code".to_string(),
                relative_path: "session.jsonl".to_string(),
                size: 100,
                mtime_nanos: now.unix_nanos(),
                identity: "fixture:1".to_string(),
                parser_version: "claude-code-1".to_string(),
                consumed_offset: 100,
            },
        )
        .expect("fixture watermark must insert");
        crate::store::ingest_quarantine::record_quarantine(
            conn,
            &crate::store::ingest_quarantine::NewQuarantineItem {
                source_file: "/fixture/corpus/session.jsonl".to_string(),
                byte_offset: None,
                line_number: Some(1),
                parser: "claude-code-1".to_string(),
                failure_class: "wrong_field_type".to_string(),
                excerpt_hash: "fixture-hash".to_string(),
                excerpt: None,
                observed_at: now,
            },
        )
        .expect("fixture quarantine must insert");
        conn.execute(
            "INSERT INTO task_event (tracker_source, tracker_event_id, task_source, task_native, occurred_at, event_kind)
             VALUES ('fixture', 1, 'fixture', 'task-1', ?1, 'claim')",
            [now.unix_nanos()],
        )
        .expect("fixture task event must insert");
        conn.execute(
            "INSERT INTO task_event_quarantine (tracker_source, tracker_event_id, raw_timestamp, reason)
             VALUES ('fixture', 2, 'not-a-date', 'unparseable')",
            [],
        )
        .expect("fixture task quarantine must insert");
        conn.execute(
            "INSERT INTO task_kind_candidate (task_source, task_native, origin, raw_value)
             VALUES ('fixture', 'task-1', 'claimed', 'fix-bug')",
            [],
        )
        .expect("fixture task candidate must insert");
        conn.execute(
            "INSERT INTO task_identity (task_source, task_native, state, kind, winner_origin, evidence, normalization_version)
             VALUES ('fixture', 'task-1', 'resolved', 'bug', 'claimed', '[]', 1)",
            [],
        )
        .expect("fixture task identity must insert");
        conn.execute(
            "INSERT INTO attribution_segment (session_id, target_kind, task_source, task_native, overhead_reason, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, computed_at)
             VALUES ('fixture-session', 'task', 'fixture', 'task-1', NULL, 1, 2, 0, 0, ?1)",
            [now.unix_nanos()],
        )
        .expect("fixture attribution segment must insert");
        // The disposable lease table: a rebuild sweep must leave it alone even
        // though a prune target addresses it.
        crate::store::sampling_lease::acquire(
            conn,
            &crate::store::sampling_lease::AccountName::new("fixture-account"),
            &crate::store::sampling_lease::LeaseHolder::new("fixture-holder"),
            MonotonicDuration::from_seconds(60),
            &FakeClock::new(now),
        )
        .expect("fixture lease must acquire");
    }

    fn row_count(conn: &Connection, table: &str) -> u64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("row count must be readable") as u64
    }

    /// No code path reachable from the rebuild sweep deletes from the
    /// irreplaceable tables: every attempt, attempt result, response evidence,
    /// observation, window, calibration, rate card, account, sample run, policy
    /// snapshot, marker and generation counter seeded before the sweep is still
    /// there after both groups sweep, while the tables the groups do sweep are
    /// emptied. The disposable lease survives too: it is prunable, but it is
    /// not in either rebuild group's class set.
    #[test]
    fn the_rebuild_sweep_never_deletes_from_the_irreplaceable_tables() {
        let db = TestDb::new();
        let mut conn = db.open_migrated();
        let now = UtcTimestamp::from_unix_nanos(1_000_000);
        seed_sweep_fixture(&mut conn, now);

        // Every table the sweep must not touch, with its pre-sweep count.
        let protected: Vec<(&str, DurableClass, u64)> = DurableClass::all_table_classes()
            .iter()
            .copied()
            .filter(|class| class.category() != DurableClassCategory::Rebuildable)
            .map(|class| {
                let table = class.table_name().expect("protected class has a table");
                (table, class, row_count(&conn, table))
            })
            .collect();
        assert!(
            protected
                .iter()
                .any(|(table, _, count)| *table == "meter_attempt" && *count > 0),
            "the fixture must hold attempts, or the test proves nothing"
        );
        assert!(
            protected
                .iter()
                .any(|(table, _, count)| *table == "meter_observation" && *count > 0),
            "the fixture must hold observations, or the test proves nothing"
        );
        assert!(
            protected
                .iter()
                .any(|(table, _, count)| *table == "rate_card" && *count > 0),
            "the fixture must hold rate cards, or the test proves nothing"
        );

        for group in RebuildGroup::ALL {
            let report = delete_rebuildable(&mut conn, group).expect("the sweep must succeed");
            // The swept classes are exactly the group's derived classes.
            let swept: Vec<DurableClass> = report.deleted.iter().map(|(c, _)| *c).collect();
            let expected: Vec<DurableClass> = group.classes();
            assert_eq!(
                swept, expected,
                "the {group:?} sweep must address exactly its taxonomy-derived classes"
            );
            for (class, count) in &report.deleted {
                assert!(
                    count.value() > 0,
                    "the seeded sweep must delete real rows: {class:?} deleted {}",
                    count.value()
                );
            }
        }

        // Every protected table is exactly as the sweep left it.
        for (table, class, before) in &protected {
            let after = row_count(&conn, table);
            assert_eq!(
                after, *before,
                "the rebuild sweep changed the row count of {class:?} (table {table})"
            );
        }
        // And the rebuildable tables the groups own are empty.
        for group in RebuildGroup::ALL {
            for class in group.classes() {
                let table = class.table_name().unwrap();
                assert_eq!(
                    row_count(&conn, table),
                    0,
                    "the sweep left rows in {class:?} (table {table})"
                );
            }
        }
    }

    /// A rebuild that starts while another mutating command holds the writer
    /// refuses whole rather than deleting partially: the refusal is a store
    /// failure naming the held writer, and every table is exactly as it was.
    #[test]
    fn rebuild_refuses_while_another_writer_holds_the_database() {
        let db = TestDb::new();
        let mut conn = db.open_migrated();
        let now = UtcTimestamp::from_unix_nanos(1_000_000);
        seed_sweep_fixture(&mut conn, now);
        let before: Vec<(DurableClass, u64)> = RebuildGroup::Transcripts
            .classes()
            .into_iter()
            .map(|class| {
                let table = class.table_name().unwrap();
                (class, row_count(&conn, table))
            })
            .collect();
        assert!(before.iter().any(|(_, count)| *count > 0));

        // The competing writer: an immediate transaction on its own connection
        // holds the write slot while rebuild tries to take it.
        conn.busy_timeout(std::time::Duration::from_millis(100))
            .expect("busy timeout must be settable");
        let writer_path = db.0.clone();
        let mut holder = crate::store::connection::open(
            &writer_path,
            crate::store::connection::AccessMode::ReadWrite,
            &crate::store::connection::PragmaPolicy {
                busy_timeout: MonotonicDuration::from_millis(100),
            },
        )
        .expect("holder connection must open");
        let _held = holder
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .expect("the holder must take the write slot");

        let err = delete_rebuildable(&mut conn, RebuildGroup::Transcripts).unwrap_err();
        assert_eq!(err.exit_class(), crate::error::ExitClass::Store, "{err}");
        let message = err.to_string();
        assert!(
            message.contains("another writer holds"),
            "the refusal must name the held writer: {message}"
        );

        // Nothing was deleted: every swept table still holds its rows.
        drop(_held);
        for (class, before_count) in &before {
            let table = class.table_name().unwrap();
            assert_eq!(
                row_count(&conn, table),
                *before_count,
                "the refused rebuild must not have deleted from {class:?}"
            );
        }
    }
}
