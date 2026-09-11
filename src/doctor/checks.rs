//! The registered checks: the seven this bead owns (sampling cadence, unresolved
//! authentication, transcript roots, backup age, projection versus database
//! generation, clock skew, missing active calibrations), and the seventeen whose
//! evidence belongs elsewhere but whose subsystem already exists and is read here
//! (including `MeterAnomalies`, `aub-eun.14`'s own read of `store::window_anomaly`,
//! and `UnmappedAccounts`, `aub-mgv.3`'s own read of
//! `store::account_attribution_segment` through `attribution::quality`).
//!
//! Every check is read-only: `doctor` performs no network operation and no check
//! here writes to the ledger. [`super::fix`] is the only writer, and only under
//! `--fix`.
//!
//! A check's reason names logical identifiers (account names, transcript source
//! names, counts) and never an absolute path, a credential value, or transcript
//! content: the state directory and the credential paths live under the
//! operator's home, so printing them would print the home (PLAN.md 37).

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
        unmapped_accounts(ctx),
        missing_active_calibrations(ctx),
        stale_rate_cards(ctx),
        projection_versus_database_generation(ctx),
        backup_age(ctx),
        meter_anomalies(ctx),
        unexplained_residual(ctx),
        heuristic_dedup_counts(ctx),
        clock_skew(ctx),
        local_filesystem_and_wal_suitability(ctx),
        accumulated_diagnostic_material(ctx),
        adapter_semantics_comparison_age(ctx),
        last_sample_tick(ctx),
        sampling_failure_counts(ctx),
        meter_error_classifications(ctx),
        subscription_identity_change(ctx),
        cost_model_active(ctx),
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
        CheckName::MeterAnomalies => "store::window_anomaly",
        CheckName::UnexplainedResidual => "reconciliation",
        CheckName::HeuristicDedupCounts => "store::ingest_quarantine",
        CheckName::ClockSkew => "doctor",
        CheckName::LocalFilesystemAndWalSuitability => "store::startup",
        CheckName::AccumulatedDiagnosticMaterial => "store::retention",
        CheckName::AdapterSemanticsComparisonAge => "store::adapter_semantics_validation",
        CheckName::LastSampleTick => "store::sample_tick",
        CheckName::SamplingFailureCounts => "store::sampling_failure_counts",
        CheckName::MeterErrorClassifications => "store::meter_attempt",
        CheckName::SubscriptionIdentityChange => "store::subscription_identity",
        CheckName::CostModelActive => "store::cost_model",
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
        CheckName::UnmappedAccounts => "no canonical usage sits in the unknown-account bucket",
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
        CheckName::MeterAnomalies => {
            "no meter window anomaly was recorded inside the configured recent horizon"
        }
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
        CheckName::SamplingFailureCounts => {
            "no persist-failed or due-lookup-failed sampler disposition has ever been recorded"
        }
        CheckName::MeterErrorClassifications => {
            "every failed attempt in the recent window carries the provider error              classification the sampler stored for it"
        }
        CheckName::SubscriptionIdentityChange => {
            "no account's credential changed subscription without being refused and recorded"
        }
        CheckName::CostModelActive => {
            "a published cost model is active whenever rate cards are imported"
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
/// fifth permitted action with no check of its own in the twenty-four-item list, so
/// no [`CheckName`] variant claims it; sampling cadence stays `false` because
/// `--fix` performs no network operation and cannot make an account get sampled;
/// [`CheckName::UnmappedAccounts`] stays `false` because `--fix` must not
/// reattribute ambiguous sessions (PLAN.md 36).
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
        CheckStatus::NotApplicable("no ledger database exists yet".to_string())
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
        CheckStatus::NotApplicable("no ledger database exists yet".to_string())
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
                CheckStatus::Fail(format!("{count} pending record(s) undrained"))
            }
        }
    };
    outcome(CheckName::PendingEvidence, status)
}

/// A recent-sampling-attempt window, three times the configured default
/// interval: shared by [`sampling_cadence`], which asks whether an attempt
/// happened recently at all, and [`env_credential_resolves_for_sampler`],
/// which asks whether a *successful* one did. One definition, so the two
/// checks cannot disagree about what "recent" means without the disagreement
/// being visible as a diff to this function.
fn recent_attempt_threshold_nanos(ctx: &DoctorContext) -> u64 {
    ctx.config
        .sampling
        .default_interval
        .as_nanos()
        .saturating_mul(3)
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
                let threshold_nanos = recent_attempt_threshold_nanos(ctx);
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
///
/// An `env`-kind credential (aub-e2uz) is unresolvable from any process that
/// does not carry the variable, which includes every interactive shell that
/// is not `aub-sample.service` itself: the same binary, the same ledger, one
/// minute apart, disagreed about an account that was sampling successfully
/// throughout (aub-a0tj). Three shapes were weighed for what this check
/// should say about a credential it cannot see but the sampler can:
///
/// - degrade straight to a warning naming the invocation as the likely
///   cause. Rejected: it is the cheapest option and the weakest one, because
///   it stops being a red flag for the one case that matters, a credential
///   that really is missing everywhere, unless something else notices first.
/// - report the check as not-applicable for `env`-kind credentials outside
///   the unit's own environment. Rejected: honest about the process's own
///   blind spot, but it silently drops real coverage exactly when the
///   variable is genuinely unset for the sampler too.
/// - keep the failure, but only when the sampler is *also* failing to
///   resolve the credential, which the ledger can answer from recent
///   sampling attempts. **Chosen.** It is the only shape that stays truthful
///   in both directions: a credential the sampler resolves reads as resolved
///   everywhere, and a credential nothing can resolve still fails loudly
///   (`env_credential_resolves_for_sampler`'s planted-negative test).
///
/// The recovered case reports [`CheckStatus::PassWithDetail`] rather than a
/// bare [`CheckStatus::Pass`], naming the invocation context in its message:
/// an operator who runs `doctor` from a shell that disagrees with the unit
/// needs to see why, not just that the answer came out the same.
fn unresolved_authentication(ctx: &DoctorContext) -> CheckOutcome {
    let status = if ctx.config.accounts.is_empty() {
        CheckStatus::NotApplicable("no accounts configured".to_string())
    } else {
        let mut unresolved = Vec::new();
        let mut recovered = Vec::new();
        for account in &ctx.config.accounts {
            if let Err(error) = crate::auth::resolve(account, &crate::auth::RealFs, false) {
                match env_credential_resolves_for_sampler(ctx, account) {
                    Some(evidence) => recovered.push(format!(
                        "{}: not set in this process; resolves for the sampler ({evidence})",
                        account.name
                    )),
                    None => unresolved.push(format!("{}: {error}", account.name)),
                }
            }
        }
        if !unresolved.is_empty() {
            CheckStatus::Fail(unresolved.join("; "))
        } else if !recovered.is_empty() {
            CheckStatus::PassWithDetail(recovered.join("; "))
        } else {
            CheckStatus::Pass
        }
    };
    outcome(CheckName::UnresolvedAuthentication, status)
}

/// Whether an `env`-kind credential this process cannot see is nonetheless
/// resolving for the sampler, read from recent attempt history rather than
/// re-derived: an unattended unit and an interactive shell disagree about
/// which environment they carry, never about which account is or is not
/// authenticated, so the ledger's own record of what actually happened is
/// the one source that cannot be invocation-dependent.
///
/// Returns `None` for a `file` or `none`-kind credential (unaffected by this
/// bead, `aub-a0tj`), for an account this process has never observed, for an
/// account with no successful attempt at all, and for a success old enough
/// that it can no longer speak for the credential's current state, using the
/// same recency window [`sampling_cadence`] uses for "recent" everywhere else
/// in this module. Any of those leaves the credential reported as failed,
/// because a `None` here is what keeps a credential missing for the sampler
/// too failing loudly rather than being read as merely unseen.
fn env_credential_resolves_for_sampler(
    ctx: &DoctorContext,
    account: &crate::config::AccountConfig,
) -> Option<String> {
    if !matches!(
        crate::auth::CredentialSource::from_account(account),
        Ok(crate::auth::CredentialSource::Env { .. })
    ) {
        return None;
    }
    let conn = ctx.db?;
    let account_id =
        crate::store::account::account_id_by_identity(conn, &account.provider, &account.name)
            .ok()??;
    let attempt =
        crate::store::meter_attempt::newest_successful_attempt_for_account(conn, account_id)
            .ok()??;
    let gap_nanos = ctx
        .timestamp
        .unix_nanos()
        .saturating_sub(attempt.request_started_at.unix_nanos()) as u64;
    if gap_nanos > recent_attempt_threshold_nanos(ctx) {
        return None;
    }
    Some(format!(
        "last successful sampler attempt {}s ago",
        gap_nanos / 1_000_000_000
    ))
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
            .map(|source| source.name.clone())
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

/// Canonical usage that landed in the unknown-account bucket (`aub-mgv.3`):
/// usage before any account marker exists has no marker to justify an account
/// assignment, so the segmentation records it as unattributed rather than
/// guessing. Reads the persisted segments through `attribution::quality`'s own
/// metric, per token kind, so the two cannot disagree about what "unknown"
/// means without the disagreement being visible as a diff to this function.
///
/// A ledger with no segments at all is [`CheckStatus::NotApplicable`] rather
/// than a pass: nothing has been attributed yet, so there is no coverage to
/// judge. Any nonzero unknown total fails naming the per-kind counts; the
/// reason carries counts only, never session ids, paths, or credentials.
fn unmapped_accounts(ctx: &DoctorContext) -> CheckOutcome {
    let status = if ctx.db_missing {
        CheckStatus::NotApplicable("no ledger database exists yet".to_string())
    } else if let Some(error) = &ctx.db_open_error {
        CheckStatus::Fail(format!("cannot open the ledger database: {error}"))
    } else {
        match ctx.db {
            None => CheckStatus::Fail("no open connection to the ledger database".to_string()),
            Some(conn) => {
                match crate::store::account_attribution_segment::attribution_observations(conn) {
                    Err(error) => {
                        CheckStatus::Fail(format!("cannot read attribution segments: {error}"))
                    }
                    Ok(observations) if observations.is_empty() => CheckStatus::NotApplicable(
                        "no attribution segments have been recorded yet".to_string(),
                    ),
                    Ok(observations) => {
                        let quality =
                            crate::attribution::quality::AttributionQuality::over(observations);
                        let mut unattributed = Vec::new();
                        for kind in crate::domain::tokens::TokenKind::ALL {
                            let breakdown = quality.breakdown(kind);
                            if breakdown.unknown_account() > 0 {
                                unattributed.push(format!(
                                    "{}: {} unattributed of {} total",
                                    kind.label(),
                                    breakdown.unknown_account(),
                                    breakdown.total(),
                                ));
                            }
                        }
                        if unattributed.is_empty() {
                            CheckStatus::Pass
                        } else {
                            CheckStatus::Fail(format!(
                                "unknown-account usage: {}",
                                unattributed.join(", ")
                            ))
                        }
                    }
                }
            }
        }
    };
    outcome(CheckName::UnmappedAccounts, status)
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
/// configured. The destination is a root holding dated archives; the age
/// comes from the newest-verified pointer inside it, read by
/// `crate::backup::backup_health`, never from file mtime.
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
                SubcheckVerdict::Failed("no verified backup found".to_string())
            }
            Ok(crate::backup::BackupHealth::Unverified { .. }) => {
                SubcheckVerdict::Failed("a backup exists but has not been verified".to_string())
            }
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
                Ok(crate::drill::DrillHealth::Missing) => {
                    SubcheckVerdict::Failed("no successful drill recorded".to_string())
                }
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

/// How many anomaly references the check prints. The count is always exact; only
/// the sample is bounded, because a check that prints hundreds of records on one
/// line is read as broken and then ignored, which is how a real anomaly hides
/// among the noise of an over-eager detector.
const METER_ANOMALY_SAMPLE_LIMIT: usize = 5;

fn meter_anomaly_detail(anomalies: &[crate::store::window_anomaly::StoredWindowAnomaly]) -> String {
    let references: Vec<String> = anomalies
        .iter()
        .take(METER_ANOMALY_SAMPLE_LIMIT)
        .map(|anomaly| {
            format!(
                "id={} kind={} account={} prior_observation={} current_observation={}",
                anomaly.row_id.value(),
                anomaly.kind.as_str(),
                anomaly.account_id.value(),
                anomaly.prior_observation_id.value(),
                anomaly.current_observation_id.value(),
            )
        })
        .collect();
    format!(
        "{} window anomaly(ies), showing {}: {}",
        anomalies.len(),
        references.len(),
        references.join("; ")
    )
}

/// Recent window anomalies (`store::window_anomaly::all_anomalies`, `aub-eun.14`).
/// This reads the persisted count and evidence references only: it never re-runs
/// consecutive-window comparison. Retained anomalies outside the configured horizon
/// remain visible as history but do not keep a recovered meter permanently failed.
fn meter_anomalies(ctx: &DoctorContext) -> CheckOutcome {
    let status = if ctx.db_missing {
        CheckStatus::NotApplicable("no ledger database exists yet".to_string())
    } else if let Some(error) = &ctx.db_open_error {
        CheckStatus::Fail(format!("cannot open the ledger database: {error}"))
    } else {
        match ctx.db {
            None => CheckStatus::Fail("no open connection to the ledger database".to_string()),
            Some(conn) => match crate::store::window_anomaly::all_anomalies(conn) {
                Err(error) => CheckStatus::Fail(format!("cannot read window anomalies: {error}")),
                Ok(anomalies) if anomalies.is_empty() => {
                    CheckStatus::PassWithDetail("0 window anomalies recorded".to_string())
                }
                Ok(anomalies) => {
                    let horizon_nanos = ctx.config.doctor.meter_anomaly_horizon.as_nanos();
                    let recent_anomalies: Vec<_> = anomalies
                        .iter()
                        .filter(|anomaly| {
                            ctx.timestamp
                                .unix_nanos()
                                .saturating_sub(anomaly.detected_at.unix_nanos())
                                .max(0) as u64
                                <= horizon_nanos
                        })
                        .cloned()
                        .collect();

                    if recent_anomalies.is_empty() {
                        CheckStatus::PassWithDetail(format!(
                            "{} historical window anomaly(ies) recorded; none within the {}s horizon",
                            anomalies.len(),
                            horizon_nanos / 1_000_000_000,
                        ))
                    } else {
                        CheckStatus::Fail(format!(
                            "{} recent of {} total window anomaly(ies) within the {}s horizon; {}",
                            recent_anomalies.len(),
                            anomalies.len(),
                            horizon_nanos / 1_000_000_000,
                            meter_anomaly_detail(&recent_anomalies),
                        ))
                    }
                }
            },
        }
    };
    outcome(CheckName::MeterAnomalies, status)
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
            Some(_) => rolling_residual_status(rolling_residual_health(ctx).as_ref()),
        }
    };
    outcome(CheckName::UnexplainedResidual, status)
}

/// The verdict half of [`unexplained_residual`]: a computed [`RollingResidualHealth`]
/// to the status the check reports for it, with no store access. Split out so the
/// discrepancy failure is provable against controlled in-memory evidence without
/// seeding a full calibration, cost-model and usage chain through the ledger.
pub fn rolling_residual_status(
    health: Option<&crate::reconciliation::RollingResidualHealth>,
) -> CheckStatus {
    match health {
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
    }
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

/// Every persist-failed and due-lookup-failed sampler disposition ever
/// recorded, by reason (`aub-b0w6`), read from
/// `crate::store::sampling_failure_counts` rather than the ledger or the
/// scheduler's journal. Cumulative and never self-clearing, the same shape as
/// [`accumulated_diagnostic_material`]'s retained rows: a nonzero total is a
/// recurrence worth a human's attention, so it stays a `Fail` until whoever
/// reads it acts on it, not until the next tick happens to succeed.
fn sampling_failure_counts(ctx: &DoctorContext) -> CheckOutcome {
    let status = match crate::store::sampling_failure_counts::read_sampling_failure_counts(
        &ctx.config.state.dir,
    ) {
        Ok(counts) if counts.is_empty() => CheckStatus::Pass,
        Ok(mut counts) => {
            counts.sort_by(|a, b| (&a.category, &a.reason).cmp(&(&b.category, &b.reason)));
            let detail = counts
                .iter()
                .map(|c| format!("{}: {} (count={})", c.category, c.reason, c.count))
                .collect::<Vec<_>>()
                .join(", ");
            CheckStatus::Fail(detail)
        }
        Err(error) => {
            CheckStatus::Fail(format!("cannot read the sampling failure counts: {error}"))
        }
    };
    outcome(CheckName::SamplingFailureCounts, status)
}

/// The window every provider error classification is listed over: one day of
/// attempts, the span an operator is asking "why did these fail" about
/// (aub-rfot's own context query).
const ERROR_CLASSIFICATION_LOOKBACK_NANOS: i64 = 24 * 60 * 60 * 1_000_000_000;

/// The provider error classifications the window's failed attempts stored,
/// per account (`aub-rfot`). A listing, never a failure: a rate limit or a
/// rejected credential is normal operation, and what this check exists to
/// say is which classification dominated, so the ledger answers "why did a
/// day of attempts fail" without a hand query. Rows written before the
/// column was populated read as `unclassified`, past and present alike.
fn meter_error_classifications(ctx: &DoctorContext) -> CheckOutcome {
    let status = if ctx.db_missing {
        CheckStatus::NotApplicable("no ledger database exists yet".to_string())
    } else if let Some(error) = &ctx.db_open_error {
        CheckStatus::Fail(format!("cannot open the ledger database: {error}"))
    } else {
        match ctx.db {
            None => CheckStatus::Fail("no open connection to the ledger database".to_string()),
            Some(conn) => {
                let start = UtcTimestamp::from_unix_nanos(
                    ctx.timestamp
                        .unix_nanos()
                        .saturating_sub(ERROR_CLASSIFICATION_LOOKBACK_NANOS),
                );
                match crate::store::meter_attempt::error_classifications_between(
                    conn,
                    start,
                    ctx.timestamp,
                ) {
                    Err(error) => CheckStatus::Fail(format!(
                        "cannot read the meter error classifications: {error}"
                    )),
                    Ok(rows) if rows.is_empty() => CheckStatus::Pass,
                    Ok(rows) => {
                        let accounts = crate::store::account::all_accounts(conn)
                            .map(|accounts| {
                                accounts
                                    .into_iter()
                                    .map(|account| {
                                        (account.id(), account.logical_name().to_string())
                                    })
                                    .collect::<std::collections::BTreeMap<_, _>>()
                            })
                            .unwrap_or_default();
                        let detail = rows
                            .iter()
                            .map(|row| {
                                let name = accounts
                                    .get(&row.account_id)
                                    .map(String::as_str)
                                    .unwrap_or("<deleted account>");
                                let parts = row
                                    .classifications
                                    .iter()
                                    .map(|(stored, count)| {
                                        format!(
                                            "{} (count={count})",
                                            crate::store::meter_attempt::
                                                error_classification_column::classification_of(
                                                    stored
                                                )
                                        )
                                    })
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                format!("{name}: {parts}")
                            })
                            .collect::<Vec<_>>()
                            .join("; ");
                        CheckStatus::PassWithDetail(format!(
                            "provider error classifications in the last 24h: {detail}"
                        ))
                    }
                }
            }
        }
    };
    outcome(CheckName::MeterErrorClassifications, status)
}

/// The subscription behind a credential path changed (aub-iwkg): the sampler
/// refused the intruding readings and recorded the identity pair in
/// `meter_subscription_change`. Fails while a configured account's newest
/// history row is a `changed` one with no newer stored observation behind
/// the established identity: that account's readings are being refused
/// right now. Passes with a historical note once an observation newer than
/// the change exists (readings under the established identity resumed),
/// and passes quietly when no account ever recorded a change.
///
/// Recovery is an operator rename to a fresh logical name for the new
/// subscription (docs/subscription-identity-change.md); no `--fix` repair
/// can acknowledge one subscription in place of another, so `has_repair`
/// stays false for this check.
fn subscription_identity_change(ctx: &DoctorContext) -> CheckOutcome {
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
                let mut refusing = Vec::new();
                let mut resumed = Vec::new();
                let mut unreadable: Option<String> = None;
                for account in &ctx.config.accounts {
                    let id = match crate::store::account::account_id_by_identity(
                        conn,
                        &account.provider,
                        &account.name,
                    ) {
                        Ok(id) => id,
                        Err(error) => {
                            unreadable = Some(error.to_string());
                            break;
                        }
                    };
                    let Some(id) = id else {
                        continue;
                    };
                    let latest =
                        match crate::store::subscription_identity::latest_for_account(conn, id) {
                            Ok(latest) => latest,
                            Err(error) => {
                                unreadable = Some(error.to_string());
                                break;
                            }
                        };
                    let Some(event) = latest else {
                        continue;
                    };
                    if event.kind
                        != crate::store::subscription_identity::SubscriptionChangeKind::Changed
                    {
                        continue;
                    }
                    let previous = event.previous_identity.as_deref().unwrap_or("<unknown>");
                    let resumed_after =
                        match crate::store::meter_evidence::newest_observation_for_account(conn, id)
                        {
                            Ok(Some(observation)) => {
                                observation.received_at.unix_nanos()
                                    > event.detected_at.unix_nanos()
                            }
                            Ok(None) | Err(_) => false,
                        };
                    if resumed_after {
                        resumed.push(format!(
                            "{}: subscription changed from '{}' to '{}' (change id={}), readings under the established subscription resumed after it",
                            account.name,
                            previous,
                            event.current_identity,
                            event.row_id.value(),
                        ));
                    } else {
                        refusing.push(format!(
                            "{}: subscription changed from '{}' to '{}' (change id={}); readings refused, see docs/subscription-identity-change.md",
                            account.name,
                            previous,
                            event.current_identity,
                            event.row_id.value(),
                        ));
                    }
                }
                if let Some(error) = unreadable {
                    CheckStatus::Fail(format!("cannot read the subscription history: {error}"))
                } else if !refusing.is_empty() {
                    CheckStatus::Fail(refusing.join("; "))
                } else if !resumed.is_empty() {
                    CheckStatus::PassWithDetail(resumed.join("; "))
                } else {
                    CheckStatus::Pass
                }
            }
        }
    };
    outcome(CheckName::SubscriptionIdentityChange, status)
}

/// A published cost model active against the ledger, whenever rate cards are
/// imported at all: credits pricing resolves through the active model, so with
/// prices present but none active `spend --credits` refuses and the doctor
/// names the one command that repairs it. Advisory, never gating: the token
/// ledger is complete without credits pricing, so the finding warns rather than
/// fails, and `--fix` has no repair because activation is an explicit operator
/// decision, not a safe mechanical one.
fn cost_model_active(ctx: &DoctorContext) -> CheckOutcome {
    let status = if ctx.db_missing {
        CheckStatus::NotApplicable("no ledger database exists yet".to_string())
    } else if let Some(error) = &ctx.db_open_error {
        CheckStatus::Fail(format!("cannot open the ledger database: {error}"))
    } else {
        match ctx.db {
            None => CheckStatus::Fail("no open connection to the ledger database".to_string()),
            Some(conn) => match crate::store::cost_model::load_active_at(conn, ctx.timestamp) {
                Err(error) => {
                    CheckStatus::Fail(format!("cannot read the active cost model: {error}"))
                }
                Ok(Some(model)) => CheckStatus::PassWithDetail(format!(
                    "active cost model: {}",
                    model.id().as_str()
                )),
                Ok(None) => match crate::store::rate_card::count(conn) {
                    Err(error) => CheckStatus::Fail(format!("cannot count rate cards: {error}")),
                    Ok(0) => CheckStatus::NotApplicable(
                        "no rate card is imported, so no credits pricing is due yet".to_string(),
                    ),
                    Ok(cards) => CheckStatus::Warn(format!(
                        "{cards} rate card row(s) imported but no cost model is active; run `aub cost-model activate {}`",
                        crate::store::cost_model::ANTHROPIC_CLAUDE_MESSAGES_V1_ID
                    )),
                },
            },
        }
    };
    outcome(CheckName::CostModelActive, status)
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
            CheckStatus::NotApplicable("no ledger database exists yet".to_string())
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
    fn meter_anomaly_detail_includes_a_bounded_sample() {
        use crate::domain::window::WindowScope;
        use crate::store::account::AccountId;
        use crate::store::meter_evidence::{ObservationRowId, WindowRowId};
        use crate::store::window_anomaly::{StoredWindowAnomaly, WindowAnomalyRowId};

        let anomalies: Vec<_> = (1..=METER_ANOMALY_SAMPLE_LIMIT as i64 + 1)
            .map(|id| StoredWindowAnomaly {
                row_id: WindowAnomalyRowId::new(id),
                kind: crate::domain::window_anomaly::WindowAnomalyKind::UnexpectedResetTimestampChange,
                account_id: AccountId::new(id),
                scope: WindowScope::AccountWide,
                prior_observation_id: ObservationRowId::new(id * 10),
                prior_window_id: WindowRowId::new(id * 10),
                current_observation_id: ObservationRowId::new(id * 10 + 1),
                current_window_id: WindowRowId::new(id * 10 + 1),
                detected_at: UtcTimestamp::from_unix_nanos(id),
                detail: String::new(),
            })
            .collect();

        let detail = meter_anomaly_detail(&anomalies);
        assert!(
            detail.contains("6 window anomaly(ies), showing 5"),
            "{detail}"
        );
        assert!(detail.contains("id=1 "), "{detail}");
        assert!(detail.contains("id=5 "), "{detail}");
        assert!(!detail.contains("id=6 "), "{detail}");
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

    // --- aub-a0tj: one answer about an env credential, whichever shell asked ---

    /// One account, `credential = { kind = "env", name = <var> }`, the shape
    /// `aub-r7k0`/`aub-e2uz` chose for the OpenCode session cookie.
    fn env_account_config(
        state_dir: &std::path::Path,
        provider: &str,
        name: &str,
        var: &str,
    ) -> Config {
        let env = RealEnv;
        let workspace_line = if provider == "opencode" {
            "opencode_workspace = \"wrk_test\"\n"
        } else {
            ""
        };
        let toml = format!(
            "[state]\ndir = {:?}\n\n[[accounts]]\nname = {:?}\nprovider = {:?}\n{workspace_line}\
             credential = {{ kind = \"env\", name = {:?} }}\n",
            state_dir, name, provider, var
        );
        let (config, _) = resolve(&Overrides::new(), &env, Some(&toml), "aub.toml")
            .expect("env-credential account config must resolve");
        config
    }

    /// One account, `credential = { kind = "file", path = <path> }`, for
    /// proving the `file` kind is unaffected by this bead's ledger fallback.
    fn missing_file_account_config(
        state_dir: &std::path::Path,
        provider: &str,
        name: &str,
        path: &std::path::Path,
    ) -> Config {
        let env = RealEnv;
        let toml = format!(
            "[state]\ndir = {:?}\n\n[[accounts]]\nname = {:?}\nprovider = {:?}\n\
             credential = {{ kind = \"file\", path = {:?} }}\n",
            state_dir, name, provider, path
        );
        let (config, _) = resolve(&Overrides::new(), &env, Some(&toml), "aub.toml")
            .expect("file-credential account config must resolve");
        config
    }

    /// One account, no `credential` table at all, i.e. the `none` kind
    /// (Codex reads its meter from the transcript).
    fn no_credential_account_config(
        state_dir: &std::path::Path,
        provider: &str,
        name: &str,
    ) -> Config {
        let env = RealEnv;
        let toml = format!(
            "[state]\ndir = {:?}\n\n[[accounts]]\nname = {:?}\nprovider = {:?}\n",
            state_dir, name, provider
        );
        let (config, _) = resolve(&Overrides::new(), &env, Some(&toml), "aub.toml")
            .expect("no-credential account config must resolve");
        config
    }

    /// Seeds one account with a single successful sampling attempt at
    /// `started_at`: the minimum fixture `env_credential_resolves_for_sampler`
    /// needs. No observation or window, because the check never reads either.
    fn seed_successful_attempt(
        conn: &rusqlite::Connection,
        provider: &str,
        name: &str,
        started_at: UtcTimestamp,
    ) {
        use crate::domain::attempt::AttemptOutcome;
        use crate::domain::time::MonotonicDuration;
        use crate::store::account::observe_account;
        use crate::store::meter_attempt::{
            DueReason, NewMeterAttempt, NewMeterAttemptResult, record_meter_attempt_result,
            start_meter_attempt,
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

        let account =
            observe_account(conn, provider, name, started_at).expect("account must insert");
        let run = start_sample_run(conn, Trigger::Manual, started_at, "seed")
            .expect("sample run must insert");
        let snapshot = resolve_policy_snapshot(conn, account, started_at, &POLICY)
            .expect("policy snapshot must insert");
        let attempt = start_meter_attempt(
            conn,
            &NewMeterAttempt {
                run_id: run,
                account_id: account,
                provider: provider.to_string(),
                request_started_at: started_at,
                credential_context_id: Some("ctx".into()),
                policy_snapshot_id: snapshot,
                due_at: started_at,
                due_reason: DueReason::OrdinaryCadence,
                due_basis: None,
                provider_contract_id: "endpoint-schema-v3".into(),
                meter_semantics_id: "account-5h-v2".into(),
            },
        )
        .expect("attempt must insert");
        record_meter_attempt_result(
            conn,
            &NewMeterAttemptResult {
                attempt_id: attempt,
                completed_at: started_at,
                elapsed: MonotonicDuration::from_millis(10),
                outcome: AttemptOutcome::Success,
                sanitized_error_classification: None,
                retry_index: None,
                clock_anomaly: false,
            },
        )
        .expect("result must insert");
    }

    fn open_fresh_ledger(db_path: &std::path::Path) -> rusqlite::Connection {
        crate::store::rate_card::open_ledger(
            db_path,
            crate::domain::time::MonotonicDuration::from_millis(500),
            &crate::domain::time::RealClock::new(),
        )
        .expect("a fresh ledger must open and migrate")
    }

    /// An `env` credential this process cannot see, but that the sampler
    /// resolved successfully a minute ago, passes rather than failing: the
    /// recovered case, evidenced from the ledger.
    #[test]
    fn env_credential_absent_in_process_but_recently_successful_for_sampler_passes() {
        let dir = scratch_dir("env-recovered");
        std::fs::create_dir_all(&dir).expect("scratch dir must be creatable");
        let config = env_account_config(
            &dir,
            "opencode",
            "opencode",
            "AUB_A0TJ_TEST_ENV_CREDENTIAL_UNSET",
        );
        let conn = open_fresh_ledger(&dir.join("ledger.sqlite3"));
        let started_at = UtcTimestamp::from_unix_nanos(1_700_000_000_000_000_000);
        seed_successful_attempt(&conn, "opencode", "opencode", started_at);

        let mut ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));
        ctx.db_missing = false;
        ctx.db = Some(&conn);
        // A minute after the successful attempt: comfortably inside the
        // three-times-default-interval recency window.
        ctx.timestamp = UtcTimestamp::from_unix_nanos(started_at.unix_nanos() + 60_000_000_000);

        let outcome = unresolved_authentication(&ctx);
        match outcome.status {
            CheckStatus::PassWithDetail(detail) => {
                assert!(detail.contains("opencode"), "{detail}");
                assert!(detail.contains("not set in this process"), "{detail}");
                assert!(detail.contains("resolves for the sampler"), "{detail}");
            }
            CheckStatus::Pass
            | CheckStatus::Warn(_)
            | CheckStatus::Fail(_)
            | CheckStatus::NotApplicable(_)
            | CheckStatus::NotYetAvailable { .. } => {
                panic!("expected PassWithDetail, got {:?}", outcome.name)
            }
        }
    }

    /// Planted negative: an `env` credential unset in this process, with no
    /// successful sampler attempt anywhere in the ledger, still fails loudly.
    /// This is the case the recovery path must never turn into a false green.
    #[test]
    fn env_credential_absent_everywhere_still_fails() {
        let dir = scratch_dir("env-absent-everywhere");
        std::fs::create_dir_all(&dir).expect("scratch dir must be creatable");
        let config = env_account_config(
            &dir,
            "opencode",
            "opencode",
            "AUB_A0TJ_TEST_ENV_CREDENTIAL_UNSET",
        );
        let conn = open_fresh_ledger(&dir.join("ledger.sqlite3"));
        // No attempt seeded at all: the account has never been observed.

        let mut ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));
        ctx.db_missing = false;
        ctx.db = Some(&conn);

        let outcome = unresolved_authentication(&ctx);
        assert!(
            matches!(outcome.status, CheckStatus::Fail(ref msg) if msg.contains("opencode")),
            "{:?}",
            outcome.status
        );
    }

    /// The same planted negative, but proving the *staleness* half rather
    /// than the absence half: a successful attempt exists, but it is older
    /// than the recency window this bead's fallback reads, so it can no
    /// longer speak for the credential's current state.
    #[test]
    fn env_credential_with_only_a_stale_successful_attempt_still_fails() {
        let dir = scratch_dir("env-stale-success");
        std::fs::create_dir_all(&dir).expect("scratch dir must be creatable");
        let config = env_account_config(
            &dir,
            "opencode",
            "opencode",
            "AUB_A0TJ_TEST_ENV_CREDENTIAL_UNSET",
        );
        let conn = open_fresh_ledger(&dir.join("ledger.sqlite3"));
        let started_at = UtcTimestamp::from_unix_nanos(1_700_000_000_000_000_000);
        seed_successful_attempt(&conn, "opencode", "opencode", started_at);

        let mut ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));
        ctx.db_missing = false;
        ctx.db = Some(&conn);
        // One hour later: past the three-times-five-minute recency window.
        ctx.timestamp = UtcTimestamp::from_unix_nanos(started_at.unix_nanos() + 3_600_000_000_000);

        let outcome = unresolved_authentication(&ctx);
        assert!(
            matches!(outcome.status, CheckStatus::Fail(ref msg) if msg.contains("opencode")),
            "{:?}",
            outcome.status
        );
    }

    /// An `env` credential this process *can* see resolves the same way this
    /// bead's fallback never gets a chance to run: the direct success path is
    /// unaffected by the ledger reachable from the same context.
    #[test]
    fn env_credential_present_in_process_passes_without_consulting_the_ledger() {
        let dir = scratch_dir("env-present");
        std::fs::create_dir_all(&dir).expect("scratch dir must be creatable");
        const VAR: &str = "AUB_A0TJ_TEST_ENV_CREDENTIAL_SET";
        let config = env_account_config(&dir, "opencode", "opencode", VAR);
        // No ledger at all: if the direct resolution did not short-circuit
        // the ledger fallback, this would panic on a missing connection
        // instead of passing.
        let ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));

        // SAFETY: VAR is unique to this test and read by no other code in
        // this crate, so no concurrently running test can observe or race it.
        unsafe {
            std::env::set_var(VAR, "cookie-value");
        }
        let outcome = unresolved_authentication(&ctx);
        unsafe {
            std::env::remove_var(VAR);
        }

        assert_eq!(outcome.status, CheckStatus::Pass);
    }

    /// `file`-kind and `none`-kind credentials never consult the ledger this
    /// bead added: a missing file still fails even with a ledger sitting
    /// right there recording the same account's sampler as succeeding, and a
    /// `none` credential still passes trivially.
    #[test]
    fn file_and_none_kind_credentials_are_unaffected() {
        let dir = scratch_dir("file-and-none-unaffected");
        std::fs::create_dir_all(&dir).expect("scratch dir must be creatable");
        let conn = open_fresh_ledger(&dir.join("ledger.sqlite3"));
        let started_at = UtcTimestamp::from_unix_nanos(1_700_000_000_000_000_000);
        // A successful attempt for the very account whose file credential is
        // about to fail to resolve: if the fallback ignored credential kind,
        // this would turn the file failure into a false green.
        seed_successful_attempt(&conn, "provider-a", "primary", started_at);

        let missing_path = dir.join("does-not-exist.json");
        let file_config = missing_file_account_config(&dir, "provider-a", "primary", &missing_path);
        let mut file_ctx = empty_ctx(&file_config, dir.join("ledger.sqlite3"));
        file_ctx.db_missing = false;
        file_ctx.db = Some(&conn);
        file_ctx.timestamp = started_at;
        assert!(matches!(
            unresolved_authentication(&file_ctx).status,
            CheckStatus::Fail(_)
        ));

        let none_config = no_credential_account_config(&dir, "provider-a", "codex");
        let mut none_ctx = empty_ctx(&none_config, dir.join("ledger.sqlite3"));
        none_ctx.db_missing = false;
        none_ctx.db = Some(&conn);
        none_ctx.timestamp = started_at;
        assert_eq!(
            unresolved_authentication(&none_ctx).status,
            CheckStatus::Pass
        );
    }

    /// Integration-shaped: the full registry's pass/fail counts must agree
    /// between an invocation that carries the credential's variable and one
    /// that does not, against a ledger where the account is sampling
    /// successfully. This is the summary line's own promise (aub-a0tj).
    #[test]
    fn doctor_summary_counts_agree_regardless_of_which_shell_started_it() {
        let dir = scratch_dir("summary-agrees");
        std::fs::create_dir_all(&dir).expect("scratch dir must be creatable");
        const VAR: &str = "AUB_A0TJ_TEST_ENV_CREDENTIAL_SUMMARY";
        let config = env_account_config(&dir, "opencode", "opencode", VAR);
        let conn = open_fresh_ledger(&dir.join("ledger.sqlite3"));
        let started_at = UtcTimestamp::from_unix_nanos(1_700_000_000_000_000_000);
        seed_successful_attempt(&conn, "opencode", "opencode", started_at);

        let mut ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));
        ctx.db_missing = false;
        ctx.db = Some(&conn);
        ctx.timestamp = UtcTimestamp::from_unix_nanos(started_at.unix_nanos() + 60_000_000_000);

        let without_var = build_registry(&ctx);
        let passed_without = without_var
            .iter()
            .filter(|o| matches!(o.status, CheckStatus::Pass | CheckStatus::PassWithDetail(_)))
            .count();
        let failed_without = without_var
            .iter()
            .filter(|o| matches!(o.status, CheckStatus::Fail(_)))
            .count();

        // SAFETY: VAR is unique to this test and read by no other code in
        // this crate, so no concurrently running test can observe or race it.
        unsafe {
            std::env::set_var(VAR, "cookie-value");
        }
        let with_var = build_registry(&ctx);
        unsafe {
            std::env::remove_var(VAR);
        }
        let passed_with = with_var
            .iter()
            .filter(|o| matches!(o.status, CheckStatus::Pass | CheckStatus::PassWithDetail(_)))
            .count();
        let failed_with = with_var
            .iter()
            .filter(|o| matches!(o.status, CheckStatus::Fail(_)))
            .count();

        assert_eq!(passed_without, passed_with);
        assert_eq!(failed_without, failed_with);
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
            | CheckStatus::Warn(_)
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
            | CheckStatus::Warn(_)
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
            | CheckStatus::Warn(_)
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
                resets_at: UtcTimestamp::from_unix_nanos(100_000).into(),
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
            | CheckStatus::Warn(_)
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

    /// The subscription-identity change check (aub-iwkg): fails while an
    /// account's newest history row refuses its readings, passes quietly
    /// with no history, and passes with a historical note once readings
    /// under the established subscription resumed.
    fn subscription_test_config(state_dir: &std::path::Path) -> Config {
        let env = RealEnv;
        let toml = format!(
            "[state]\ndir = {:?}\n\n[[accounts]]\nname = \"primary\"\nprovider = \"anthropic\"\ncredential = {{ kind = \"file\", path = \"/nonexistent/creds.json\" }}\n",
            state_dir
        );
        let (config, _) =
            resolve(&Overrides::new(), &env, Some(&toml), "aub.toml").expect("config must resolve");
        config
    }

    /// A migrated ledger with one account, one run and two attempts: the
    /// parents every subscription-history row references.
    fn seeded_subscription_parents(
        conn: &rusqlite::Connection,
    ) -> (
        crate::store::account::AccountId,
        crate::store::meter_attempt::MeterAttemptRowId,
        crate::store::meter_attempt::MeterAttemptRowId,
    ) {
        use crate::domain::time::MonotonicDuration;
        use crate::store::account::observe_account;
        use crate::store::meter_attempt::{DueReason, NewMeterAttempt, start_meter_attempt};
        use crate::store::sample_run::{Trigger, start_sample_run};
        use crate::store::sampling_policy_snapshot::{
            ResolvedSamplingPolicy, resolve_policy_snapshot,
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
        let snapshot = resolve_policy_snapshot(
            conn,
            account,
            UtcTimestamp::from_unix_nanos(10),
            &ResolvedSamplingPolicy {
                ordinary_cadence: MonotonicDuration::from_seconds(300),
                freshness_horizon: MonotonicDuration::from_seconds(900),
                reset_edge_policy: "lead-120s".into(),
                retry_backoff_policy: "exponential-3".into(),
                command_budget: MonotonicDuration::from_seconds(30),
                policy_algorithm_version: "v1".into(),
            },
        )
        .expect("policy snapshot must insert");
        let attempt = |started: i64| {
            start_meter_attempt(
                conn,
                &NewMeterAttempt {
                    run_id: run,
                    account_id: account,
                    provider: "anthropic".into(),
                    request_started_at: UtcTimestamp::from_unix_nanos(started),
                    credential_context_id: Some("ctx".into()),
                    policy_snapshot_id: snapshot,
                    due_at: UtcTimestamp::from_unix_nanos(started - 1),
                    due_reason: DueReason::ForcedOrManual,
                    due_basis: None,
                    provider_contract_id: "contract-v1".into(),
                    meter_semantics_id: "semantics-v1".into(),
                },
            )
            .expect("attempt must insert")
        };
        (account, attempt(20), attempt(40))
    }

    fn record_established(
        conn: &rusqlite::Connection,
        account: crate::store::account::AccountId,
        attempt: crate::store::meter_attempt::MeterAttemptRowId,
        identity: &str,
        at: i64,
    ) {
        use crate::store::subscription_identity::{
            NewSubscriptionChange, SubscriptionChangeKind, insert_change,
        };
        insert_change(
            conn,
            &NewSubscriptionChange {
                account_id: account,
                kind: SubscriptionChangeKind::Established,
                previous_identity: None,
                current_identity: identity.into(),
                detecting_attempt_id: attempt,
                previous_observation_id: None,
                detected_at: UtcTimestamp::from_unix_nanos(at),
            },
        )
        .expect("establishment must insert");
    }

    fn record_changed(
        conn: &rusqlite::Connection,
        account: crate::store::account::AccountId,
        attempt: crate::store::meter_attempt::MeterAttemptRowId,
        previous: &str,
        current: &str,
        at: i64,
    ) {
        use crate::store::subscription_identity::{
            NewSubscriptionChange, SubscriptionChangeKind, insert_change,
        };
        insert_change(
            conn,
            &NewSubscriptionChange {
                account_id: account,
                kind: SubscriptionChangeKind::Changed,
                previous_identity: Some(previous.into()),
                current_identity: current.into(),
                detecting_attempt_id: attempt,
                previous_observation_id: None,
                detected_at: UtcTimestamp::from_unix_nanos(at),
            },
        )
        .expect("change must insert");
    }

    #[test]
    fn a_refused_subscription_change_fails_the_check() {
        let dir = scratch_dir("subscription-refused");
        std::fs::create_dir_all(&dir).expect("scratch dir must be creatable");
        let config = subscription_test_config(&dir);
        let mut ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));
        ctx.db_missing = false;
        let conn = crate::store::rate_card::open_ledger(
            &ctx.db_path,
            crate::domain::time::MonotonicDuration::from_millis(500),
            &crate::domain::time::RealClock::new(),
        )
        .expect("a fresh ledger must open and migrate");
        let (account, first, second) = seeded_subscription_parents(&conn);
        record_established(&conn, account, first, "anthropic:max:tier", 30);
        record_changed(
            &conn,
            account,
            second,
            "anthropic:max:tier",
            "anthropic:pro:tier",
            50,
        );
        ctx.db = Some(&conn);
        let outcome = subscription_identity_change(&ctx);
        match outcome.status {
            CheckStatus::Fail(message) => {
                assert!(message.contains("primary"), "{message}");
                assert!(message.contains("anthropic:pro:tier"), "{message}");
            }
            CheckStatus::Pass
            | CheckStatus::Warn(_)
            | CheckStatus::PassWithDetail(_)
            | CheckStatus::NotApplicable(_)
            | CheckStatus::NotYetAvailable { .. } => {
                panic!("a refused change must fail, got {:?}", outcome.status)
            }
        }
        assert!(
            !outcome.has_repair,
            "no repair can acknowledge a subscription"
        );
    }

    #[test]
    fn no_subscription_history_passes_quietly() {
        let dir = scratch_dir("subscription-quiet");
        std::fs::create_dir_all(&dir).expect("scratch dir must be creatable");
        let config = subscription_test_config(&dir);
        let mut ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));
        ctx.db_missing = false;
        let conn = crate::store::rate_card::open_ledger(
            &ctx.db_path,
            crate::domain::time::MonotonicDuration::from_millis(500),
            &crate::domain::time::RealClock::new(),
        )
        .expect("a fresh ledger must open and migrate");
        let (_account, _first, _second) = seeded_subscription_parents(&conn);
        ctx.db = Some(&conn);
        let outcome = subscription_identity_change(&ctx);
        assert_eq!(outcome.status, CheckStatus::Pass);
    }

    #[test]
    fn an_observation_newer_than_the_change_passes_with_a_historical_note() {
        // Planted negative: the newest row is still a change, so a check
        // that only read the latest kind would keep failing. The observation
        // stored after the change proves readings under the established
        // subscription resumed.
        let dir = scratch_dir("subscription-resumed");
        std::fs::create_dir_all(&dir).expect("scratch dir must be creatable");
        let config = subscription_test_config(&dir);
        let mut ctx = empty_ctx(&config, dir.join("ledger.sqlite3"));
        ctx.db_missing = false;
        let conn = crate::store::rate_card::open_ledger(
            &ctx.db_path,
            crate::domain::time::MonotonicDuration::from_millis(500),
            &crate::domain::time::RealClock::new(),
        )
        .expect("a fresh ledger must open and migrate");
        let (account, first, second) = seeded_subscription_parents(&conn);
        record_established(&conn, account, first, "anthropic:max:tier", 30);
        record_changed(
            &conn,
            account,
            second,
            "anthropic:max:tier",
            "anthropic:pro:tier",
            50,
        );
        // An observation received after the change was detected.
        let evidence = crate::store::meter_evidence::insert_response_evidence(
            &conn,
            &crate::store::meter_evidence::NewMeterResponseEvidence {
                attempt_id: first,
                response_classification: "200".into(),
                received_at: UtcTimestamp::from_unix_nanos(60),
                provider_observed_at_original: None,
                evidence_capsule: r#"{"windows":[]}"#.into(),
                capsule_schema_version: "capsule-v1".into(),
                sanitizer_version: "sanitizer-v1".into(),
                capture_truncated: false,
            },
        )
        .expect("evidence must insert");
        {
            use crate::domain::ids::{AdapterVersion, MeterSemanticsId, ProviderContractId};
            use crate::domain::time::{MeasurementBasis, MonotonicDuration as Duration};
            let _ = Duration::from_seconds(1);
            crate::store::meter_evidence::insert_observation(
                &conn,
                &crate::store::meter_evidence::NewMeterObservation {
                    attempt_id: first,
                    evidence_id: evidence,
                    account_id: account,
                    provider: "anthropic".into(),
                    provider_observed_at: None,
                    received_at: UtcTimestamp::from_unix_nanos(60),
                    measurement_basis: MeasurementBasis::LocallyReceived,
                    observed_plan: None,
                    observed_tier: None,
                    adapter_version: AdapterVersion::new("adapter-v1"),
                    provider_contract_id: ProviderContractId::new("contract-v1"),
                    meter_semantics_id: MeterSemanticsId::new("semantics-v1"),
                    normalized_fingerprint: "fp-1".into(),
                },
            )
            .expect("observation must insert");
        }
        ctx.db = Some(&conn);
        let outcome = subscription_identity_change(&ctx);
        match outcome.status {
            CheckStatus::PassWithDetail(message) => {
                assert!(message.contains("primary"), "{message}");
                assert!(message.contains("resumed"), "{message}");
            }
            CheckStatus::Pass
            | CheckStatus::Warn(_)
            | CheckStatus::Fail(_)
            | CheckStatus::NotApplicable(_)
            | CheckStatus::NotYetAvailable { .. } => {
                panic!(
                    "a resumed account must pass with detail, got {:?}",
                    outcome.status
                )
            }
        }
    }
}
