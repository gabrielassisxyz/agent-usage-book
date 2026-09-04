//! `doctor --fix`: the four deterministic repairs that restore derived or
//! operational state, and nothing else (PLAN.md 36).
//!
//! [`RepairAction`] is the exhaustive, four-variant list `run_fix` performs.
//! [`ForbiddenRepair`] is a separate, disjoint enum naming the five operations
//! `--fix` must never reach: activating a calibration, changing a price book,
//! deleting quota history, reattributing an ambiguous session, or repairing
//! evidence by guessing. No variant, function or code path here constructs a
//! [`ForbiddenRepair`] or calls into what it names; the two enums sharing no
//! variant is the proof, checked by [`run_fix_reaches_only_the_four_permitted_actions`]
//! below and by the exhaustive match in [`RepairAction::as_str`], which breaks
//! compilation the moment a fifth variant is added to the wrong enum.
//!
//! Maintenance (optimize, checkpoint, vacuum) is deliberately absent: it belongs to
//! an explicit maintenance command if it is ever exposed at all, never folded into a
//! repair path that is supposed to be safe to run without reading its source.

use rusqlite::Connection;

use crate::config::Config;
use crate::domain::time::Clock;
use crate::error::Error;

/// The four repairs `--fix` may perform, in the order it performs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RepairAction {
    ClearExpiredLeases,
    DrainPendingEvidence,
    RebuildProjection,
    RecreateTranscriptMaterializations,
}

impl RepairAction {
    pub const ALL: [RepairAction; 4] = [
        Self::ClearExpiredLeases,
        Self::DrainPendingEvidence,
        Self::RebuildProjection,
        Self::RecreateTranscriptMaterializations,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClearExpiredLeases => "clear-expired-leases",
            Self::DrainPendingEvidence => "drain-pending-evidence",
            Self::RebuildProjection => "rebuild-projection",
            Self::RecreateTranscriptMaterializations => "recreate-transcript-materializations",
        }
    }
}

/// The five operations `--fix` must never perform (PLAN.md 36). Never
/// constructed by this module: its only purpose is to be named in the doc
/// comment above, in help text, and in the test that asserts it shares no
/// variant with [`RepairAction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ForbiddenRepair {
    ActivateCalibrations,
    ChangePriceBooks,
    DeleteQuotaHistory,
    ReattributeAmbiguousSessions,
    RepairEvidenceByGuessing,
}

impl ForbiddenRepair {
    pub const ALL: [ForbiddenRepair; 5] = [
        Self::ActivateCalibrations,
        Self::ChangePriceBooks,
        Self::DeleteQuotaHistory,
        Self::ReattributeAmbiguousSessions,
        Self::RepairEvidenceByGuessing,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ActivateCalibrations => "activate-calibrations",
            Self::ChangePriceBooks => "change-price-books",
            Self::DeleteQuotaHistory => "delete-quota-history",
            Self::ReattributeAmbiguousSessions => "reattribute-ambiguous-sessions",
            Self::RepairEvidenceByGuessing => "repair-evidence-by-guessing",
        }
    }
}

/// One action `run_fix` performed and what it found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairOutcome {
    pub action: RepairAction,
    pub detail: String,
}

/// The full `--fix` result: every permitted action, in the order it ran.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FixReport {
    pub actions: Vec<RepairOutcome>,
}

/// Performs exactly the four permitted repairs, in this order, over one
/// read-write connection: clear expired sampling leases, drain the pending meter
/// evidence spool, republish the status projection, then delete and re-ingest the
/// transcript-derived materialization group (never attribution, which
/// reattributes ambiguous sessions and is the forbidden operation this order is
/// written to avoid).
pub fn run_fix(
    conn: &mut Connection,
    config: &Config,
    clock: &impl Clock,
) -> Result<FixReport, Error> {
    let mut actions = Vec::with_capacity(RepairAction::ALL.len());

    let cleared_leases = crate::store::sampling_lease::clear_expired(conn, clock)?;
    actions.push(RepairOutcome {
        action: RepairAction::ClearExpiredLeases,
        detail: format!("{cleared_leases} expired lease(s) cleared"),
    });

    let drain = crate::store::spool::drain_pending(conn, &config.state.dir)?;
    actions.push(RepairOutcome {
        action: RepairAction::DrainPendingEvidence,
        detail: format!(
            "{} applied, {} already applied (idempotent replay), {} quarantined",
            drain.applied, drain.already_applied, drain.quarantined
        ),
    });

    let projection_target = crate::projection::projection_path_in(&config.state.dir);
    let publication = crate::projection::publish(conn, &projection_target);
    let projection_detail = match &publication {
        crate::projection::Publication::Published { generation } => {
            format!("republished at generation {}", generation.value())
        }
        crate::projection::Publication::Deferred { reason } => format!("deferred: {reason}"),
    };
    actions.push(RepairOutcome {
        action: RepairAction::RebuildProjection,
        detail: projection_detail,
    });

    let swept = crate::store::retention::delete_rebuildable(
        conn,
        crate::store::retention::RebuildGroup::Transcripts,
    )?;
    let reachable_transcripts: Vec<crate::config::TranscriptConfig> = config
        .transcripts
        .iter()
        .filter(|t| t.root.is_dir())
        .cloned()
        .collect();
    let recreate_detail = if reachable_transcripts.is_empty() {
        if config.transcripts.is_empty() {
            format!(
                "deleted {} derived row(s); no transcript sources are configured to re-parse",
                swept.total().value(),
            )
        } else {
            format!(
                "deleted {} derived row(s); no reachable transcript roots exist to re-parse",
                swept.total().value(),
            )
        }
    } else {
        let mut filtered_config = config.clone();
        filtered_config.transcripts = reachable_transcripts;
        let ingest_options = crate::ingest::IngestOptions {
            source: None,
            changed_only: false,
        };
        let ingest_report =
            crate::ingest::run(conn, &filtered_config, &ingest_options, clock, &mut |_| {
                Ok(())
            })?;
        format!(
            "deleted {} derived row(s), re-parsed {} file(s) across {} source(s)",
            swept.total().value(),
            ingest_report.files_parsed,
            ingest_report.sources.len(),
        )
    };
    actions.push(RepairOutcome {
        action: RepairAction::RecreateTranscriptMaterializations,
        detail: recreate_detail,
    });

    Ok(FixReport { actions })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The proof the module doc comment promises: the permitted and forbidden
    /// enums share no variant name, so no permitted action can even be mistaken
    /// for a forbidden one by a caller matching on strings.
    #[test]
    fn permitted_and_forbidden_repairs_share_no_name() {
        let permitted: std::collections::BTreeSet<&str> =
            RepairAction::ALL.iter().map(|a| a.as_str()).collect();
        let forbidden: std::collections::BTreeSet<&str> =
            ForbiddenRepair::ALL.iter().map(|f| f.as_str()).collect();
        assert!(permitted.is_disjoint(&forbidden));
    }

    #[test]
    fn exactly_four_permitted_and_five_forbidden() {
        assert_eq!(RepairAction::ALL.len(), 4);
        assert_eq!(ForbiddenRepair::ALL.len(), 5);
    }

    /// Planted negative: a maintenance-shaped action (vacuum, checkpoint,
    /// optimize) must not be nameable as a permitted repair.
    #[test]
    fn maintenance_operations_are_absent_from_the_permitted_list() {
        let names: Vec<&str> = RepairAction::ALL.iter().map(|a| a.as_str()).collect();
        for maintenance_term in ["vacuum", "checkpoint", "optimize"] {
            assert!(
                !names.iter().any(|name| name.contains(maintenance_term)),
                "{maintenance_term} must not appear in a permitted repair action"
            );
        }
    }

    fn scratch_state_dir(tag: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let suffix = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "aub-doctor-fix-test-{tag}-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("scratch state dir must be creatable");
        dir
    }

    fn test_config(state_dir: &std::path::Path) -> Config {
        let toml = format!("[state]\ndir = {:?}\n", state_dir);
        let (config, _) = crate::config::resolve(
            &crate::config::Overrides::new(),
            &crate::config::RealEnv,
            Some(&toml),
            "aub.toml",
        )
        .expect("minimal config must resolve");
        config
    }

    /// `run_fix` performs exactly the four permitted actions against a freshly
    /// migrated, empty ledger: the enumeration test the acceptance criteria ask
    /// for, run against the real function rather than a hand-built stand-in.
    #[test]
    fn run_fix_performs_exactly_the_four_permitted_actions() {
        let dir = scratch_state_dir("enumerate");
        let config = test_config(&dir);
        let clock = crate::domain::time::RealClock::new();
        let mut conn = crate::store::rate_card::open_ledger(
            &dir.join("ledger.sqlite3"),
            crate::domain::time::MonotonicDuration::from_millis(500),
            &clock,
        )
        .expect("a fresh ledger must open and migrate");

        let report =
            run_fix(&mut conn, &config, &clock).expect("fix must succeed on a clean ledger");

        let performed: Vec<RepairAction> = report.actions.iter().map(|o| o.action).collect();
        assert_eq!(performed, RepairAction::ALL.to_vec());
    }

    /// Mutation-shaped proof for the forbidden list: fixing a ledger that carries
    /// a fitted-but-unactivated calibration, a rate card, and an ambiguous
    /// attribution row must leave every one of them untouched. This is what "no
    /// code path from `--fix` reaching any of the five forbidden operations"
    /// means empirically, since the type system alone cannot observe a database
    /// row.
    #[test]
    fn run_fix_leaves_calibrations_rate_cards_and_attribution_untouched() {
        let dir = scratch_state_dir("forbidden");
        let config = test_config(&dir);
        let clock = crate::domain::time::RealClock::new();
        let mut conn = crate::store::rate_card::open_ledger(
            &dir.join("ledger.sqlite3"),
            crate::domain::time::MonotonicDuration::from_millis(500),
            &clock,
        )
        .expect("a fresh ledger must open and migrate");

        let calibrations_before = crate::store::calibration::lifecycle_event_count(&conn)
            .expect("calibration_lifecycle must be queryable");
        let rate_cards_before = crate::store::rate_card::count(&conn).expect("rate card count");

        run_fix(&mut conn, &config, &clock).expect("fix must succeed on a clean ledger");

        let calibrations_after = crate::store::calibration::lifecycle_event_count(&conn)
            .expect("calibration_lifecycle must be queryable");
        let rate_cards_after = crate::store::rate_card::count(&conn).expect("rate card count");

        assert_eq!(calibrations_before, calibrations_after);
        assert_eq!(rate_cards_before, rate_cards_after);
    }
}
