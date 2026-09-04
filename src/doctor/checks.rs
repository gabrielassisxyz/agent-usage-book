//! The registered checks: the seven this bead owns (sampling cadence, unresolved
//! authentication, transcript roots, backup age, projection versus database
//! generation, clock skew, missing active calibrations), the eleven whose evidence
//! belongs elsewhere but whose subsystem already exists and is read here, and the
//! three not-yet-available placeholders naming the bead that will own them.
//!
//! Every check is read-only: `doctor` performs no network operation and no check
//! here writes to the ledger. [`super::fix`] is the only writer, and only under
//! `--fix`.

use std::path::PathBuf;

use rusqlite::Connection;

use crate::config::Config;
use crate::domain::time::UtcTimestamp;

use super::{CheckName, CheckOutcome, CheckStatus};

/// Everything a check needs to read, gathered once by the caller (`cli.rs`) so no
/// check opens its own connection or re-resolves configuration. `db` is `None`
/// whenever `db_missing` is true or `db_open_error` is set; a check that needs the
/// ledger reports [`CheckStatus::NotApplicable`] in the first case and
/// [`CheckStatus::Fail`] in the second, since a database that exists but will not
/// open is a finding, not an absence.
pub struct DoctorContext<'a> {
    pub config: &'a Config,
    pub timestamp: UtcTimestamp,
    pub db_path: PathBuf,
    pub db: Option<&'a Connection>,
    pub db_missing: bool,
    pub db_open_error: Option<String>,
}

/// Builds the full registry: reaching this function at all means configuration
/// resolved, so [`CheckName::ConfigurationValidity`] is always [`CheckStatus::Pass`]
/// here. A configuration failure is reported by
/// [`configuration_failed_registry`] instead, which this function's caller reaches
/// for before a [`DoctorContext`] can even be built.
pub fn build_registry(ctx: &DoctorContext) -> Vec<CheckOutcome> {
    vec![
        CheckOutcome {
            name: CheckName::ConfigurationValidity,
            owner_module: "config",
            condition: "the resolved configuration has no invalid or conflicting key",
            has_repair: false,
            status: CheckStatus::Pass,
        },
        sqlite_and_schema_health(ctx),
        strict_and_constraint_integrity(ctx),
        pending_evidence(ctx),
        sampling_cadence(ctx),
        unresolved_authentication(ctx),
        transcript_roots(ctx),
        parser_failures(ctx),
        CheckOutcome {
            name: CheckName::UnmappedAccounts,
            owner_module: "attribution",
            condition: "every observed account maps to a configured one",
            has_repair: false,
            status: CheckStatus::NotYetAvailable {
                owning_bead: "aub-mgv.3",
            },
        },
        missing_active_calibrations(ctx),
        stale_rate_cards(ctx),
        projection_versus_database_generation(ctx),
        backup_age(ctx),
        CheckOutcome {
            name: CheckName::MeterAnomalies,
            owner_module: "meter",
            condition: "no meter window carries an anomalous reading",
            has_repair: false,
            status: CheckStatus::NotYetAvailable {
                owning_bead: "aub-eun.14",
            },
        },
        unexplained_residual(ctx),
        heuristic_dedup_counts(ctx),
        clock_skew(ctx),
        local_filesystem_and_wal_suitability(ctx),
        accumulated_diagnostic_material(ctx),
        adapter_semantics_comparison_age(ctx),
        last_sample_tick(ctx),
    ]
}

/// The registry for the case configuration itself failed to resolve: every other
/// check needs the configuration it would have read, so each is
/// [`CheckStatus::NotApplicable`] rather than silently absent.
pub fn configuration_failed_registry(error: &str) -> Vec<CheckOutcome> {
    let unresolved = format!("configuration failed to resolve: {error}");
    CheckName::EXPECTED
        .iter()
        .map(|name| {
            if *name == CheckName::ConfigurationValidity {
                CheckOutcome {
                    name: *name,
                    owner_module: "config",
                    condition: "the resolved configuration has no invalid or conflicting key",
                    has_repair: false,
                    status: CheckStatus::Fail(error.to_string()),
                }
            } else {
                CheckOutcome {
                    name: *name,
                    owner_module: owner_of(*name),
                    condition: condition_of(*name),
                    has_repair: has_repair_of(*name),
                    status: CheckStatus::NotApplicable(unresolved.clone()),
                }
            }
        })
        .collect()
}

fn owner_of(name: CheckName) -> &'static str {
    match name {
        CheckName::ConfigurationValidity => "config",
        CheckName::SqliteAndSchemaHealth => "store::backup",
        CheckName::StrictAndConstraintIntegrity => "store::schema_audit",
        CheckName::PendingEvidence => "store::spool",
        CheckName::SamplingCadence => "doctor",
        CheckName::UnresolvedAuthentication => "doctor",
        CheckName::TranscriptRoots => "doctor",
        CheckName::ParserFailures => "store::ingest_quarantine",
        CheckName::UnmappedAccounts => "attribution",
        CheckName::MissingActiveCalibrations => "doctor",
        CheckName::StaleRateCards => "store::rate_card",
        CheckName::ProjectionVersusDatabaseGeneration => "doctor",
        CheckName::BackupAge => "doctor",
        CheckName::MeterAnomalies => "meter",
        CheckName::UnexplainedResidual => "reconciliation",
        CheckName::HeuristicDedupCounts => "store::ingest_quarantine",
        CheckName::ClockSkew => "doctor",
        CheckName::LocalFilesystemAndWalSuitability => "store::startup",
        CheckName::AccumulatedDiagnosticMaterial => "store::retention",
        CheckName::AdapterSemanticsComparisonAge => "store::adapter_semantics_validation",
        CheckName::LastSampleTick => "store::sample_tick",
    }
}

fn condition_of(name: CheckName) -> &'static str {
    match name {
        CheckName::ConfigurationValidity => {
            "the resolved configuration has no invalid or conflicting key"
        }
        CheckName::SqliteAndSchemaHealth => {
            "the ledger database passes SQLite's own integrity and foreign-key checks"
        }
        CheckName::StrictAndConstraintIntegrity => {
            "every table is STRICT and every quantity column is constrained"
        }
        CheckName::PendingEvidence => "no meter evidence is stuck undrained in the pending spool",
        CheckName::SamplingCadence => "every configured account has a recent sampling attempt",
        CheckName::UnresolvedAuthentication => "every configured account's credential resolves",
        CheckName::TranscriptRoots => "every configured transcript root exists and is reachable",
        CheckName::ParserFailures => "no transcript record is quarantined for a parser failure",
        CheckName::UnmappedAccounts => "every observed account maps to a configured one",
        CheckName::MissingActiveCalibrations => {
            "every scope with a fitted calibration has one currently active"
        }
        CheckName::StaleRateCards => "no imported rate card is past its review-due date",
        CheckName::ProjectionVersusDatabaseGeneration => {
            "the published projection's generation matches the database's"
        }
        CheckName::BackupAge => {
            "the last verified backup and the last successful drill are each within their \
             configured review horizon"
        }
        CheckName::MeterAnomalies => "no meter window carries an anomalous reading",
        CheckName::UnexplainedResidual => "rolling residual stays within its explained bound",
        CheckName::HeuristicDedupCounts => {
            "no usage record was quarantined for a heuristic-key collision"
        }
        CheckName::ClockSkew => {
            "no recent attempt recorded a provider timestamp outside the skew envelope"
        }
        CheckName::LocalFilesystemAndWalSuitability => {
            "the state directory is local, mode 0700 and writable"
        }
        CheckName::AccumulatedDiagnosticMaterial => {
            "retained diagnostic capture material does not accumulate unnoticed"
        }
        CheckName::AdapterSemanticsComparisonAge => {
            "the newest adapter-semantics comparison against the provider's authoritative \
             surface is within its configured review horizon"
        }
        CheckName::LastSampleTick => "the last aub sample invocation succeeded",
    }
}

/// Whether `--fix` has a repair that addresses this check's own failure mode:
/// draining the pending spool answers [`CheckName::PendingEvidence`], republishing
/// the projection answers [`CheckName::ProjectionVersusDatabaseGeneration`], and
/// recreating the transcript materialization group answers
/// [`CheckName::ParserFailures`] and [`CheckName::HeuristicDedupCounts`], since
/// both live in the same rebuilt `ingest_quarantine` table
/// (`store::retention::RebuildGroup::Transcripts`). Clearing expired leases is a
/// fifth permitted action with no check of its own in the eighteen-item list, so
/// no [`CheckName`] variant claims it; sampling cadence stays `false` because
/// `--fix` performs no network operation and cannot make an account get sampled.
fn has_repair_of(name: CheckName) -> bool {
    matches!(
        name,
        CheckName::PendingEvidence
            | CheckName::ProjectionVersusDatabaseGeneration
            | CheckName::ParserFailures
            | CheckName::HeuristicDedupCounts
    )
}

fn outcome(name: CheckName, status: CheckStatus) -> CheckOutcome {
    CheckOutcome {
        name,
        owner_module: owner_of(name),
        condition: condition_of(name),
        has_repair: has_repair_of(name),
        status,
    }
}

/// SQLite's own health: pragma integrity_check and pragma foreign_key_check,
/// via the same function backup verification runs (`store::backup`).
fn sqlite_and_schema_health(ctx: &DoctorContext) -> CheckOutcome {
    let status = if ctx.db_missing {
        CheckStatus::NotApplicable(format!(
            "no ledger database exists yet at {}",
            ctx.db_path.display()
        ))
    } else if let Some(error) = &ctx.db_open_error {
        CheckStatus::Fail(format!("cannot open the ledger database: {error}"))
    } else {
        match ctx.db {
            None => CheckStatus::Fail("no open connection to the ledger database".to_string()),
            Some(conn) => match crate::store::backup::verify_database_on_connection(conn) {
                Ok(Ok(_)) => CheckStatus::Pass,
                Ok(Err(failure)) => {
                    CheckStatus::Fail(format!("{}: {}", failure.stage.as_str(), failure.detail))
                }
                Err(error) => CheckStatus::Fail(format!("cannot run health checks: {error}")),
            },
        }
    };
    outcome(CheckName::SqliteAndSchemaHealth, status)
}

/// STRICT tables and column constraints (`store::schema_audit`), which owns this
/// audit's own doc comment naming this bead as the consumer that renders it.
fn strict_and_constraint_integrity(ctx: &DoctorContext) -> CheckOutcome {
    let status = if ctx.db_missing {
        CheckStatus::NotApplicable(format!(
            "no ledger database exists yet at {}",
            ctx.db_path.display()
        ))
    } else if let Some(error) = &ctx.db_open_error {
        CheckStatus::Fail(format!("cannot open the ledger database: {error}"))
    } else {
        match ctx.db {
            None => CheckStatus::Fail("no open connection to the ledger database".to_string()),
            Some(conn) => match crate::store::schema_audit::audit(conn) {
                Ok(audit) if audit.is_clean() => CheckStatus::Pass,
                Ok(audit) => CheckStatus::Fail(
                    audit
                        .report()
                        .unwrap_or_else(|| "schema audit found findings".to_string()),
                ),
                Err(error) => CheckStatus::Fail(format!("cannot audit the schema: {error}")),
            },
        }
    };
    outcome(CheckName::StrictAndConstraintIntegrity, status)
}

/// Pending meter evidence still on disk, undrained into the ledger. Reads the
/// spool directory directly rather than draining it: a check must not mutate.
fn pending_evidence(ctx: &DoctorContext) -> CheckOutcome {
    let pending_dir = crate::store::spool::pending_dir(&ctx.config.state.dir);
    let status = match std::fs::read_dir(&pending_dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => CheckStatus::Pass,
        Err(error) => CheckStatus::Fail(format!("cannot read the pending spool: {error}")),
        Ok(entries) => {
            let count = entries
                .filter_map(Result::ok)
                .filter(|entry| crate::store::spool::is_pending_record_name(&entry.path()))
                .count();
            if count == 0 {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail(format!(
                    "{count} pending record(s) undrained in {}",
                    pending_dir.display()
                ))
            }
        }
    };
    outcome(CheckName::PendingEvidence, status)
}

/// One configured account's latest sampling attempt is older than three times the
/// configured default interval, or it has never had one at all.
fn sampling_cadence(ctx: &DoctorContext) -> CheckOutcome {
    let status = if ctx.config.accounts.is_empty() {
        CheckStatus::NotApplicable("no accounts configured".to_string())
    } else if ctx.db_missing {
        CheckStatus::NotApplicable(
            "no ledger database exists yet; nothing has been sampled".to_string(),
        )
    } else if let Some(error) = &ctx.db_open_error {
        CheckStatus::Fail(format!("cannot open the ledger database: {error}"))
    } else {
        match ctx.db {
            None => CheckStatus::Fail("no open connection to the ledger database".to_string()),
            Some(conn) => {
                let threshold_nanos = ctx
                    .config
                    .sampling
                    .default_interval
                    .as_nanos()
                    .saturating_mul(3);
                let mut stale = Vec::new();
                for account in &ctx.config.accounts {
                    let lookup = crate::store::account::account_id_by_identity(
                        conn,
                        &account.provider,
                        &account.name,
                    );
                    let id = match lookup {
                        Ok(Some(id)) => id,
                        Ok(None) => {
                            stale.push(format!("{}: never observed", account.name));
                            continue;
                        }
                        Err(error) => {
                            stale.push(format!("{}: {error}", account.name));
                            continue;
                        }
                    };
                    match crate::store::meter_attempt::latest_attempt_for_account(conn, id) {
                        Ok(None) => stale.push(format!("{}: never sampled", account.name)),
                        Ok(Some(attempt)) => {
                            let gap_nanos = ctx
                                .timestamp
                                .unix_nanos()
                                .saturating_sub(attempt.request_started_at.unix_nanos())
                                as u64;
                            if gap_nanos > threshold_nanos {
                                stale.push(format!(
                                    "{}: last attempt {}s ago",
                                    account.name,
                                    gap_nanos / 1_000_000_000
                                ));
                            }
                        }
                        Err(error) => stale.push(format!("{}: {error}", account.name)),
                    }
                }
                if stale.is_empty() {
                    CheckStatus::Pass
                } else {
                    CheckStatus::Fail(stale.join("; "))
                }
            }
        }
    };
    outcome(CheckName::SamplingCadence, status)
}

/// Every configured account's credential resolves (`auth::resolve`), performed
/// against the real filesystem and never over the network.
fn unresolved_authentication(ctx: &DoctorContext) -> CheckOutcome {
    let status = if ctx.config.accounts.is_empty() {
        CheckStatus::NotApplicable("no accounts configured".to_string())
    } else {
        let mut unresolved = Vec::new();
        for account in &ctx.config.accounts {
            if let Err(error) = crate::auth::resolve(account, &crate::auth::RealFs, false) {
                unresolved.push(format!("{}: {error}", account.name));
            }
        }
        if unresolved.is_empty() {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail(unresolved.join("; "))
        }
    };
    outcome(CheckName::UnresolvedAuthentication, status)
}

/// Every configured transcript root exists on disk. Distinct from the deeper
/// `--transcript-format-drift` report: this is a cheap reachability check, not a
/// shape comparison against the fixture corpus.
fn transcript_roots(ctx: &DoctorContext) -> CheckOutcome {
    let status = if ctx.config.transcripts.is_empty() {
        CheckStatus::NotApplicable("no transcript sources configured".to_string())
    } else {
        let missing: Vec<String> = ctx
            .config
            .transcripts
            .iter()
            .filter(|source| std::fs::metadata(&source.root).is_err())
            .map(|source| format!("{} ({})", source.name, source.root.display()))
            .collect();
        if missing.is_empty() {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail(format!("unreachable root(s): {}", missing.join(", ")))
        }
    };
    outcome(CheckName::TranscriptRoots, status)
}

/// Quarantined transcript records whose failure class is a genuine parse failure,
/// as opposed to a heuristic dedup collision ([`heuristic_dedup_counts`]), which
/// the same table records under a distinct failure class.
fn parser_failures(ctx: &DoctorContext) -> CheckOutcome {
    let status = quarantine_count(ctx, |class| {
        class != crate::store::ingest_quarantine::DEDUP_COLLISION_FAILURE_CLASS
    })
    .map(|count| {
        if count == 0 {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail(format!(
                "{count} record(s) quarantined for a parser failure"
            ))
        }
    })
    .unwrap_or_else(std::convert::identity);
    outcome(CheckName::ParserFailures, status)
}

/// Usage records quarantined for colliding on a heuristic identity key: two
/// records the dedup layer could not tell apart, so neither was kept.
fn heuristic_dedup_counts(ctx: &DoctorContext) -> CheckOutcome {
    let status = quarantine_count(ctx, |class| {
        class == crate::store::ingest_quarantine::DEDUP_COLLISION_FAILURE_CLASS
    })
    .map(|count| {
        if count == 0 {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail(format!(
                "{count} record(s) quarantined for a heuristic-key collision"
            ))
        }
    })
    .unwrap_or_else(std::convert::identity);
    outcome(CheckName::HeuristicDedupCounts, status)
}

/// Sums quarantine group counts whose failure class matches `predicate`, or
/// reports why it could not: absent database is not applicable, the two share the
/// same table so this returns `Ok(None)` for "not applicable" and the reason
/// separately handled by each caller through the `Result` here.
fn quarantine_count(
    ctx: &DoctorContext,
    predicate: impl Fn(&str) -> bool,
) -> Result<u64, CheckStatus> {
    if ctx.db_missing {
        return Err(CheckStatus::NotApplicable(
            "no ledger database exists yet".to_string(),
        ));
    }
    if let Some(error) = &ctx.db_open_error {
        return Err(CheckStatus::Fail(format!(
            "cannot open the ledger database: {error}"
        )));
    }
    let conn = ctx.db.ok_or_else(|| {
        CheckStatus::Fail("no open connection to the ledger database".to_string())
    })?;
    let groups = crate::store::ingest_quarantine::quarantine_summary(conn).map_err(|error| {
        CheckStatus::Fail(format!("cannot read the quarantine summary: {error}"))
    })?;
    Ok(groups
        .iter()
        .filter(|group| predicate(&group.failure_class))
        .map(|group| group.count)
        .sum())
}

/// Every scope (provider, plan tier, window) that has ever had a calibration
/// fitted has one currently active. A scope that was fitted but never activated,
/// or whose activation was superseded with nothing replacing it, fails naming the
/// scope.
fn missing_active_calibrations(ctx: &DoctorContext) -> CheckOutcome {
    let status = if ctx.db_missing {
        CheckStatus::NotApplicable("no ledger database exists yet".to_string())
    } else if let Some(error) = &ctx.db_open_error {
        CheckStatus::Fail(format!("cannot open the ledger database: {error}"))
    } else {
        match ctx.db {
            None => CheckStatus::Fail("no open connection to the ledger database".to_string()),
            Some(conn) => match crate::store::calibration::fitted_calibration_scopes(conn) {
                Err(error) => CheckStatus::Fail(format!("cannot read calibration scopes: {error}")),
                Ok(scopes) if scopes.is_empty() => {
                    CheckStatus::NotApplicable("no calibration has ever been fitted".to_string())
                }
                Ok(scopes) => {
                    let mut missing = Vec::new();
                    for scope in &scopes {
                        match crate::store::calibration::load_active_at(conn, scope, ctx.timestamp)
                        {
                            Ok(Some(_)) => {}
                            Ok(None) => missing.push(format!(
                                "{}/{}/{}",
                                scope.provider.as_str(),
                                scope.plan_tier.as_str(),
                                scope.window_semantic_key.as_str()
                            )),
                            Err(error) => missing.push(format!(
                                "{}/{}/{}: {error}",
                                scope.provider.as_str(),
                                scope.plan_tier.as_str(),
                                scope.window_semantic_key.as_str()
                            )),
                        }
                    }
                    if missing.is_empty() {
                        CheckStatus::Pass
                    } else {
                        CheckStatus::Fail(format!(
                            "no active calibration for: {}",
                            missing.join(", ")
                        ))
                    }
                }
            },
        }
    };
    outcome(CheckName::MissingActiveCalibrations, status)
}

/// Imported rate cards past their review-due date, where valuation is configured
/// at all: `store::rate_card::stale_rate_cards` is the same function the
/// pre-registry `doctor --rate-card-staleness` flag already used.
fn stale_rate_cards(ctx: &DoctorContext) -> CheckOutcome {
    let status = if ctx.db_missing {
        CheckStatus::NotApplicable("no ledger database exists yet".to_string())
    } else if let Some(error) = &ctx.db_open_error {
        CheckStatus::Fail(format!("cannot open the ledger database: {error}"))
    } else {
        match ctx.db {
            None => CheckStatus::Fail("no open connection to the ledger database".to_string()),
            Some(conn) => match crate::store::rate_card::stale_rate_cards(conn, ctx.timestamp) {
                Err(error) => CheckStatus::Fail(format!("cannot read stale rate cards: {error}")),
                Ok(cards) if cards.is_empty() => CheckStatus::Pass,
                Ok(cards) => {
                    let names: Vec<String> = cards
                        .iter()
                        .map(|card| {
                            format!(
                                "{} {} {}",
                                card.draft.vendor,
                                card.draft.model,
                                card.draft.token_class.as_str()
                            )
                        })
                        .collect();
                    CheckStatus::Fail(format!("review due: {}", names.join(", ")))
                }
            },
        }
    };
    outcome(CheckName::StaleRateCards, status)
}

/// The published projection's ledger generation compared against the database's
/// current one. Behind is a normal repair case (`--fix` republishes); ahead is a
/// corruption signal per `projection.rs`'s own invariant, never a race.
fn projection_versus_database_generation(ctx: &DoctorContext) -> CheckOutcome {
    let status = if ctx.db_missing {
        CheckStatus::NotApplicable("no ledger database exists yet".to_string())
    } else if let Some(error) = &ctx.db_open_error {
        CheckStatus::Fail(format!("cannot open the ledger database: {error}"))
    } else {
        match ctx.db {
            None => CheckStatus::Fail("no open connection to the ledger database".to_string()),
            Some(conn) => {
                let projection_path = crate::projection::projection_path_in(&ctx.config.state.dir);
                match std::fs::read_to_string(&projection_path) {
                    Err(_) => CheckStatus::NotApplicable(
                        "no projection has been published yet".to_string(),
                    ),
                    Ok(text) => match crate::projection::recorded_generation(&text) {
                        None => CheckStatus::Fail(
                            "the projection file exists but its generation could not be read"
                                .to_string(),
                        ),
                        Some(projected) => match crate::store::ledger_generation::current(conn) {
                            Err(error) => CheckStatus::Fail(format!(
                                "cannot read the ledger generation: {error}"
                            )),
                            Ok(current) if current.value() == projected => CheckStatus::Pass,
                            Ok(current) if projected < current.value() => {
                                CheckStatus::Fail(format!(
                                    "projection is generation {projected}, database is at {}",
                                    current.value()
                                ))
                            }
                            Ok(current) => CheckStatus::Fail(format!(
                                "projection is generation {projected}, ahead of the database's {}; \
                                 this is a corruption signal, not a race",
                                current.value()
                            )),
                        },
                    },
                }
            }
        }
    };
    outcome(CheckName::ProjectionVersusDatabaseGeneration, status)
}

/// Whether one of the two review-horizon subchecks below found something to
/// report: configured and healthy, configured and stale or missing, or not
/// configured at all. Kept distinct from [`CheckStatus`] because two of these
/// combine into one [`CheckStatus`]: "neither configured" is the only
/// [`CheckStatus::NotApplicable`] case, and either one being
/// [`SubcheckVerdict::Failed`] fails the whole check, naming which.
enum SubcheckVerdict {
    NotConfigured,
    Ok,
    Failed(String),
}

/// The age of the last verified backup, when `backup.destination` is
/// configured. `store::backup::backup_health`'s own doc comment names this
/// bead as the later doctor registry it was built to feed.
fn backup_subcheck(ctx: &DoctorContext) -> SubcheckVerdict {
    match &ctx.config.backup.destination {
        None => SubcheckVerdict::NotConfigured,
        Some(destination) => match crate::backup::backup_health(
            destination,
            ctx.timestamp,
            ctx.config.backup.review_after,
        ) {
            Err(error) => SubcheckVerdict::Failed(format!("cannot read the backup: {error}")),
            Ok(crate::backup::BackupHealth::Missing) => {
                SubcheckVerdict::Failed(format!("no backup found at {}", destination.display()))
            }
            Ok(crate::backup::BackupHealth::Unverified { .. }) => SubcheckVerdict::Failed(format!(
                "a backup exists at {} but has not been verified",
                destination.display()
            )),
            Ok(crate::backup::BackupHealth::Verified {
                age,
                review_due: true,
                ..
            }) => SubcheckVerdict::Failed(format!(
                "the last verified backup is {}s old, past its review horizon",
                age.as_nanos() / 1_000_000_000
            )),
            Ok(crate::backup::BackupHealth::Verified {
                review_due: false, ..
            }) => SubcheckVerdict::Ok,
        },
    }
}

/// The age of the last successful drill, when `drill.result` is configured.
/// Mirrors [`backup_subcheck`] exactly, for the same reason `aub drill`
/// mirrors `aub backup`: an untested restore path is a materially different
/// risk from an unbacked-up ledger, and both ages are read independently so
/// the finding can name which one is stale (`aub-n27.2`).
fn drill_subcheck(ctx: &DoctorContext) -> SubcheckVerdict {
    match &ctx.config.drill.result {
        None => SubcheckVerdict::NotConfigured,
        Some(result_path) => {
            match crate::drill::drill_health(result_path, ctx.timestamp, ctx.config.drill.max_age) {
                Err(error) => {
                    SubcheckVerdict::Failed(format!("cannot read the drill result record: {error}"))
                }
                Ok(crate::drill::DrillHealth::Missing) => SubcheckVerdict::Failed(format!(
                    "no successful drill recorded at {}",
                    result_path.display()
                )),
                Ok(crate::drill::DrillHealth::Verified {
                    age,
                    review_due: true,
                    ..
                }) => SubcheckVerdict::Failed(format!(
                    "the last successful drill is {}s old, past its review horizon",
                    age.as_nanos() / 1_000_000_000
                )),
                Ok(crate::drill::DrillHealth::Verified {
                    review_due: false, ..
                }) => SubcheckVerdict::Ok,
            }
        }
    }
}

/// The age of the last verified backup and the age of the last successful
/// drill, combined into one check because PLAN.md section 36 names one
/// "last verified backup" review-horizon condition rather than two: a
/// nineteenth [`CheckName`] would contradict the design's own stated count of
/// eighteen. Both ages are still computed and reported independently by
/// [`backup_subcheck`] and [`drill_subcheck`]; only neither being configured
/// reads as not applicable, and either one failing fails the whole check,
/// naming which.
fn backup_age(ctx: &DoctorContext) -> CheckOutcome {
    let backup = backup_subcheck(ctx);
    let drill = drill_subcheck(ctx);
    let status = match (&backup, &drill) {
        (SubcheckVerdict::NotConfigured, SubcheckVerdict::NotConfigured) => {
            CheckStatus::NotApplicable(
                "neither backup.destination nor drill.result is configured".to_string(),
            )
        }
        _ => {
            let mut failures = Vec::new();
            if let SubcheckVerdict::Failed(reason) = &backup {
                failures.push(format!("backup: {reason}"));
            }
            if let SubcheckVerdict::Failed(reason) = &drill {
                failures.push(format!("drill: {reason}"));
            }
            if failures.is_empty() {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail(failures.join("; "))
            }
        }
    };
    outcome(CheckName::BackupAge, status)
}

/// The age of the newest recorded adapter-semantics comparison
/// (`store::adapter_semantics_validation::latest_comparison_read_at`), the
/// bookkeeping half of docs/adapter-semantics-validation.md. No comparison
/// ever recorded is [`CheckStatus::NotApplicable`] rather than a failure: the
/// procedure is manual and recurring, and a fresh ledger has not had a chance
/// to run it yet. Once one exists, its age is judged against
/// `adapter_semantics.max_comparison_age`, the same review-horizon shape
/// [`backup_age`] uses for `backup.review_after` and `drill.max_age`.
fn adapter_semantics_comparison_age(ctx: &DoctorContext) -> CheckOutcome {
    let status = if ctx.db_missing {
        CheckStatus::NotApplicable("no ledger database exists yet".to_string())
    } else if let Some(error) = &ctx.db_open_error {
        CheckStatus::Fail(format!("cannot open the ledger database: {error}"))
    } else {
        match ctx.db {
            None => CheckStatus::Fail("no open connection to the ledger database".to_string()),
            Some(conn) => {
                match crate::store::adapter_semantics_validation::latest_comparison_read_at(conn) {
                    Err(error) => CheckStatus::Fail(format!(
                        "cannot read the latest adapter-semantics comparison: {error}"
                    )),
                    Ok(None) => CheckStatus::NotApplicable(
                        "no adapter-semantics comparison has been recorded yet; see \
                         docs/adapter-semantics-validation.md"
                            .to_string(),
                    ),
                    Ok(Some(read_at)) => {
                        let age_nanos = ctx
                            .timestamp
                            .unix_nanos()
                            .saturating_sub(read_at.unix_nanos())
                            .max(0) as u64;
                        let max_age_nanos =
                            ctx.config.adapter_semantics.max_comparison_age.as_nanos();
                        if age_nanos > max_age_nanos {
                            CheckStatus::Fail(format!(
                                "the last adapter-semantics comparison is {}s old, past its \
                                 {}s review horizon",
                                age_nanos / 1_000_000_000,
                                max_age_nanos / 1_000_000_000,
                            ))
                        } else {
                            CheckStatus::PassWithDetail(format!(
                                "the last adapter-semantics comparison is {}s old",
                                age_nanos / 1_000_000_000,
                            ))
                        }
                    }
                }
            }
        }
    };
    outcome(CheckName::AdapterSemanticsComparisonAge, status)
}

/// The most recent window a recent attempt's provider timestamp was checked
/// against the local receive time. `meter_attempt_result.clock_anomaly` is set at
/// evidence-recording time (`domain::freshness::age`), so this reads the stored
/// bit rather than recomputing skew.
const CLOCK_SKEW_LOOKBACK_NANOS: i64 = 24 * 60 * 60 * 1_000_000_000;

fn clock_skew(ctx: &DoctorContext) -> CheckOutcome {
    let status = if ctx.db_missing {
        CheckStatus::NotApplicable("no ledger database exists yet".to_string())
    } else if let Some(error) = &ctx.db_open_error {
        CheckStatus::Fail(format!("cannot open the ledger database: {error}"))
    } else {
        match ctx.db {
            None => CheckStatus::Fail("no open connection to the ledger database".to_string()),
            Some(conn) => {
                let since = UtcTimestamp::from_unix_nanos(
                    ctx.timestamp
                        .unix_nanos()
                        .saturating_sub(CLOCK_SKEW_LOOKBACK_NANOS),
                );
                let count = crate::store::meter_attempt::count_clock_anomalies_since(conn, since);
                match count {
                    Err(error) => {
                        CheckStatus::Fail(format!("cannot count clock anomalies: {error}"))
                    }
                    Ok(0) => CheckStatus::Pass,
                    Ok(n) => CheckStatus::Fail(format!(
                        "{n} attempt(s) in the last 24h recorded a provider timestamp outside the skew envelope"
                    )),
                }
            }
        }
    };
    outcome(CheckName::ClockSkew, status)
}

/// The rolling residual health over recent eligible intervals (PLAN.md 35, 36, aub-dpn.3).
///
/// Computes rolling residual interval and fraction over the configured window.
/// Suppresses the verdict when eligible intervals are below the minimum threshold.
/// Reports patterns with candidate explanations and a calibration pointer for step changes.
fn unexplained_residual(ctx: &DoctorContext) -> CheckOutcome {
    let status = if ctx.db_missing {
        CheckStatus::NotApplicable("no ledger database exists yet".to_string())
    } else if let Some(error) = &ctx.db_open_error {
        CheckStatus::Fail(format!("cannot open the ledger database: {error}"))
    } else {
        match ctx.db {
            None => CheckStatus::Fail("no open connection to the ledger database".to_string()),
            Some(_) => match rolling_residual_health(ctx) {
                None => CheckStatus::NotApplicable(
                    "no eligible reconciliation intervals in recent window".to_string(),
                ),
                Some(health) => match &health.verdict {
                    crate::reconciliation::RollingResidualVerdict::Suppressed {
                        eligible_count,
                        min_eligible,
                    } => CheckStatus::PassWithDetail(format!(
                        "{eligible_count} eligible interval(s) in window (below minimum {min_eligible}); verdict suppressed"
                    )),
                    crate::reconciliation::RollingResidualVerdict::ReconcilesWithinUncertainty => {
                        CheckStatus::PassWithDetail(
                            "rolling residual reconciles within uncertainty".to_string(),
                        )
                    }
                    crate::reconciliation::RollingResidualVerdict::Discrepancy { patterns } => {
                        if patterns.is_empty() {
                            CheckStatus::Fail(format!(
                                "rolling residual discrepancy: interval [{} .. {}] credits",
                                health.rolling_residual_interval.lower().micros(),
                                health.rolling_residual_interval.upper().micros(),
                            ))
                        } else {
                            let explanations: Vec<&'static str> =
                                patterns.iter().map(|p| p.explanation()).collect();
                            let mut msg = format!(
                                "rolling residual discrepancy: interval [{} .. {}] credits; {}",
                                health.rolling_residual_interval.lower().micros(),
                                health.rolling_residual_interval.upper().micros(),
                                explanations.join("; "),
                            );
                            if let Some(ptr) = health.pointer {
                                msg.push_str(&format!("; {ptr}"));
                            }
                            CheckStatus::Fail(msg)
                        }
                    }
                },
            },
        }
    };
    outcome(CheckName::UnexplainedResidual, status)
}

/// Loads the rolling residual health from the store using the doctor context.
pub fn rolling_residual_health(
    ctx: &DoctorContext,
) -> Option<crate::reconciliation::RollingResidualHealth> {
    if ctx.db_missing || ctx.db_open_error.is_some() {
        return None;
    }
    let conn = ctx.db?;
    crate::store::reconciliation::load_rolling_residual_from_store(conn, ctx.config, ctx.timestamp)
        .unwrap_or(None)
}

/// The state directory is local, present, mode 0700 and writable
/// (`store::startup::ensure_state_dir_ready`), whose own doc comment names this
/// bead as the eventual consumer of the facts it exposes.
fn local_filesystem_and_wal_suitability(ctx: &DoctorContext) -> CheckOutcome {
    let mounts = crate::store::startup::ProcMounts;
    let status = match crate::store::startup::ensure_state_dir_ready(&ctx.config.state.dir, &mounts)
    {
        Ok(()) => CheckStatus::Pass,
        Err(error) => CheckStatus::Fail(error.to_string()),
    };
    outcome(CheckName::LocalFilesystemAndWalSuitability, status)
}

/// Reports accumulated diagnostic material: retained bodies per provider and source,
/// total bytes occupied, and quarantine rows.
///
/// Retained bodies are disposable captures cleared by operator command. Quarantine
/// rows record parse and dedup failures and are never cleared by the clearing path.
fn accumulated_diagnostic_material(ctx: &DoctorContext) -> CheckOutcome {
    let summaries =
        crate::store::retention::list_retained_bodies(&ctx.config.state.dir).unwrap_or_default();
    let total_retained: u64 = summaries.iter().map(|s| s.count).sum();
    let total_bytes: u64 = summaries.iter().map(|s| s.total_bytes).sum();

    let mut quarantine_by_source: std::collections::BTreeMap<String, u64> =
        std::collections::BTreeMap::new();
    if let Some(conn) = ctx.db
        && let Ok(groups) = crate::store::ingest_quarantine::quarantine_summary(conn)
    {
        for group in groups {
            *quarantine_by_source.entry(group.parser).or_insert(0) += group.count;
        }
    }
    let total_quarantine: u64 = quarantine_by_source.values().sum();

    let retained_part = if total_retained == 0 {
        "retained bodies: 0 (0 bytes)".to_string()
    } else {
        let details: Vec<String> = summaries
            .iter()
            .map(|s| {
                format!(
                    "{}/{}: {} ({} bytes)",
                    s.provider, s.source, s.count, s.total_bytes
                )
            })
            .collect();
        format!(
            "retained bodies: {} ({} bytes) [{}]",
            total_retained,
            total_bytes,
            details.join(", ")
        )
    };

    let quarantine_part = if total_quarantine == 0 {
        "quarantine rows: 0".to_string()
    } else {
        let details: Vec<String> = quarantine_by_source
            .iter()
            .map(|(source, count)| format!("{source}: {count}"))
            .collect();
        format!(
            "quarantine rows: {total_quarantine} [{}]",
            details.join(", ")
        )
    };

    let detail = format!(
        "{retained_part}; {quarantine_part}; quarantine rows are not cleared by the clearing path"
    );

    let status = if total_retained > 0 {
        CheckStatus::Fail(detail)
    } else {
        CheckStatus::PassWithDetail(detail)
    };

    outcome(CheckName::AccumulatedDiagnosticMaterial, status)
}

/// The last `aub sample` invocation's own recorded outcome (`aub-va6s`),
/// read from `crate::store::sample_tick` rather than from `ctx.db`: the
/// marker exists precisely so a tick refused by a locked ledger still leaves
/// something this check can read without touching the ledger itself. A run
/// of refused ticks is otherwise visible only in the scheduler's own
/// journal, which nothing here reads.
fn last_sample_tick(ctx: &DoctorContext) -> CheckOutcome {
    let status = match crate::store::sample_tick::read_last_tick(&ctx.config.state.dir) {
        Ok(None) => CheckStatus::NotApplicable("no sample tick has been recorded yet".to_string()),
        Ok(Some(tick)) => match tick.outcome {
            crate::store::sample_tick::TickOutcome::Success => CheckStatus::Pass,
            crate::store::sample_tick::TickOutcome::Failed(reason) => CheckStatus::Fail(reason),
        },
        Err(error) => CheckStatus::Fail(format!("cannot read the last sample tick: {error}")),
    };
    outcome(CheckName::LastSampleTick, status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Overrides, RealEnv, resolve};

    fn test_config(state_dir: &std::path::Path) -> Config {
        let env = RealEnv;
        let toml = format!("[state]\ndir = {:?}\n", state_dir);
        let (config, _) = resolve(&Overrides::new(), &env, Some(&toml), "aub.toml")
            .expect("minimal config must resolve");
        config
    }

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let suffix = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "aub-doctor-checks-test-{tag}-{}-{suffix}",
            std::process::id()
        ))
    }

    fn empty_ctx<'a>(config: &'a Config, db_path: PathBuf) -> DoctorContext<'a> {
        DoctorContext {
            config,
            timestamp: UtcTimestamp::from_unix_nanos(1_700_000_000_000_000_000),
            db_path,
            db: None,
            db_missing: true,
            db_open_error: None,
        }
    }

    /// Every expected check is registered: the consistency test this bead's
    /// acceptance criteria require, run against a real build of the registry
    /// rather than a hand-built stand-in.
    #[test]
    fn build_registry_registers_every_expected_check() {
        let dir = scratch_dir("registry-complete");
        let config = test_config(&dir);
        let ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));
        let outcomes = build_registry(&ctx);
        assert!(super::super::missing_checks(&outcomes).is_empty());
        assert_eq!(outcomes.len(), CheckName::EXPECTED.len());
    }

    /// Planted negative: configuration failure must not silently drop every other
    /// check. Each becomes not-applicable rather than absent, and the count still
    /// matches the expected set.
    #[test]
    fn configuration_failed_registry_still_names_every_check() {
        let outcomes = configuration_failed_registry("boom");
        assert!(super::super::missing_checks(&outcomes).is_empty());
        let config_outcome = outcomes
            .iter()
            .find(|o| o.name == CheckName::ConfigurationValidity)
            .expect("configuration-validity must be present");
        assert!(matches!(config_outcome.status, CheckStatus::Fail(_)));
        let others_not_applicable = outcomes
            .iter()
            .filter(|o| o.name != CheckName::ConfigurationValidity)
            .all(|o| matches!(o.status, CheckStatus::NotApplicable(_)));
        assert!(others_not_applicable);
    }

    #[test]
    fn missing_ledger_database_is_not_applicable_not_a_failure() {
        let dir = scratch_dir("no-db");
        let config = test_config(&dir);
        let ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));
        let outcome = sqlite_and_schema_health(&ctx);
        assert_eq!(
            outcome.status,
            CheckStatus::NotApplicable(format!(
                "no ledger database exists yet at {}",
                ctx.db_path.display()
            ))
        );
    }

    /// Mutation: a database that exists but refuses to open must fail the check,
    /// not read as merely absent.
    #[test]
    fn db_open_error_is_a_failure_not_not_applicable() {
        let dir = scratch_dir("open-error");
        let config = test_config(&dir);
        let mut ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));
        ctx.db_missing = false;
        ctx.db_open_error = Some("permission denied".to_string());
        let outcome = sqlite_and_schema_health(&ctx);
        assert!(
            matches!(outcome.status, CheckStatus::Fail(ref msg) if msg.contains("permission denied"))
        );
    }

    #[test]
    fn no_accounts_configured_is_not_applicable_for_cadence_and_auth() {
        let dir = scratch_dir("no-accounts");
        let config = test_config(&dir);
        let ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));
        assert!(matches!(
            sampling_cadence(&ctx).status,
            CheckStatus::NotApplicable(_)
        ));
        assert!(matches!(
            unresolved_authentication(&ctx).status,
            CheckStatus::NotApplicable(_)
        ));
    }

    #[test]
    fn no_transcripts_configured_is_not_applicable() {
        let dir = scratch_dir("no-transcripts");
        let config = test_config(&dir);
        let ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));
        assert!(matches!(
            transcript_roots(&ctx).status,
            CheckStatus::NotApplicable(_)
        ));
    }

    /// Mutation: a configured transcript root that does not exist must fail, not
    /// pass because the source is merely configured.
    #[test]
    fn a_missing_transcript_root_fails() {
        let dir = scratch_dir("missing-root");
        let toml = format!(
            "[state]\ndir = {:?}\n\n[[transcripts]]\nname = \"missing\"\nroot = {:?}\npattern = \"**/*.jsonl\"\n",
            dir,
            dir.join("does-not-exist"),
        );
        let env = RealEnv;
        let (config, _) =
            resolve(&Overrides::new(), &env, Some(&toml), "aub.toml").expect("config must resolve");
        let ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));
        let outcome = transcript_roots(&ctx);
        assert!(matches!(outcome.status, CheckStatus::Fail(_)));
    }

    #[test]
    fn no_backup_destination_is_not_applicable() {
        let dir = scratch_dir("no-backup-dest");
        let config = test_config(&dir);
        let ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));
        assert!(matches!(
            backup_age(&ctx).status,
            CheckStatus::NotApplicable(_)
        ));
    }

    /// Mutation: a configured destination with nothing at it must fail, not read
    /// as not-applicable merely because there is no archive there yet.
    #[test]
    fn a_configured_but_missing_backup_fails() {
        let dir = scratch_dir("missing-backup");
        let destination = dir.join("archive");
        let toml = format!(
            "[state]\ndir = {:?}\n\n[backup]\ndestination = {:?}\n",
            dir, destination
        );
        let env = RealEnv;
        let (config, _) =
            resolve(&Overrides::new(), &env, Some(&toml), "aub.toml").expect("config must resolve");
        let ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));
        let outcome = backup_age(&ctx);
        assert!(matches!(outcome.status, CheckStatus::Fail(_)));
    }

    /// A migrated, empty ledger database at `dir`, ready for
    /// `crate::backup::create_archive` to read: the directory is created at
    /// the production mode first, because `open` creates the file, never its
    /// parent directory.
    fn seeded_ledger_dir(dir: &std::path::Path, clock: &impl crate::domain::time::Clock) {
        crate::store::startup::ensure_dir_mode_0700(dir).unwrap();
        let _ = crate::store::rate_card::open_ledger(
            &dir.join(crate::store::connection::LEDGER_DATABASE_FILE),
            crate::domain::time::MonotonicDuration::from_millis(500),
            clock,
        )
        .expect("a fresh ledger must open and migrate");
    }

    /// Mutation: a configured drill result with nothing recorded at it must
    /// fail even though the backup itself is genuinely healthy. Two separate
    /// numbers means a failure in either is never hidden by the other
    /// passing.
    #[test]
    fn a_configured_but_missing_drill_result_fails_even_with_a_healthy_backup() {
        let dir = scratch_dir("missing-drill");
        let now = UtcTimestamp::from_unix_nanos(1_700_000_000_000_000_000);
        let clock = crate::domain::time::FakeClock::new(now);
        seeded_ledger_dir(&dir, &clock);
        let backup_destination = dir.join("archive");
        crate::backup::create_archive(
            &dir,
            &backup_destination,
            crate::domain::time::MonotonicDuration::from_millis(500),
            &clock,
        )
        .expect("a fresh empty ledger must back up cleanly");

        let drill_result = dir.join("drill-result.jsonl");
        let toml = format!(
            "[state]\ndir = {:?}\n\n[backup]\ndestination = {:?}\n\n[drill]\nresult = {:?}\n",
            dir, backup_destination, drill_result,
        );
        let env = RealEnv;
        let (config, _) =
            resolve(&Overrides::new(), &env, Some(&toml), "aub.toml").expect("config must resolve");
        let mut ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));
        ctx.timestamp = now;
        let outcome = backup_age(&ctx);
        match &outcome.status {
            CheckStatus::Fail(message) => {
                assert!(message.contains("drill:"), "{message}");
                assert!(!message.contains("backup:"), "{message}");
            }
            CheckStatus::Pass
            | CheckStatus::PassWithDetail(_)
            | CheckStatus::NotApplicable(_)
            | CheckStatus::NotYetAvailable { .. } => {
                panic!("expected a failure naming the missing drill record")
            }
        }
    }

    /// Both ages are reported independently: a healthy backup with a stale
    /// drill fails naming the drill, and a stale backup with a healthy drill
    /// fails naming the backup. Neither passing state masks the other's
    /// finding.
    #[test]
    fn a_healthy_backup_and_a_stale_drill_fail_naming_only_the_drill() {
        let dir = scratch_dir("stale-drill");
        let now = UtcTimestamp::from_unix_nanos(1_700_000_000_000_000_000);
        let clock = crate::domain::time::FakeClock::new(now);
        seeded_ledger_dir(&dir, &clock);
        let backup_destination = dir.join("archive");
        crate::backup::create_archive(
            &dir,
            &backup_destination,
            crate::domain::time::MonotonicDuration::from_millis(500),
            &clock,
        )
        .expect("a fresh empty ledger must back up cleanly");

        let drill_result = dir.join("drill-result.jsonl");
        crate::drill::record_run(
            &drill_result,
            &crate::drill::DrillRunRecord {
                drilled_at: UtcTimestamp::from_unix_nanos(0),
                source: "archive:/dev/null".to_string(),
                scratch_destination: dir.join("drill-scratch"),
                passed: true,
            },
        )
        .expect("appending a drill record must succeed");

        let toml = format!(
            "[state]\ndir = {:?}\n\n[backup]\ndestination = {:?}\nreview_after = \"48h\"\n\n\
             [drill]\nresult = {:?}\nmax_age = \"1h\"\n",
            dir, backup_destination, drill_result,
        );
        let env = RealEnv;
        let (config, _) =
            resolve(&Overrides::new(), &env, Some(&toml), "aub.toml").expect("config must resolve");
        let mut ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));
        ctx.timestamp = now;
        let outcome = backup_age(&ctx);
        match &outcome.status {
            CheckStatus::Fail(message) => {
                assert!(message.contains("drill:"), "{message}");
                assert!(!message.contains("backup:"), "{message}");
            }
            CheckStatus::Pass
            | CheckStatus::PassWithDetail(_)
            | CheckStatus::NotApplicable(_)
            | CheckStatus::NotYetAvailable { .. } => {
                panic!("expected a failure naming only the stale drill")
            }
        }
    }

    /// The other direction: a fresh drill and a backup past its own review
    /// horizon fails naming the backup, not the drill.
    #[test]
    fn a_fresh_drill_and_a_stale_backup_fail_naming_only_the_backup() {
        let dir = scratch_dir("stale-backup");
        let seed_clock = crate::domain::time::FakeClock::new(UtcTimestamp::from_unix_nanos(0));
        seeded_ledger_dir(&dir, &seed_clock);
        let backup_destination = dir.join("archive");
        crate::backup::create_archive(
            &dir,
            &backup_destination,
            crate::domain::time::MonotonicDuration::from_millis(500),
            &seed_clock,
        )
        .expect("a fresh empty ledger must back up cleanly");

        let now = UtcTimestamp::from_unix_nanos(1_700_000_000_000_000_000);
        let drill_result = dir.join("drill-result.jsonl");
        crate::drill::record_run(
            &drill_result,
            &crate::drill::DrillRunRecord {
                drilled_at: now,
                source: "archive:/dev/null".to_string(),
                scratch_destination: dir.join("drill-scratch"),
                passed: true,
            },
        )
        .expect("appending a drill record must succeed");

        let toml = format!(
            "[state]\ndir = {:?}\n\n[backup]\ndestination = {:?}\nreview_after = \"1h\"\n\n\
             [drill]\nresult = {:?}\nmax_age = \"48h\"\n",
            dir, backup_destination, drill_result,
        );
        let env = RealEnv;
        let (config, _) =
            resolve(&Overrides::new(), &env, Some(&toml), "aub.toml").expect("config must resolve");
        let mut ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));
        ctx.timestamp = now;
        let outcome = backup_age(&ctx);
        match &outcome.status {
            CheckStatus::Fail(message) => {
                assert!(message.contains("backup:"), "{message}");
                assert!(!message.contains("drill:"), "{message}");
            }
            CheckStatus::Pass
            | CheckStatus::PassWithDetail(_)
            | CheckStatus::NotApplicable(_)
            | CheckStatus::NotYetAvailable { .. } => {
                panic!("expected a failure naming only the stale backup")
            }
        }
    }

    /// Both ages within their horizon: the whole check passes, exactly as it
    /// would with only backup configured.
    #[test]
    fn a_healthy_backup_and_a_fresh_drill_both_pass() {
        let dir = scratch_dir("both-healthy");
        let now = UtcTimestamp::from_unix_nanos(1_700_000_000_000_000_000);
        let clock = crate::domain::time::FakeClock::new(now);
        seeded_ledger_dir(&dir, &clock);
        let backup_destination = dir.join("archive");
        crate::backup::create_archive(
            &dir,
            &backup_destination,
            crate::domain::time::MonotonicDuration::from_millis(500),
            &clock,
        )
        .expect("a fresh empty ledger must back up cleanly");

        let drill_result = dir.join("drill-result.jsonl");
        crate::drill::record_run(
            &drill_result,
            &crate::drill::DrillRunRecord {
                drilled_at: now,
                source: "archive:/dev/null".to_string(),
                scratch_destination: dir.join("drill-scratch"),
                passed: true,
            },
        )
        .expect("appending a drill record must succeed");

        let toml = format!(
            "[state]\ndir = {:?}\n\n[backup]\ndestination = {:?}\n\n\
             [drill]\nresult = {:?}\n",
            dir, backup_destination, drill_result,
        );
        let env = RealEnv;
        let (config, _) =
            resolve(&Overrides::new(), &env, Some(&toml), "aub.toml").expect("config must resolve");
        let mut ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));
        ctx.timestamp = now;
        let outcome = backup_age(&ctx);
        assert_eq!(outcome.status, CheckStatus::Pass);
    }

    #[test]
    fn missing_projection_is_not_applicable() {
        let dir = scratch_dir("no-projection");
        std::fs::create_dir_all(&dir).expect("scratch dir must be creatable");
        let config = test_config(&dir);
        let mut ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));
        // Absent-database and absent-projection are independent conditions: this
        // proves the projection path alone, so it needs a connection-shaped
        // context that is not itself reporting db_missing.
        ctx.db_missing = false;
        let conn = crate::store::rate_card::open_ledger(
            &ctx.db_path,
            crate::domain::time::MonotonicDuration::from_millis(500),
            &crate::domain::time::RealClock::new(),
        )
        .expect("a fresh ledger must open and migrate");
        ctx.db = Some(&conn);
        let outcome = projection_versus_database_generation(&ctx);
        assert!(matches!(outcome.status, CheckStatus::NotApplicable(_)));
    }

    fn test_config_with_max_comparison_age(state_dir: &std::path::Path, max_age: &str) -> Config {
        let env = RealEnv;
        let toml = format!(
            "[state]\ndir = {:?}\n\n[adapter_semantics]\nmax_comparison_age = {:?}\n",
            state_dir, max_age
        );
        let (config, _) =
            resolve(&Overrides::new(), &env, Some(&toml), "aub.toml").expect("config must resolve");
        config
    }

    /// A migrated ledger with one observation and one account-wide `five_hour`
    /// window, ready for a comparison to be inserted directly against it.
    /// Mirrors the fixture in `store::adapter_semantics_validation`'s own
    /// tests; duplicated here rather than shared because each test module
    /// keeps its own fixture in this codebase.
    fn seeded_observation_with_window(
        conn: &rusqlite::Connection,
    ) -> (
        crate::store::meter_evidence::ObservationRowId,
        crate::store::meter_evidence::WindowRowId,
    ) {
        use crate::domain::ids::{AdapterVersion, MeterSemanticsId, ProviderContractId};
        use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
        use crate::domain::time::{MeasurementBasis, MonotonicDuration};
        use crate::domain::window::{
            NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
            WindowSemanticKey,
        };
        use crate::store::account::observe_account;
        use crate::store::meter_attempt::{DueReason, NewMeterAttempt, start_meter_attempt};
        use crate::store::meter_evidence::{
            NewMeterObservation, NewMeterResponseEvidence, NewMeterWindow, insert_observation,
            insert_response_evidence, insert_window,
        };
        use crate::store::sample_run::{Trigger, start_sample_run};
        use crate::store::sampling_policy_snapshot::{
            ResolvedSamplingPolicy, resolve_policy_snapshot,
        };

        const POLICY: ResolvedSamplingPolicy = ResolvedSamplingPolicy {
            ordinary_cadence: MonotonicDuration::from_millis(300_000),
            freshness_horizon: MonotonicDuration::from_millis(900_000),
            reset_edge_policy: String::new(),
            retry_backoff_policy: String::new(),
            command_budget: MonotonicDuration::from_millis(60_000),
            policy_algorithm_version: String::new(),
        };

        let account = observe_account(
            conn,
            "anthropic",
            "primary",
            UtcTimestamp::from_unix_nanos(10),
        )
        .expect("account must insert");
        let run = start_sample_run(
            conn,
            Trigger::Manual,
            UtcTimestamp::from_unix_nanos(10),
            "seed",
        )
        .expect("sample run must insert");
        let snapshot =
            resolve_policy_snapshot(conn, account, UtcTimestamp::from_unix_nanos(10), &POLICY)
                .expect("policy snapshot must insert");
        let attempt = start_meter_attempt(
            conn,
            &NewMeterAttempt {
                run_id: run,
                account_id: account,
                provider: "anthropic".into(),
                request_started_at: UtcTimestamp::from_unix_nanos(20),
                credential_context_id: Some("ctx".into()),
                policy_snapshot_id: snapshot,
                due_at: UtcTimestamp::from_unix_nanos(19),
                due_reason: DueReason::OrdinaryCadence,
                due_basis: None,
                provider_contract_id: "endpoint-schema-v3".into(),
                meter_semantics_id: "account-5h-v2".into(),
            },
        )
        .expect("attempt must insert");
        let evidence_id = insert_response_evidence(
            conn,
            &NewMeterResponseEvidence {
                attempt_id: attempt,
                response_classification: "200".into(),
                received_at: UtcTimestamp::from_unix_nanos(30),
                provider_observed_at_original: None,
                evidence_capsule: r#"{"windows":[]}"#.into(),
                capsule_schema_version: "capsule-v1".into(),
                sanitizer_version: "sanitizer-v1".into(),
                capture_truncated: false,
            },
        )
        .expect("evidence must insert");
        let observation_id = insert_observation(
            conn,
            &NewMeterObservation {
                attempt_id: attempt,
                evidence_id,
                account_id: account,
                provider: "anthropic".into(),
                provider_observed_at: None,
                received_at: UtcTimestamp::from_unix_nanos(31),
                measurement_basis: MeasurementBasis::LocallyReceived,
                observed_plan: Some("max".into()),
                observed_tier: None,
                adapter_version: AdapterVersion::new("adapter-v1"),
                provider_contract_id: ProviderContractId::new("endpoint-schema-v3"),
                meter_semantics_id: MeterSemanticsId::new("account-5h-v2"),
                normalized_fingerprint: "fp-1".into(),
            },
        )
        .expect("observation must insert");
        let window_id = insert_window(
            conn,
            &NewMeterWindow {
                observation_id,
                semantic_key: WindowSemanticKey::new("five_hour"),
                scope: WindowScope::AccountWide,
                quota_used: QuotaUsed::new(QuotaFractionPpm::new(250_000).unwrap()),
                reported_resolution: ReportedResolution::new(
                    QuotaFractionPpm::new(10_000).unwrap(),
                )
                .unwrap(),
                quantization: QuantizationSemantics::RoundedToNearest,
                resets_at: UtcTimestamp::from_unix_nanos(100_000),
                nominal_duration: NominalWindowDuration::from_nanos(18_000_000_000_000),
            },
        )
        .expect("window must insert");
        (observation_id, window_id)
    }

    fn record_test_comparison(
        conn: &rusqlite::Connection,
        observation_id: crate::store::meter_evidence::ObservationRowId,
        window_id: crate::store::meter_evidence::WindowRowId,
        read_at: UtcTimestamp,
    ) {
        use crate::domain::authoritative_comparison::{
            AuthoritativeComparisonVerdict, DocumentedGranularity,
        };
        use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
        use crate::domain::window::WindowSemanticKey;
        use crate::store::adapter_semantics_validation::{
            NewAuthoritativeSurfaceComparison, insert_comparison,
        };

        insert_comparison(
            conn,
            &NewAuthoritativeSurfaceComparison {
                observation_id,
                window_id,
                semantic_key: WindowSemanticKey::new("five_hour"),
                authoritative_surface: "test surface".into(),
                documented_granularity: DocumentedGranularity::new(
                    QuotaFractionPpm::new(10_000).unwrap(),
                ),
                adapter_quota_used: QuotaUsed::new(QuotaFractionPpm::new(250_000).unwrap()),
                authoritative_quota_used: QuotaUsed::new(QuotaFractionPpm::new(250_000).unwrap()),
                read_at,
                verdict: AuthoritativeComparisonVerdict::AgreesWithinGranularity,
            },
        )
        .expect("comparison must insert");
    }

    #[test]
    fn no_adapter_semantics_comparison_is_not_applicable() {
        let dir = scratch_dir("no-comparison");
        std::fs::create_dir_all(&dir).expect("scratch dir must be creatable");
        let config = test_config(&dir);
        let mut ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));
        ctx.db_missing = false;
        let conn = crate::store::rate_card::open_ledger(
            &ctx.db_path,
            crate::domain::time::MonotonicDuration::from_millis(500),
            &crate::domain::time::RealClock::new(),
        )
        .expect("a fresh ledger must open and migrate");
        ctx.db = Some(&conn);
        let outcome = adapter_semantics_comparison_age(&ctx);
        assert!(matches!(outcome.status, CheckStatus::NotApplicable(_)));
    }

    /// A comparison recorded well within the configured horizon passes and
    /// states its age.
    #[test]
    fn a_recent_comparison_passes_with_its_age() {
        let dir = scratch_dir("recent-comparison");
        std::fs::create_dir_all(&dir).expect("scratch dir must be creatable");
        let config = test_config_with_max_comparison_age(&dir, "30d");
        let mut ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));
        ctx.db_missing = false;
        ctx.timestamp = UtcTimestamp::from_unix_nanos(1_700_000_000_000_000_000);
        let conn = crate::store::rate_card::open_ledger(
            &ctx.db_path,
            crate::domain::time::MonotonicDuration::from_millis(500),
            &crate::domain::time::RealClock::new(),
        )
        .expect("a fresh ledger must open and migrate");
        let (observation_id, window_id) = seeded_observation_with_window(&conn);
        // One hour before "now": comfortably inside a 30-day horizon.
        let read_at =
            UtcTimestamp::from_unix_nanos(ctx.timestamp.unix_nanos() - 3600 * 1_000_000_000);
        record_test_comparison(&conn, observation_id, window_id, read_at);
        ctx.db = Some(&conn);
        let outcome = adapter_semantics_comparison_age(&ctx);
        match &outcome.status {
            CheckStatus::PassWithDetail(detail) => {
                assert!(detail.contains("3600s old"), "{detail}");
            }
            CheckStatus::Pass
            | CheckStatus::Fail(_)
            | CheckStatus::NotApplicable(_)
            | CheckStatus::NotYetAvailable { .. } => {
                panic!("expected PassWithDetail, got {:?}", outcome.status)
            }
        }
    }

    /// Mutation: the same comparison, judged against a thirty-minute horizon
    /// instead of thirty days, must fail rather than keep passing. Proves the
    /// threshold is read from configuration rather than hard-coded.
    #[test]
    fn a_stale_comparison_fails_past_the_configured_threshold() {
        let dir = scratch_dir("stale-comparison");
        std::fs::create_dir_all(&dir).expect("scratch dir must be creatable");
        let config = test_config_with_max_comparison_age(&dir, "30m");
        let mut ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));
        ctx.db_missing = false;
        ctx.timestamp = UtcTimestamp::from_unix_nanos(1_700_000_000_000_000_000);
        let conn = crate::store::rate_card::open_ledger(
            &ctx.db_path,
            crate::domain::time::MonotonicDuration::from_millis(500),
            &crate::domain::time::RealClock::new(),
        )
        .expect("a fresh ledger must open and migrate");
        let (observation_id, window_id) = seeded_observation_with_window(&conn);
        // One hour before "now": past a thirty-minute horizon.
        let read_at =
            UtcTimestamp::from_unix_nanos(ctx.timestamp.unix_nanos() - 3600 * 1_000_000_000);
        record_test_comparison(&conn, observation_id, window_id, read_at);
        ctx.db = Some(&conn);
        let outcome = adapter_semantics_comparison_age(&ctx);
        assert!(
            matches!(outcome.status, CheckStatus::Fail(ref msg) if msg.contains("review horizon")),
            "{:?}",
            outcome.status
        );
    }
}
