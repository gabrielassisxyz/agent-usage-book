//! Controlled calibration experiment runs (`aub-c0b.2`, PLAN.md 23.3, 23.4).
//!
//! A controlled experiment is a record, not a session: `calibrate begin` writes
//! the premise (account, plan tier, target window, cost-model version, baseline
//! meter observation, start timestamp and the explicit exclusivity assertion),
//! the process exits, the ordinary scheduler keeps sampling, and `calibrate
//! end` later records the end of controlled local work. Sampling continues
//! afterwards so server-side accounting can catch up; `end` never declares the
//! meter settled, it only closes the controlled-work boundary. Whether the
//! meter settled is recomputed on every `status` from the stored observations
//! through the settled-boundary detector (`aub-c0b.3`), so a later change to
//! the reading never rewrites what the experiment meant.
//!
//! All state lives in `calibration_controlled_run` (migration 0028). The
//! `begin` columns are immutable; the one permitted mutation is the single
//! `NULL -> set` transition of `ended_at` performed only by [`record_end`].
//! There is no resident process and no in-memory state: reopening the database
//! is a simulated reboot that loses nothing.
//!
//! May not depend on:
//! - HTTP or terminal-formatting crates
//! - presentation
//! - provider adapters

use rusqlite::{Connection, OptionalExtension, params};

use crate::calibration::contamination::{
    ContaminationInputs, ContaminationMarkerPoint, ContaminationMeterPoint,
    ContaminationThresholds, ContaminationVerdict, evaluate_contamination,
    require_uncontaminated_for_activation,
};
use crate::calibration::settlement::{
    SettlementMeterObservation, SettlementObservationSeries, SettlementOutcome, SettlementPolicy,
    SettlementRole, detect_settlement,
};
use crate::domain::credits::Credits;
use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
use crate::domain::time::{MonotonicDuration, UtcTimestamp};
use crate::domain::tokens::TokenKind;
use crate::domain::window::{ReportedResolution, WindowSemanticKey};
use crate::error::Error;
use crate::store::account::account_id_by_identity;
use crate::store::calibration::PlanTier;
use crate::store::cost_model::{CostModel, ProviderKey};
use crate::store::meter_evidence::{ObservationRowId, observation_times_for_account_between};

/// The semantic identifier of one controlled experiment run, as named on the
/// `calibrate` command line.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ControlledExperimentId(String);

impl ControlledExperimentId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Which side of the controlled-work boundary an experiment sits on. There are
/// exactly two: controlled work is still running, or its end was recorded.
/// Settlement is not a phase: it is recomputed on every `status`, so an ended
/// experiment can be settled or not without anything being rewritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlledRunPhase {
    Running,
    Ended,
}

impl ControlledRunPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Ended => "ended",
        }
    }
}

/// One controlled experiment run: the premise `begin` recorded plus the
/// optional end boundary. The exclusivity assertion is a recorded premise,
/// never an enforced lock: `aub` does not stop other work on the account, it
/// records that the experiment assumed none, which is what makes a later
/// contamination finding meaningful rather than a surprise.
///
/// `baseline_plateau_started_at` is the idle plateau period `begin` asserted:
/// the earliest stored observation in the trailing stable run ending at the
/// baseline, so the pre-burn contamination check examines exactly the window
/// `begin` vouched for. `contamination_thresholds` are the per-signal
/// thresholds `begin` recorded; detection reads them from this row, never from
/// a source constant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlledExperimentRun {
    pub id: ControlledExperimentId,
    pub account: String,
    pub provider: ProviderKey,
    pub plan_tier: PlanTier,
    pub window_semantic_key: WindowSemanticKey,
    pub cost_model_id: crate::domain::provenance::CostModelId,
    pub expected_token_kinds: Vec<TokenKind>,
    pub baseline_observation_id: ObservationRowId,
    pub baseline_quota_used: QuotaUsed,
    pub baseline_resolution: ReportedResolution,
    pub baseline_observed_at: UtcTimestamp,
    pub baseline_plateau_started_at: UtcTimestamp,
    pub contamination_thresholds: ContaminationThresholds,
    pub started_at: UtcTimestamp,
    pub ended_at: Option<UtcTimestamp>,
    pub exclusivity_assertion: String,
}

impl ControlledExperimentRun {
    pub fn phase(&self) -> ControlledRunPhase {
        match self.ended_at {
            None => ControlledRunPhase::Running,
            Some(_) => ControlledRunPhase::Ended,
        }
    }
}

/// What `status` reports for one run: the phase, the elapsed wall time since
/// `begin`, the samples collected since the baseline observation, and whether
/// the settlement criterion is currently met. Settlement is a fresh detection
/// over the stored series, never a stored flag, so `end` cannot declare it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlledRunStatus {
    pub phase: ControlledRunPhase,
    pub elapsed: MonotonicDuration,
    pub samples_since_baseline: u64,
    pub settlement: SettlementOutcome,
}

impl ControlledRunStatus {
    pub fn is_settled(&self) -> bool {
        self.settlement.is_settled()
    }
}

/// The default expected token kinds when `begin` names none: every known kind,
/// which is the same as requiring a complete cost model.
pub fn default_expected_token_kinds() -> Vec<TokenKind> {
    TokenKind::ALL.to_vec()
}

/// Parses the `--expect-kinds` value: comma-separated stable [`TokenKind`]
/// labels in any order, deduplicated into [`TokenKind::ALL`] order. Empty
/// input and unknown labels are refused rather than guessed at.
pub fn parse_expected_token_kinds(text: &str) -> Result<Vec<TokenKind>, Error> {
    let mut selected = Vec::new();
    for raw in text.split(',') {
        let label = raw.trim();
        if label.is_empty() {
            return Err(Error::Usage(
                "calibrate begin --expect-kinds names no token kind; use input,output,cache_read,cache_write or omit the flag".into(),
            ));
        }
        let mut matched = None;
        for kind in TokenKind::ALL {
            if kind.label() == label {
                matched = Some(kind);
                break;
            }
        }
        match matched {
            Some(kind) => {
                if !selected.contains(&kind) {
                    selected.push(kind);
                }
            }
            None => {
                return Err(Error::Usage(format!(
                    "calibrate begin --expect-kinds names unknown token kind '{label}'; use input,output,cache_read,cache_write"
                )));
            }
        }
    }
    if selected.is_empty() {
        return Err(Error::Usage(
            "calibrate begin --expect-kinds names no token kind; use input,output,cache_read,cache_write or omit the flag".into(),
        ));
    }
    let mut ordered = TokenKind::ALL.to_vec();
    ordered.retain(|kind| selected.contains(kind));
    Ok(ordered)
}

/// Encodes expected token kinds for the `expected_token_kinds` column, in
/// [`TokenKind::ALL`] order so the stored form is canonical.
pub fn encode_expected_token_kinds(kinds: &[TokenKind]) -> String {
    let mut ordered = TokenKind::ALL.to_vec();
    ordered.retain(|kind| kinds.contains(kind));
    ordered
        .iter()
        .copied()
        .map(TokenKind::label)
        .collect::<Vec<_>>()
        .join(",")
}

/// Every expected token kind the cost model carries no term for. A non-empty
/// result is why `begin` refuses: fitting window capacity against credits the
/// model cannot price would silently misattribute the burn (PLAN.md 23.8).
pub fn missing_expected_terms(model: &CostModel, expected: &[TokenKind]) -> Vec<TokenKind> {
    expected
        .iter()
        .copied()
        .filter(|kind| model.term(*kind).is_none())
        .collect()
}

fn row_to_run(row: &rusqlite::Row<'_>) -> Result<ControlledExperimentRun, Error> {
    let expected_text: String = row
        .get(6)
        .map_err(|e| Error::Store(format!("cannot read expected_token_kinds: {e}")))?;
    let baseline_quota_ppm: i64 = row
        .get(8)
        .map_err(|e| Error::Store(format!("cannot read baseline_quota_used_ppm: {e}")))?;
    let baseline_resolution_ppm: i64 = row
        .get(9)
        .map_err(|e| Error::Store(format!("cannot read baseline resolution: {e}")))?;
    let ended_at: Option<i64> = row
        .get(12)
        .map_err(|e| Error::Store(format!("cannot read ended_at: {e}")))?;
    let baseline_quota = i32::try_from(baseline_quota_ppm)
        .ok()
        .and_then(QuotaFractionPpm::new)
        .map(QuotaUsed::new)
        .ok_or_else(|| Error::Store("stored baseline quota is outside 0..=1000000".into()))?;
    let baseline_resolution = i32::try_from(baseline_resolution_ppm)
        .ok()
        .and_then(QuotaFractionPpm::new)
        .and_then(ReportedResolution::new)
        .ok_or_else(|| Error::Store("stored baseline resolution is invalid".into()))?;
    let baseline_observed_at_nanos: i64 = row
        .get(10)
        .map_err(|e| Error::Store(format!("cannot read baseline_observed_at: {e}")))?;
    let plateau_started_at_nanos: i64 = row
        .get(14)
        .map_err(|e| Error::Store(format!("cannot read baseline_plateau_started_at: {e}")))?;
    let contamination_version: String = row
        .get(15)
        .map_err(|e| Error::Store(format!("cannot read contamination version: {e}")))?;
    let pre_burn_max: i64 = row
        .get(16)
        .map_err(|e| Error::Store(format!("cannot read contamination pre-burn tolerance: {e}")))?;
    let post_settlement_max: i64 = row
        .get(17)
        .map_err(|e| Error::Store(format!("cannot read contamination drift tolerance: {e}")))?;
    let post_settlement_grace: i64 = row
        .get(18)
        .map_err(|e| Error::Store(format!("cannot read contamination grace: {e}")))?;
    let flat_meter_min: i64 = row.get(19).map_err(|e| {
        Error::Store(format!(
            "cannot read contamination flat-meter threshold: {e}"
        ))
    })?;
    let flat_local_max: i64 = row.get(20).map_err(|e| {
        Error::Store(format!(
            "cannot read contamination flat-local threshold: {e}"
        ))
    })?;
    let contamination_thresholds = ContaminationThresholds::new(
        contamination_version,
        u32::try_from(pre_burn_max).map_err(|_| {
            Error::Store("stored contamination pre-burn tolerance is out of u32 range".into())
        })?,
        u32::try_from(post_settlement_max).map_err(|_| {
            Error::Store("stored contamination drift tolerance is out of u32 range".into())
        })?,
        u64::try_from(post_settlement_grace)
            .map(MonotonicDuration::from_nanos)
            .map_err(|_| Error::Store("stored contamination grace is negative".into()))?,
        u32::try_from(flat_meter_min).map_err(|_| {
            Error::Store("stored contamination flat-meter threshold is out of u32 range".into())
        })?,
        Credits::from_micros(flat_local_max),
    )
    .map_err(|e| Error::Store(format!("stored contamination thresholds are invalid: {e}")))?;
    let get_string = |index: usize| -> Result<String, Error> {
        row.get(index)
            .map_err(|e| Error::Store(format!("cannot read column {index}: {e}")))
    };
    Ok(ControlledExperimentRun {
        id: ControlledExperimentId::new(get_string(0)?),
        account: get_string(1)?,
        provider: ProviderKey::new(get_string(2)?),
        plan_tier: PlanTier::new(get_string(3)?),
        window_semantic_key: WindowSemanticKey::new(get_string(4)?),
        cost_model_id: crate::domain::provenance::CostModelId::new(get_string(5)?),
        expected_token_kinds: parse_stored_expected_kinds(&expected_text)?,
        baseline_observation_id: ObservationRowId::new(
            row.get(7)
                .map_err(|e| Error::Store(format!("cannot read baseline_observation_id: {e}")))?,
        ),
        baseline_quota_used: baseline_quota,
        baseline_resolution,
        baseline_observed_at: UtcTimestamp::from_unix_nanos(baseline_observed_at_nanos),
        baseline_plateau_started_at: UtcTimestamp::from_unix_nanos(plateau_started_at_nanos),
        contamination_thresholds,
        started_at: UtcTimestamp::from_unix_nanos(
            row.get(11)
                .map_err(|e| Error::Store(format!("cannot read started_at: {e}")))?,
        ),
        ended_at: ended_at.map(UtcTimestamp::from_unix_nanos),
        exclusivity_assertion: get_string(13)?,
    })
}

fn parse_stored_expected_kinds(text: &str) -> Result<Vec<TokenKind>, Error> {
    parse_expected_token_kinds(text).map_err(|_| {
        Error::Store(format!(
            "stored expected token kinds are malformed: '{text}'"
        ))
    })
}

const RUN_COLUMNS: &str = "experiment_id, account, provider, plan_tier, window_semantic_key, \
     cost_model_id, expected_token_kinds, baseline_observation_id, baseline_quota_used_ppm, \
     baseline_reported_resolution_ppm, baseline_observed_at, started_at, ended_at, \
     exclusivity_assertion, baseline_plateau_started_at, contamination_policy_version, \
     contamination_pre_burn_max_movement_ppm, contamination_post_settlement_max_movement_ppm, \
     contamination_post_settlement_grace_nanos, contamination_flat_meter_min_movement_ppm, \
     contamination_flat_local_max_micros";

/// Records the `begin` premise. The `experiment_id` is unique; a second
/// `begin` with the same identifier fails at the database.
pub fn insert_begin(conn: &Connection, run: &ControlledExperimentRun) -> Result<i64, Error> {
    if run.exclusivity_assertion.trim().is_empty() {
        return Err(Error::Usage(
            "calibrate begin requires the explicit exclusivity assertion; pass --assert-exclusive"
                .into(),
        ));
    }
    if run.expected_token_kinds.is_empty() {
        return Err(Error::Usage(
            "calibrate begin names no expected token kind; use --expect-kinds or omit it for all four".into(),
        ));
    }
    conn.query_row(
        "INSERT INTO calibration_controlled_run (
            experiment_id, account, provider, plan_tier, window_semantic_key,
            cost_model_id, expected_token_kinds, baseline_observation_id,
            baseline_quota_used_ppm, baseline_reported_resolution_ppm,
            baseline_observed_at, started_at, ended_at, exclusivity_assertion,
            baseline_plateau_started_at, contamination_policy_version,
            contamination_pre_burn_max_movement_ppm,
            contamination_post_settlement_max_movement_ppm,
            contamination_post_settlement_grace_nanos,
            contamination_flat_meter_min_movement_ppm,
            contamination_flat_local_max_micros
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                  ?15, ?16, ?17, ?18, ?19, ?20, ?21)
        RETURNING id",
        params![
            run.id.as_str(),
            run.account,
            run.provider.as_str(),
            run.plan_tier.as_str(),
            run.window_semantic_key.as_str(),
            run.cost_model_id.as_str(),
            encode_expected_token_kinds(&run.expected_token_kinds),
            run.baseline_observation_id.value(),
            i64::from(run.baseline_quota_used.as_ppm().get()),
            i64::from(run.baseline_resolution.as_ppm().get()),
            run.baseline_observed_at.unix_nanos(),
            run.started_at.unix_nanos(),
            run.ended_at.map(UtcTimestamp::unix_nanos),
            run.exclusivity_assertion,
            run.baseline_plateau_started_at.unix_nanos(),
            run.contamination_thresholds.version(),
            i64::from(run.contamination_thresholds.pre_burn_max_movement_ppm()),
            i64::from(
                run.contamination_thresholds
                    .post_settlement_max_movement_ppm()
            ),
            i64::try_from(
                run.contamination_thresholds
                    .post_settlement_grace()
                    .as_nanos()
            )
            .map_err(|_| Error::Store(
                "contamination grace does not fit in SQLite INTEGER".into()
            ))?,
            i64::from(
                run.contamination_thresholds
                    .flat_credits_min_meter_movement_ppm()
            ),
            run.contamination_thresholds
                .flat_credits_max_local()
                .micros(),
        ],
        |row| row.get::<_, i64>(0),
    )
    .map_err(|e| Error::Store(format!("cannot record the calibrate begin row: {e}")))
}

/// Loads one run by its semantic identifier.
pub fn load_by_experiment_id(
    conn: &Connection,
    id: &ControlledExperimentId,
) -> Result<Option<ControlledExperimentRun>, Error> {
    conn.query_row(
        &format!("SELECT {RUN_COLUMNS} FROM calibration_controlled_run WHERE experiment_id = ?1"),
        params![id.as_str()],
        |row| row_to_run(row).map_err(store_error_to_sql),
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot load the controlled experiment run: {e}")))
}

/// The most recently begun run, if any. `status` and `end` read this when the
/// command line names no experiment.
pub fn load_latest(conn: &Connection) -> Result<Option<ControlledExperimentRun>, Error> {
    conn.query_row(
        &format!(
            "SELECT {RUN_COLUMNS} FROM calibration_controlled_run ORDER BY started_at DESC, id DESC LIMIT 1"
        ),
        [],
        |row| row_to_run(row).map_err(store_error_to_sql),
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot load the latest controlled experiment run: {e}")))
}

/// The still-running experiment for one account, if any. `begin` refuses when
/// this exists: a second controlled burn on the same account would break the
/// first experiment's exclusivity premise, so the refusal names the holder.
pub fn running_for_account(
    conn: &Connection,
    account: &str,
) -> Result<Option<ControlledExperimentRun>, Error> {
    conn.query_row(
        &format!(
            "SELECT {RUN_COLUMNS} FROM calibration_controlled_run WHERE account = ?1 AND ended_at IS NULL ORDER BY started_at DESC, id DESC LIMIT 1"
        ),
        params![account],
        |row| row_to_run(row).map_err(store_error_to_sql),
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot load the running experiment: {e}")))
}

/// Records the end of controlled local work. This is the one permitted
/// mutation of a run row: it sets `ended_at` once and changes nothing else
/// (the database trigger refuses every other `UPDATE` shape). It deliberately
/// says nothing about settlement: sampling continues afterwards so
/// server-side accounting can catch up, and `status` keeps reporting the
/// freshly detected settlement state.
pub fn record_end(
    conn: &Connection,
    id: &ControlledExperimentId,
    ended_at: UtcTimestamp,
) -> Result<(), Error> {
    let changed = conn
        .execute(
            "UPDATE calibration_controlled_run SET ended_at = ?1 WHERE experiment_id = ?2 AND ended_at IS NULL",
            params![ended_at.unix_nanos(), id.as_str()],
        )
        .map_err(|e| Error::Store(format!("cannot record the calibrate end: {e}")))?;
    if changed == 1 {
        return Ok(());
    }
    match load_by_experiment_id(conn, id)? {
        None => Err(Error::Usage(format!(
            "no controlled experiment '{}'; begin one with `aub calibrate begin`",
            id.as_str()
        ))),
        Some(_) => Err(Error::Usage(format!(
            "controlled experiment '{}' already ended; its end boundary is recorded once",
            id.as_str()
        ))),
    }
}

fn store_error_to_sql(e: Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
}

/// How many meter observations this account collected from the baseline
/// instant up to now, inclusive of the baseline itself. The count comes from
/// the stored observations, so dropping every in-memory handle and reopening
/// the database (a simulated reboot) changes nothing.
pub fn count_samples_since_baseline(
    conn: &Connection,
    run: &ControlledExperimentRun,
    now: UtcTimestamp,
) -> Result<u64, Error> {
    let Some(account_id) = account_id_by_identity(conn, run.provider.as_str(), &run.account)?
    else {
        return Ok(0);
    };
    let end = UtcTimestamp::from_unix_nanos(now.unix_nanos().saturating_add(1));
    let times =
        observation_times_for_account_between(conn, account_id, run.baseline_observed_at, end)?;
    u64::try_from(times.len()).map_err(|_| Error::Store("sample count out of u64 range".into()))
}

/// The stored quota series for the run's account and target window from the
/// series start up to now, oldest first. The series starts at the end of
/// controlled work once `end` is recorded, and at `begin` before that: the
/// terminal plateau the detector looks for is the post-work catch-up, not the
/// burn itself.
pub fn quota_series_for_run(
    conn: &Connection,
    run: &ControlledExperimentRun,
    now: UtcTimestamp,
) -> Result<Vec<(UtcTimestamp, QuotaUsed, ReportedResolution)>, Error> {
    let Some(account_id) = account_id_by_identity(conn, run.provider.as_str(), &run.account)?
    else {
        return Ok(Vec::new());
    };
    let series_start = run.ended_at.unwrap_or(run.started_at);
    let mut statement = conn
        .prepare(
            "SELECT mo.received_at, mw.quota_used_ppm, mw.reported_resolution_ppm
             FROM meter_window mw
             JOIN meter_observation mo ON mo.id = mw.observation_id
             WHERE mo.account_id = ?1
               AND mw.semantic_key = ?2
               AND mo.received_at >= ?3
               AND mo.received_at <= ?4
             ORDER BY mo.received_at, mw.id",
        )
        .map_err(|e| Error::Store(format!("cannot prepare the run quota series: {e}")))?;
    let rows = statement
        .query_map(
            params![
                account_id.value(),
                run.window_semantic_key.as_str(),
                series_start.unix_nanos(),
                now.unix_nanos(),
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .map_err(|e| Error::Store(format!("cannot read the run quota series: {e}")))?;
    let mut out = Vec::new();
    for row in rows {
        let (at, quota_ppm, resolution_ppm) =
            row.map_err(|e| Error::Store(format!("cannot read a series row: {e}")))?;
        let quota = i32::try_from(quota_ppm)
            .ok()
            .and_then(QuotaFractionPpm::new)
            .map(QuotaUsed::new);
        let resolution = i32::try_from(resolution_ppm)
            .ok()
            .and_then(QuotaFractionPpm::new)
            .and_then(ReportedResolution::new);
        match (quota, resolution) {
            (Some(quota), Some(resolution)) => {
                out.push((UtcTimestamp::from_unix_nanos(at), quota, resolution));
            }
            _ => {
                return Err(Error::Store("stored run series quota is invalid".into()));
            }
        }
    }
    Ok(out)
}

/// Whether the run's meter currently satisfies the settlement criterion: the
/// conservative provider-lag policy for the baseline resolution over the
/// post-boundary series. A fresh detection on every call, never a stored
/// flag, so `end` cannot declare it and a later sample can change the answer
/// without rewriting the experiment.
pub fn terminal_settlement_for_run(
    conn: &Connection,
    run: &ControlledExperimentRun,
    now: UtcTimestamp,
) -> Result<SettlementOutcome, Error> {
    let series_start = run.ended_at.unwrap_or(run.started_at);
    let points = quota_series_for_run(conn, run, now)?;
    let observations = points
        .into_iter()
        .map(|(at, quota_used, reported_resolution)| {
            SettlementMeterObservation::new(at, quota_used, reported_resolution)
        })
        .collect::<Vec<_>>();
    let series = SettlementObservationSeries::complete(series_start, observations);
    let policy = SettlementPolicy::conservative_default(run.baseline_resolution);
    Ok(detect_settlement(
        &series,
        &policy,
        SettlementRole::Terminal,
    ))
}

/// Assembles everything `status` reports for one run at one instant.
pub fn status_for_run(
    conn: &Connection,
    run: &ControlledExperimentRun,
    now: UtcTimestamp,
) -> Result<ControlledRunStatus, Error> {
    let elapsed_nanos =
        u64::try_from((now.unix_nanos() - run.started_at.unix_nanos()).max(0)).unwrap_or(u64::MAX);
    Ok(ControlledRunStatus {
        phase: run.phase(),
        elapsed: MonotonicDuration::from_nanos(elapsed_nanos),
        samples_since_baseline: count_samples_since_baseline(conn, run, now)?,
        settlement: terminal_settlement_for_run(conn, run, now)?,
    })
}

/// The idle plateau period `begin` asserts for the pre-burn check: the earliest
/// stored observation in the trailing stable run ending at the baseline, where
/// stable means within `tolerance_ppm` of the baseline quota. With no earlier
/// stable observation the plateau is degenerate at the baseline instant, which
/// keeps the pre-burn check vacuous rather than inventing a period nobody
/// observed.
pub fn baseline_plateau_start_for(
    conn: &Connection,
    provider: &str,
    account: &str,
    window_key: &WindowSemanticKey,
    baseline_quota: QuotaUsed,
    baseline_at: UtcTimestamp,
    tolerance_ppm: u32,
) -> Result<UtcTimestamp, Error> {
    let Some(account_id) = account_id_by_identity(conn, provider, account)? else {
        return Ok(baseline_at);
    };
    let mut statement = conn
        .prepare(
            "SELECT mo.received_at, mw.quota_used_ppm
             FROM meter_window mw
             JOIN meter_observation mo ON mo.id = mw.observation_id
             WHERE mo.account_id = ?1
               AND mw.semantic_key = ?2
               AND mo.received_at <= ?3
             ORDER BY mo.received_at DESC, mw.id DESC",
        )
        .map_err(|e| Error::Store(format!("cannot prepare the plateau scan: {e}")))?;
    let rows = statement
        .query_map(
            params![
                account_id.value(),
                window_key.as_str(),
                baseline_at.unix_nanos(),
            ],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .map_err(|e| Error::Store(format!("cannot read the plateau scan: {e}")))?;
    let baseline_ppm = baseline_quota.as_ppm().get();
    let mut start = baseline_at;
    for row in rows {
        let (at, quota_ppm) =
            row.map_err(|e| Error::Store(format!("cannot read a plateau row: {e}")))?;
        let quota_ppm = u32::try_from(quota_ppm)
            .map_err(|_| Error::Store("stored plateau quota is outside the u32 range".into()))?;
        if baseline_ppm.abs_diff(quota_ppm) <= tolerance_ppm {
            start = UtcTimestamp::from_unix_nanos(at);
        } else {
            break;
        }
    }
    Ok(start)
}

/// One locally known session marked against the experiment's account inside
/// the experiment window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlappingSessionRef {
    pub session_source: String,
    pub session_native: String,
}

impl OverlappingSessionRef {
    /// The session identity as one display string, `source/native`.
    pub fn label(&self) -> String {
        format!("{}/{}", self.session_source, self.session_native)
    }
}

/// Every distinct session marked against the run's account inside the
/// experiment window (from `begin` to the recorded `end`, else to now),
/// oldest first. Reads the marker timeline for the same account, so the
/// overlapping-session check reports which sessions overlapped.
pub fn overlapping_sessions_for_run(
    conn: &Connection,
    run: &ControlledExperimentRun,
    now: UtcTimestamp,
) -> Result<Vec<OverlappingSessionRef>, Error> {
    let window_end = run.ended_at.unwrap_or(now);
    let mut statement = conn
        .prepare(
            "SELECT DISTINCT session_source, session_native
             FROM session_account_marker
             WHERE logical_account = ?1
               AND observed_at >= ?2
               AND observed_at <= ?3
             ORDER BY session_source, session_native",
        )
        .map_err(|e| Error::Store(format!("cannot prepare the overlap scan: {e}")))?;
    let rows = statement
        .query_map(
            params![
                run.account,
                run.started_at.unix_nanos(),
                window_end.unix_nanos(),
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|e| Error::Store(format!("cannot read the overlap scan: {e}")))?;
    let mut out = Vec::new();
    for row in rows {
        let (source, native) =
            row.map_err(|e| Error::Store(format!("cannot read an overlap row: {e}")))?;
        out.push(OverlappingSessionRef {
            session_source: source,
            session_native: native,
        });
    }
    Ok(out)
}

/// The stored quota series for the run's account and target window between two
/// instants, oldest first. The narrow helper behind both the plateau assembly
/// and the drift tail.
fn meter_points_between(
    conn: &Connection,
    run: &ControlledExperimentRun,
    from: UtcTimestamp,
    to: UtcTimestamp,
) -> Result<Vec<ContaminationMeterPoint>, Error> {
    Ok(quota_series_between_raw(conn, run, from, to)?
        .into_iter()
        .map(|(at, quota_used, _)| ContaminationMeterPoint::new(at, quota_used))
        .collect())
}

/// The raw stored quota series between two instants, oldest first.
fn quota_series_between_raw(
    conn: &Connection,
    run: &ControlledExperimentRun,
    from: UtcTimestamp,
    to: UtcTimestamp,
) -> Result<Vec<(UtcTimestamp, QuotaUsed, ReportedResolution)>, Error> {
    let Some(account_id) = account_id_by_identity(conn, run.provider.as_str(), &run.account)?
    else {
        return Ok(Vec::new());
    };
    let mut statement = conn
        .prepare(
            "SELECT mo.received_at, mw.quota_used_ppm, mw.reported_resolution_ppm
             FROM meter_window mw
             JOIN meter_observation mo ON mo.id = mw.observation_id
             WHERE mo.account_id = ?1
               AND mw.semantic_key = ?2
               AND mo.received_at >= ?3
               AND mo.received_at <= ?4
             ORDER BY mo.received_at, mw.id",
        )
        .map_err(|e| Error::Store(format!("cannot prepare the run quota window: {e}")))?;
    let rows = statement
        .query_map(
            params![
                account_id.value(),
                run.window_semantic_key.as_str(),
                from.unix_nanos(),
                to.unix_nanos(),
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .map_err(|e| Error::Store(format!("cannot read the run quota window: {e}")))?;
    let mut out = Vec::new();
    for row in rows {
        let (at, quota_ppm, resolution_ppm) =
            row.map_err(|e| Error::Store(format!("cannot read a window row: {e}")))?;
        let quota = i32::try_from(quota_ppm)
            .ok()
            .and_then(QuotaFractionPpm::new)
            .map(QuotaUsed::new);
        let resolution = i32::try_from(resolution_ppm)
            .ok()
            .and_then(QuotaFractionPpm::new)
            .and_then(ReportedResolution::new);
        match (quota, resolution) {
            (Some(quota), Some(resolution)) => {
                out.push((UtcTimestamp::from_unix_nanos(at), quota, resolution));
            }
            _ => {
                return Err(Error::Store("stored run window quota is invalid".into()));
            }
        }
    }
    Ok(out)
}

/// Computes all four contamination signals for one stored run at one instant.
///
/// The thresholds and the plateau period are read from the run row (`begin`
/// recorded them); only the evidence is fresh. `local_credits` is the
/// caller-computed locally attributed total for the controlled window in the
/// experiment's cost model: the usage-to-credits pipeline that produces it is
/// a later fit bead's work, and this function takes the total rather than
/// inventing one.
pub fn evaluate_contamination_for_run(
    conn: &Connection,
    run: &ControlledExperimentRun,
    local_credits: Credits,
    now: UtcTimestamp,
) -> Result<ContaminationVerdict, Error> {
    let pre_burn_series =
        meter_points_between(conn, run, run.baseline_plateau_started_at, run.started_at)?;
    let series_start = run.ended_at.unwrap_or(run.started_at);
    let post_series = meter_points_between(conn, run, series_start, now)?;
    let controlled_end = meter_points_between(conn, run, run.started_at, now)?
        .last()
        .copied()
        .map(|point| point.quota_used())
        .unwrap_or(run.baseline_quota_used);
    let mut marker_statement = conn
        .prepare(
            "SELECT session_source, session_native, logical_account, observed_at
             FROM session_account_marker
             ORDER BY observed_at, id",
        )
        .map_err(|e| Error::Store(format!("cannot prepare the marker timeline: {e}")))?;
    let marker_rows = marker_statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(|e| Error::Store(format!("cannot read the marker timeline: {e}")))?;
    let mut markers = Vec::new();
    for row in marker_rows {
        let (source, native, account, at) =
            row.map_err(|e| Error::Store(format!("cannot read a marker row: {e}")))?;
        markers.push(ContaminationMarkerPoint::new(
            source,
            native,
            account,
            UtcTimestamp::from_unix_nanos(at),
        ));
    }
    let inputs = ContaminationInputs {
        experiment_account: &run.account,
        baseline_plateau_started_at: run.baseline_plateau_started_at,
        started_at: run.started_at,
        ended_at: run.ended_at,
        evaluated_at: now,
        pre_burn_series: &pre_burn_series,
        post_series: &post_series,
        controlled_meter_start: run.baseline_quota_used,
        controlled_meter_end: controlled_end,
        local_credits_delta: local_credits,
        markers: &markers,
    };
    Ok(evaluate_contamination(
        &inputs,
        &run.contamination_thresholds,
    ))
}

/// The store-level activation gate: evaluates the run and refuses with the
/// named signal when contaminated. A contaminated candidate is never
/// activatable; a contaminated fit that still publishes must carry the
/// verdict's explicit mark instead.
pub fn refuse_activation_for_contaminated_run(
    conn: &Connection,
    run: &ControlledExperimentRun,
    local_credits: Credits,
    now: UtcTimestamp,
) -> Result<(), Error> {
    let verdict = evaluate_contamination_for_run(conn, run, local_credits, now)?;
    require_uncontaminated_for_activation(&verdict).map_err(|refusal| {
        Error::ThresholdNotMet(format!(
            "controlled experiment '{}' is contaminated: {refusal}",
            run.id.as_str()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::provenance::CostModelId;
    use crate::domain::time::{FakeClock, MonotonicDuration};
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::store::cost_model::{
        anthropic_claude_messages_incomplete_v1, anthropic_claude_messages_v1,
    };
    use crate::store::migrate::run_migrations;
    use crate::store::migrations::registry;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-calibrate-controlled-test-{}-{suffix}",
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

    fn migrated() -> (ScratchDir, Connection) {
        let scratch = ScratchDir::new();
        let mut conn = open(
            &scratch.path().join("controlled.db"),
            AccessMode::ReadWrite,
            &PragmaPolicy {
                busy_timeout: MonotonicDuration::from_millis(1_000),
            },
        )
        .expect("scratch database must open");
        run_migrations(
            &mut conn,
            &registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(1_000)),
        )
        .expect("migrations must run");
        (scratch, conn)
    }

    /// Inserts the thinnest meter chain a baseline observation needs: account,
    /// run, policy snapshot, attempt, evidence, observation and one window for
    /// `semantic_key` at `received_at` with `quota_ppm`. Returns the
    /// observation row id and its received instant.
    fn insert_meter_chain(
        conn: &Connection,
        account: &str,
        semantic_key: &str,
        received_at: UtcTimestamp,
        quota_ppm: i64,
    ) -> ObservationRowId {
        let account_id: i64 = conn
            .query_row(
                "INSERT INTO account (logical_name, provider_key, first_observed_at, last_observed_at)
                 VALUES (?1, 'anthropic', ?2, ?2)
                 ON CONFLICT (provider_key, logical_name) DO UPDATE SET
                     last_observed_at = MAX(last_observed_at, excluded.last_observed_at)
                 RETURNING id",
                params![account, received_at.unix_nanos()],
                |row| row.get(0),
            )
            .expect("account insert must work");
        let run_id: i64 = conn
            .query_row(
                "INSERT INTO sample_run (trigger, started_at, ended_at, aub_version, configuration_fingerprint)
                 VALUES ('manual', ?1, NULL, 'test', 'fp') RETURNING id",
                params![received_at.unix_nanos()],
                |row| row.get(0),
            )
            .expect("sample run insert must work");
        let snapshot_id: i64 = conn
            .query_row(
                "INSERT INTO sampling_policy_snapshot (
                    account_id, effective_at, ordinary_cadence_nanos, freshness_horizon_nanos,
                    reset_edge_policy, retry_backoff_policy, command_budget_nanos, policy_algorithm_version
                 ) VALUES (?1, ?2, 3600000000000, 300000000000, 'lead-60s', 'none', 10000000000, 'v1')
                 RETURNING id",
                params![account_id, received_at.unix_nanos()],
                |row| row.get(0),
            )
            .expect("policy snapshot insert must work");
        let attempt_id: i64 = conn
            .query_row(
                "INSERT INTO meter_attempt (
                    run_id, account_id, provider, request_started_at, policy_snapshot_id,
                    due_at, due_reason, provider_contract_id, meter_semantics_id
                 ) VALUES (?1, ?2, 'anthropic', ?3, ?4, ?3, 'forced_or_manual', 'contract-v1', 'meter-v1')
                 RETURNING id",
                params![run_id, account_id, received_at.unix_nanos(), snapshot_id],
                |row| row.get(0),
            )
            .expect("attempt insert must work");
        let evidence_id: i64 = conn
            .query_row(
                "INSERT INTO meter_response_evidence (
                    attempt_id, response_classification, received_at, evidence_capsule,
                    capsule_schema_version, sanitizer_version, content_hash, capture_truncated
                 ) VALUES (?1, 'success', ?2, 'capsule', 'v1', 'v1', 'hash', 0) RETURNING id",
                params![attempt_id, received_at.unix_nanos()],
                |row| row.get(0),
            )
            .expect("evidence insert must work");
        let observation_id: i64 = conn
            .query_row(
                "INSERT INTO meter_observation (
                    attempt_id, evidence_id, account_id, provider, received_at,
                    measurement_basis, adapter_version, provider_contract_id,
                    meter_semantics_id, normalized_fingerprint
                 ) VALUES (?1, ?2, ?3, 'anthropic', ?4, 'locally_received', 'adapter-v1',
                    'contract-v1', 'meter-v1', 'fingerprint') RETURNING id",
                params![
                    attempt_id,
                    evidence_id,
                    account_id,
                    received_at.unix_nanos()
                ],
                |row| row.get(0),
            )
            .expect("observation insert must work");
        conn.execute(
            "INSERT INTO meter_window (
                observation_id, semantic_key, scope_kind, quota_used_ppm,
                reported_resolution_ppm, quantization, resets_at, reset_state,
                nominal_duration_nanos, is_active, severity
             ) VALUES (?1, ?2, 'account_wide', ?3, 10000, 'exact', ?4, 'known', 18000000000000, 1, 'unknown')",
            params![
                observation_id,
                semantic_key,
                quota_ppm,
                received_at.unix_nanos() + 18_000_000_000_000
            ],
        )
        .expect("window insert must work");
        ObservationRowId::new(observation_id)
    }

    fn begin_fixture(
        observation_id: ObservationRowId,
        received_at: UtcTimestamp,
    ) -> ControlledExperimentRun {
        ControlledExperimentRun {
            id: ControlledExperimentId::new("exp-1"),
            account: "work-a".to_string(),
            provider: ProviderKey::new("anthropic"),
            plan_tier: PlanTier::new("pro-5h"),
            window_semantic_key: WindowSemanticKey::new("five_hour"),
            cost_model_id: CostModelId::new("anthropic-claude-messages-v1"),
            expected_token_kinds: default_expected_token_kinds(),
            baseline_observation_id: observation_id,
            baseline_quota_used: QuotaUsed::new(
                QuotaFractionPpm::new(100_000).expect("valid test quota"),
            ),
            baseline_resolution: ReportedResolution::new(
                QuotaFractionPpm::new(10_000).expect("valid test resolution"),
            )
            .expect("non-zero test resolution"),
            baseline_observed_at: received_at,
            baseline_plateau_started_at: received_at,
            contamination_thresholds: ContaminationThresholds::conservative_default(),
            started_at: received_at,
            ended_at: None,
            exclusivity_assertion: "account work-a reserved for controlled experiment exp-1"
                .to_string(),
        }
    }

    #[test]
    fn begin_row_round_trips_and_reads_back_running() {
        let (_scratch, conn) = migrated();
        let at = UtcTimestamp::from_unix_nanos(1_000_000_000);
        let observation_id = insert_meter_chain(&conn, "work-a", "five_hour", at, 100_000);
        let run = begin_fixture(observation_id, at);
        insert_begin(&conn, &run).expect("begin insert must work");

        let loaded = load_by_experiment_id(&conn, &run.id)
            .expect("load must work")
            .expect("run must exist");
        assert_eq!(loaded, run);
        assert_eq!(loaded.phase(), ControlledRunPhase::Running);
    }

    #[test]
    fn end_records_the_boundary_once_and_leaves_settlement_to_status() {
        let (_scratch, conn) = migrated();
        let at = UtcTimestamp::from_unix_nanos(1_000_000_000);
        let observation_id = insert_meter_chain(&conn, "work-a", "five_hour", at, 100_000);
        let run = begin_fixture(observation_id, at);
        insert_begin(&conn, &run).expect("begin insert must work");

        let ended_at = UtcTimestamp::from_unix_nanos(2_000_000_000);
        record_end(&conn, &run.id, ended_at).expect("first end must work");
        let loaded = load_by_experiment_id(&conn, &run.id)
            .expect("load must work")
            .expect("run must exist");
        assert_eq!(loaded.phase(), ControlledRunPhase::Ended);
        assert_eq!(loaded.ended_at, Some(ended_at));

        let second = record_end(&conn, &run.id, UtcTimestamp::from_unix_nanos(3_000_000_000));
        assert!(
            second.is_err(),
            "a second end must be refused: the boundary is recorded once"
        );
    }

    #[test]
    fn a_complete_cost_model_covers_every_expected_kind_and_an_incomplete_one_is_named() {
        let at = UtcTimestamp::from_unix_nanos(1_000_000_000);
        let complete = anthropic_claude_messages_v1(at);
        let incomplete = anthropic_claude_messages_incomplete_v1(at);
        let expected = default_expected_token_kinds();

        assert!(
            missing_expected_terms(&complete, &expected).is_empty(),
            "the complete seed model must cover all four token kinds"
        );
        assert_eq!(
            missing_expected_terms(&incomplete, &expected),
            vec![TokenKind::CacheWrite],
            "the incomplete seed model must be missing exactly cache_write"
        );
    }

    #[test]
    fn an_unknown_expect_kinds_label_is_refused_rather_than_guessed() {
        let err = parse_expected_token_kinds("input,mystery").unwrap_err();
        assert!(err.to_string().contains("mystery"));
    }

    #[test]
    fn status_reports_phase_elapsed_samples_and_settlement() {
        let (_scratch, conn) = migrated();
        let start = UtcTimestamp::from_unix_nanos(0);
        let observation_id = insert_meter_chain(&conn, "work-a", "five_hour", start, 600_000);
        // Two more settled readings five minutes apart, spanning ten minutes.
        let five_minutes: i64 = 300_000_000_000;
        insert_meter_chain(
            &conn,
            "work-a",
            "five_hour",
            UtcTimestamp::from_unix_nanos(five_minutes),
            600_000,
        );
        insert_meter_chain(
            &conn,
            "work-a",
            "five_hour",
            UtcTimestamp::from_unix_nanos(2 * five_minutes),
            600_000,
        );
        let run = ControlledExperimentRun {
            baseline_quota_used: QuotaUsed::new(
                QuotaFractionPpm::new(600_000).expect("valid test quota"),
            ),
            ..begin_fixture(observation_id, start)
        };
        insert_begin(&conn, &run).expect("begin insert must work");

        let now = UtcTimestamp::from_unix_nanos(2 * five_minutes);
        let status = status_for_run(&conn, &run, now).expect("status must work");
        assert_eq!(status.phase, ControlledRunPhase::Running);
        assert_eq!(
            status.elapsed,
            MonotonicDuration::from_nanos(2 * five_minutes as u64)
        );
        assert_eq!(status.samples_since_baseline, 3);
        assert!(
            status.is_settled(),
            "three stable five-minute readings must satisfy the conservative settlement criterion"
        );
    }

    /// The planted negative for the status test above: the same three readings
    /// with the meter still climbing never settle, so a status that reported
    /// settled here would prove the detector was not consulted.
    #[test]
    fn status_reports_unsettled_while_the_meter_still_moves() {
        let (_scratch, conn) = migrated();
        let start = UtcTimestamp::from_unix_nanos(0);
        let observation_id = insert_meter_chain(&conn, "work-a", "five_hour", start, 100_000);
        let five_minutes: i64 = 300_000_000_000;
        insert_meter_chain(
            &conn,
            "work-a",
            "five_hour",
            UtcTimestamp::from_unix_nanos(five_minutes),
            200_000,
        );
        insert_meter_chain(
            &conn,
            "work-a",
            "five_hour",
            UtcTimestamp::from_unix_nanos(2 * five_minutes),
            300_000,
        );
        let run = begin_fixture(observation_id, start);
        insert_begin(&conn, &run).expect("begin insert must work");

        let now = UtcTimestamp::from_unix_nanos(2 * five_minutes);
        let status = status_for_run(&conn, &run, now).expect("status must work");
        assert_eq!(status.samples_since_baseline, 3);
        assert!(
            !status.is_settled(),
            "a climbing meter must not satisfy the settlement criterion"
        );
    }

    #[test]
    fn direct_rewrites_of_a_begin_row_are_refused() {
        let (_scratch, conn) = migrated();
        let at = UtcTimestamp::from_unix_nanos(1_000_000_000);
        let observation_id = insert_meter_chain(&conn, "work-a", "five_hour", at, 100_000);
        let run = begin_fixture(observation_id, at);
        insert_begin(&conn, &run).expect("begin insert must work");

        let rewrite = conn
            .execute(
                "UPDATE calibration_controlled_run SET plan_tier = 'other' WHERE experiment_id = 'exp-1'",
                [],
            )
            .unwrap_err()
            .to_string();
        assert!(
            rewrite.contains("append-only"),
            "a begin rewrite must be refused, got: {rewrite}"
        );
        let delete = conn
            .execute(
                "DELETE FROM calibration_controlled_run WHERE experiment_id = 'exp-1'",
                [],
            )
            .unwrap_err()
            .to_string();
        assert!(
            delete.contains("append-only"),
            "a begin delete must be refused, got: {delete}"
        );
    }

    #[test]
    fn contamination_thresholds_round_trip_through_the_begin_row() {
        let (_scratch, conn) = migrated();
        let at = UtcTimestamp::from_unix_nanos(1_000_000_000);
        let observation_id = insert_meter_chain(&conn, "work-a", "five_hour", at, 100_000);
        let thresholds = ContaminationThresholds::new(
            "custom-v9",
            1_000,
            2_000,
            MonotonicDuration::from_nanos(7_200_000_000_000),
            5_000,
            Credits::from_micros(42),
        )
        .unwrap();
        let run = ControlledExperimentRun {
            contamination_thresholds: thresholds.clone(),
            baseline_plateau_started_at: at,
            ..begin_fixture(observation_id, at)
        };
        insert_begin(&conn, &run).expect("begin insert must work");
        let loaded = load_by_experiment_id(&conn, &run.id)
            .expect("load must work")
            .expect("run must exist");
        assert_eq!(loaded.contamination_thresholds, thresholds);
        assert_eq!(loaded.baseline_plateau_started_at, at);
    }

    #[test]
    fn plateau_scan_asserts_the_trailing_stable_run() {
        let (_scratch, conn) = migrated();
        let minute: i64 = 60_000_000_000;
        insert_meter_chain(
            &conn,
            "work-a",
            "five_hour",
            UtcTimestamp::from_unix_nanos(0),
            500_000,
        );
        insert_meter_chain(
            &conn,
            "work-a",
            "five_hour",
            UtcTimestamp::from_unix_nanos(minute),
            100_000,
        );
        insert_meter_chain(
            &conn,
            "work-a",
            "five_hour",
            UtcTimestamp::from_unix_nanos(2 * minute),
            100_000,
        );
        let baseline = QuotaUsed::new(QuotaFractionPpm::new(100_000).unwrap());
        let start = baseline_plateau_start_for(
            &conn,
            "anthropic",
            "work-a",
            &WindowSemanticKey::new("five_hour"),
            baseline,
            UtcTimestamp::from_unix_nanos(2 * minute),
            10_000,
        )
        .expect("plateau scan must work");
        assert_eq!(start, UtcTimestamp::from_unix_nanos(minute));
    }

    #[test]
    fn overlap_scan_reports_sessions_marked_against_the_same_account() {
        use crate::domain::ids::{NativeSessionId, SessionId, SourceNamespace};
        use crate::store::session_account_marker::{
            EvidenceDesignation, MarkerSource, NewSessionAccountMarker, insert_marker,
        };
        let (_scratch, conn) = migrated();
        let at = UtcTimestamp::from_unix_nanos(1_000_000_000);
        let observation_id = insert_meter_chain(&conn, "work-a", "five_hour", at, 100_000);
        let run = begin_fixture(observation_id, at);
        insert_begin(&conn, &run).expect("begin insert must work");
        let mark = |source: &str, native: &str, account: &str, observed: i64| {
            insert_marker(
                &conn,
                &NewSessionAccountMarker {
                    session_id: SessionId::new(
                        SourceNamespace::new(source),
                        NativeSessionId::new(native),
                    ),
                    observed_at: UtcTimestamp::from_unix_nanos(observed),
                    source_ordering_key: None,
                    logical_account: account.to_string(),
                    resolved_account_id: None,
                    marker_source: MarkerSource::new("hook"),
                    run_id: None,
                    evidence_designation: EvidenceDesignation::ExplicitLauncherOrHook,
                },
            )
            .expect("marker insert must work");
        };
        mark("claude-code", "sess-other", "work-a", 1_000_000_001);
        mark("codex", "sess-unrelated", "personal", 1_000_000_001);
        let overlapping =
            overlapping_sessions_for_run(&conn, &run, UtcTimestamp::from_unix_nanos(1_000_000_002))
                .expect("overlap scan must work");
        assert_eq!(overlapping.len(), 1);
        assert_eq!(overlapping[0].label(), "claude-code/sess-other");
    }

    #[test]
    fn hidden_traffic_with_flat_local_credits_is_contaminated() {
        let (_scratch, conn) = migrated();
        let start = UtcTimestamp::from_unix_nanos(0);
        let five_minutes: i64 = 300_000_000_000;
        let baseline_id = insert_meter_chain(&conn, "work-a", "five_hour", start, 100_000);
        // Hidden traffic moves the meter while no local work is attributed.
        insert_meter_chain(
            &conn,
            "work-a",
            "five_hour",
            UtcTimestamp::from_unix_nanos(five_minutes),
            200_000,
        );
        insert_meter_chain(
            &conn,
            "work-a",
            "five_hour",
            UtcTimestamp::from_unix_nanos(2 * five_minutes),
            200_000,
        );
        let run = begin_fixture(baseline_id, start);
        insert_begin(&conn, &run).expect("begin insert must work");
        let verdict = evaluate_contamination_for_run(
            &conn,
            &run,
            Credits::from_micros(0),
            UtcTimestamp::from_unix_nanos(2 * five_minutes),
        )
        .expect("evaluation must work");
        assert!(
            !verdict
                .findings_for(
                    crate::calibration::contamination::ContaminationSignal::FlatCreditsWithMeterMovement
                )
                .is_empty(),
            "injected hidden traffic with flat local credits must fire the flat-credits signal"
        );
    }

    #[test]
    fn activation_is_refused_for_a_contaminated_run() {
        let (_scratch, conn) = migrated();
        let start = UtcTimestamp::from_unix_nanos(0);
        let five_minutes: i64 = 300_000_000_000;
        let baseline_id = insert_meter_chain(&conn, "work-a", "five_hour", start, 100_000);
        insert_meter_chain(
            &conn,
            "work-a",
            "five_hour",
            UtcTimestamp::from_unix_nanos(five_minutes),
            200_000,
        );
        let run = begin_fixture(baseline_id, start);
        insert_begin(&conn, &run).expect("begin insert must work");
        let refusal = refuse_activation_for_contaminated_run(
            &conn,
            &run,
            Credits::from_micros(0),
            UtcTimestamp::from_unix_nanos(five_minutes),
        )
        .unwrap_err();
        assert!(
            refusal.to_string().contains("contaminated"),
            "refusal must name contamination, got: {refusal}"
        );
    }
}
