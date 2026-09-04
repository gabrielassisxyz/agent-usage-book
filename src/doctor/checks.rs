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
        CheckOutcome {
            name: CheckName::UnexplainedResidual,
            owner_module: "valuation",
            condition: "rolling residual stays within its explained bound",
            has_repair: false,
            status: CheckStatus::NotYetAvailable {
                owning_bead: "aub-dpn.3",
            },
        },
        heuristic_dedup_counts(ctx),
        clock_skew(ctx),
        local_filesystem_and_wal_suitability(ctx),
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
        CheckName::UnexplainedResidual => "valuation",
        CheckName::HeuristicDedupCounts => "store::ingest_quarantine",
        CheckName::ClockSkew => "doctor",
        CheckName::LocalFilesystemAndWalSuitability => "store::startup",
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
        CheckName::BackupAge => "the last verified backup is within its configured review horizon",
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

/// The age of the last verified backup, when `backup.destination` is configured.
/// `store::backup::backup_health`'s own doc comment names this bead as the later
/// doctor registry it was built to feed.
fn backup_age(ctx: &DoctorContext) -> CheckOutcome {
    let status = match &ctx.config.backup.destination {
        None => CheckStatus::NotApplicable(
            "backup.destination is not configured; aub backup takes its destination \
             explicitly and none is remembered without it"
                .to_string(),
        ),
        Some(destination) => {
            match crate::backup::backup_health(
                destination,
                ctx.timestamp,
                ctx.config.backup.review_after,
            ) {
                Err(error) => CheckStatus::Fail(format!("cannot read the backup: {error}")),
                Ok(crate::backup::BackupHealth::Missing) => {
                    CheckStatus::Fail(format!("no backup found at {}", destination.display()))
                }
                Ok(crate::backup::BackupHealth::Unverified { .. }) => CheckStatus::Fail(format!(
                    "a backup exists at {} but has not been verified",
                    destination.display()
                )),
                Ok(crate::backup::BackupHealth::Verified {
                    age,
                    review_due: true,
                    ..
                }) => CheckStatus::Fail(format!(
                    "the last verified backup is {}s old, past its review horizon",
                    age.as_nanos() / 1_000_000_000
                )),
                Ok(crate::backup::BackupHealth::Verified {
                    review_due: false, ..
                }) => CheckStatus::Pass,
            }
        }
    };
    outcome(CheckName::BackupAge, status)
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
}
