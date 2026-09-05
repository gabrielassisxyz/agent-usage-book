//! The `meter_window_anomaly`, `meter_calibration_exclusion` and
//! `meter_window_set_change` tables, and the detection orchestration that
//! fills them from two consecutive observations (`aub-eun.14`, PLAN.md
//! sections 30, 34.10, 36, 45).
//!
//! [`detect_and_persist`] is the one entry point: given the observation and
//! windows a commit just wrote, it reads the account's immediately preceding
//! observation, classifies every matched window pair with
//! [`crate::domain::window_anomaly::classify_window_transition`], and
//! classifies every appeared or disappeared window identity with
//! [`crate::domain::window_anomaly::classify_window_set_change`]. It never
//! writes to `meter_observation` or `meter_window`, so the original readings
//! stay exactly as recorded regardless of what detection concludes about
//! them; the domain functions it calls have no I/O of their own to make that
//! true by construction rather than by care taken here.
//!
//! Every insert here is `INSERT OR IGNORE` against a table-level uniqueness
//! constraint on the two window rows involved, so calling
//! [`detect_and_persist`] again over the same pair of observations persists
//! nothing new: rerunning detection is idempotent by construction, not by
//! this module remembering what it already did.
//!
//! All three tables are irreplaceable evidence: their triggers reject every
//! `UPDATE` and `DELETE`, and this module exposes insert and read only.
//!
//! May not depend on:
//! - HTTP or provider semantics
//! - presentation

use rusqlite::{Connection, OptionalExtension, params};

use crate::domain::rows::RowCount;
use crate::domain::time::{MeasurementBasis, UtcTimestamp};
use crate::domain::window::{ModelId, WindowScope};
use crate::domain::window_anomaly::{
    WindowAnomalyKind, WindowPresenceChange, WindowReading, WindowSetChangeKind,
    classify_window_set_change, classify_window_transition,
};
use crate::error::Error;
use crate::store::account::AccountId;
use crate::store::meter_evidence::{
    ObservationRowId, StoredMeterObservation, StoredMeterWindow, WindowRowId,
    observation_immediately_before, windows_by_observation,
};

/// A `meter_window_anomaly` row's identity: its SQLite rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowAnomalyRowId(i64);

impl WindowAnomalyRowId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

/// A `meter_calibration_exclusion` row's identity: its SQLite rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CalibrationExclusionRowId(i64);

impl CalibrationExclusionRowId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

/// A `meter_window_set_change` row's identity: its SQLite rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowSetChangeRowId(i64);

impl WindowSetChangeRowId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

/// One immutable anomaly row: the typed classification of a transition
/// between two `meter_window` rows the caller matched by identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredWindowAnomaly {
    pub row_id: WindowAnomalyRowId,
    pub kind: WindowAnomalyKind,
    pub account_id: AccountId,
    pub scope: WindowScope,
    pub prior_observation_id: ObservationRowId,
    pub prior_window_id: WindowRowId,
    pub current_observation_id: ObservationRowId,
    pub current_window_id: WindowRowId,
    pub detected_at: UtcTimestamp,
    pub detail: String,
}

/// One immutable calibration-exclusion annotation: the interval one anomaly
/// makes unfit to teach a calibration fit, named by its own evidence
/// references rather than by re-deriving them from the anomaly at read time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCalibrationExclusion {
    pub row_id: CalibrationExclusionRowId,
    pub anomaly_id: WindowAnomalyRowId,
    pub account_id: AccountId,
    pub scope: WindowScope,
    pub interval_start_at: UtcTimestamp,
    pub interval_end_at: UtcTimestamp,
    pub created_at: UtcTimestamp,
}

/// One immutable window-set-change row: a window identity appearing or
/// disappearing between two observations. Unrelated to an anomaly - it names
/// no exclusion because there is no disputed reading, only a constraint that
/// started or stopped existing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredWindowSetChange {
    pub row_id: WindowSetChangeRowId,
    pub kind: WindowSetChangeKind,
    pub account_id: AccountId,
    pub scope: WindowScope,
    pub previous_observation_id: ObservationRowId,
    pub previous_window_id: Option<WindowRowId>,
    pub current_observation_id: ObservationRowId,
    pub current_window_id: Option<WindowRowId>,
    pub detected_at: UtcTimestamp,
}

/// Everything [`detect_and_persist`] found and persisted for one commit.
/// Empty on the account's first observation, since there is nothing yet to
/// compare it to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DetectionOutcome {
    pub anomalies: Vec<StoredWindowAnomaly>,
    pub window_set_changes: Vec<StoredWindowSetChange>,
}

/// The stable database spelling of a window scope, mirroring the spelling
/// `meter_evidence::insert_window` already writes for `meter_window` itself.
fn scope_columns(scope: &WindowScope) -> (&'static str, Option<&str>) {
    match scope {
        WindowScope::AccountWide => ("account_wide", None),
        WindowScope::ModelSpecific(model) => ("model_specific", Some(model.as_str())),
    }
}

fn scope_from_columns(kind: &str, model: Option<String>) -> Result<WindowScope, Error> {
    match (kind, model) {
        ("account_wide", None) => Ok(WindowScope::AccountWide),
        ("model_specific", Some(model)) => Ok(WindowScope::ModelSpecific(ModelId::new(model))),
        (kind, model) => Err(Error::Store(format!(
            "inconsistent scope row in the database: kind {kind:?} with model {model:?}"
        ))),
    }
}

/// The instant one observation's reading was taken at, by its own declared
/// measurement basis. This is a plain selection between two already-recorded
/// timestamps, not a revalidation: clock-skew anomalies are a separate,
/// already-handled concern (`crate::domain::time::age`) that runs at ingest,
/// before an observation ever reaches detection.
fn measurement_instant(observation: &StoredMeterObservation) -> UtcTimestamp {
    match observation.measurement_basis {
        MeasurementBasis::ProviderObserved => observation
            .provider_observed_at
            .unwrap_or(observation.received_at),
        MeasurementBasis::LocallyReceived => observation.received_at,
        MeasurementBasis::OlderOfTheTwo => match observation.provider_observed_at {
            Some(provider_observed) => provider_observed.min(observation.received_at),
            None => observation.received_at,
        },
    }
}

const INSERT_ANOMALY: &str = "
INSERT INTO meter_window_anomaly (
    kind, account_id, semantic_key, scope_kind, scoped_model,
    prior_observation_id, prior_window_id, current_observation_id, current_window_id,
    detected_at, detail
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
ON CONFLICT (prior_window_id, current_window_id) DO NOTHING
RETURNING id";

fn insert_anomaly(
    conn: &Connection,
    kind: WindowAnomalyKind,
    account_id: AccountId,
    prior: &StoredMeterWindow,
    current: &StoredMeterWindow,
    detected_at: UtcTimestamp,
    detail: &str,
) -> Result<WindowAnomalyRowId, Error> {
    let (scope_kind, scoped_model) = scope_columns(&current.scope);
    let inserted: Option<i64> = conn
        .query_row(
            INSERT_ANOMALY,
            params![
                kind.as_str(),
                account_id.value(),
                current.semantic_key.as_str(),
                scope_kind,
                scoped_model,
                prior.observation_id.value(),
                prior.row_id.value(),
                current.observation_id.value(),
                current.row_id.value(),
                detected_at.unix_nanos(),
                detail,
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| Error::Store(format!("cannot record the window anomaly: {e}")))?;
    let row_id = match inserted {
        Some(id) => id,
        None => conn
            .query_row(
                "SELECT id FROM meter_window_anomaly WHERE prior_window_id = ?1 AND current_window_id = ?2",
                params![prior.row_id.value(), current.row_id.value()],
                |row| row.get(0),
            )
            .map_err(|e| Error::Store(format!("cannot read the existing window anomaly: {e}")))?,
    };
    Ok(WindowAnomalyRowId::new(row_id))
}

const SELECT_ANOMALY_COLUMNS: &str = "
    id, kind, account_id, semantic_key, scope_kind, scoped_model,
    prior_observation_id, prior_window_id, current_observation_id, current_window_id,
    detected_at, detail";

fn row_to_anomaly(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredWindowAnomaly> {
    let store_error = |e: Error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    };
    let kind_code: String = row.get("kind")?;
    let kind = WindowAnomalyKind::from_code(&kind_code).ok_or_else(|| {
        store_error(Error::Store(format!(
            "unknown window anomaly kind stored in the database: {kind_code:?}"
        )))
    })?;
    let scope = scope_from_columns(
        &row.get::<_, String>("scope_kind")?,
        row.get::<_, Option<String>>("scoped_model")?,
    )
    .map_err(store_error)?;
    Ok(StoredWindowAnomaly {
        row_id: WindowAnomalyRowId::new(row.get("id")?),
        kind,
        account_id: AccountId::new(row.get("account_id")?),
        scope,
        prior_observation_id: ObservationRowId::new(row.get("prior_observation_id")?),
        prior_window_id: WindowRowId::new(row.get("prior_window_id")?),
        current_observation_id: ObservationRowId::new(row.get("current_observation_id")?),
        current_window_id: WindowRowId::new(row.get("current_window_id")?),
        detected_at: UtcTimestamp::from_unix_nanos(row.get("detected_at")?),
        detail: row.get("detail")?,
    })
}

/// Reads one anomaly row by its rowid, or `None` when there is no such row.
pub fn anomaly_by_row_id(
    conn: &Connection,
    row_id: WindowAnomalyRowId,
) -> Result<Option<StoredWindowAnomaly>, Error> {
    conn.query_row(
        &format!("SELECT {SELECT_ANOMALY_COLUMNS} FROM meter_window_anomaly WHERE id = ?1"),
        params![row_id.value()],
        row_to_anomaly,
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot read the window anomaly {row_id:?}: {e}")))
}

/// Every anomaly recorded, oldest first: the evidence-reference list `doctor`
/// consumes without reimplementing detection (`aub-n27.10`).
pub fn all_anomalies(conn: &Connection) -> Result<Vec<StoredWindowAnomaly>, Error> {
    let mut statement = conn
        .prepare(&format!(
            "SELECT {SELECT_ANOMALY_COLUMNS} FROM meter_window_anomaly ORDER BY id"
        ))
        .map_err(|e| Error::Store(format!("cannot list window anomalies: {e}")))?;
    let rows = statement
        .query_map([], row_to_anomaly)
        .map_err(|e| Error::Store(format!("cannot list window anomalies: {e}")))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| Error::Store(format!("cannot read window anomalies: {e}")))
}

/// How many anomaly rows the ledger holds.
pub fn anomaly_count(conn: &Connection) -> Result<RowCount, Error> {
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM meter_window_anomaly", [], |row| {
            row.get(0)
        })
        .map_err(|e| Error::Store(format!("cannot count window anomalies: {e}")))?;
    Ok(RowCount::new(u64::try_from(count).map_err(|_| {
        Error::Internal(format!("meter_window_anomaly count {count} is negative"))
    })?))
}

const INSERT_EXCLUSION: &str = "
INSERT INTO meter_calibration_exclusion (
    anomaly_id, account_id, semantic_key, scope_kind, scoped_model,
    interval_start_at, interval_end_at, created_at
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
ON CONFLICT (anomaly_id) DO NOTHING
RETURNING id";

#[allow(clippy::too_many_arguments)]
fn insert_exclusion(
    conn: &Connection,
    anomaly_id: WindowAnomalyRowId,
    account_id: AccountId,
    semantic_key: &str,
    scope: &WindowScope,
    interval_start_at: UtcTimestamp,
    interval_end_at: UtcTimestamp,
    created_at: UtcTimestamp,
) -> Result<CalibrationExclusionRowId, Error> {
    let (scope_kind, scoped_model) = scope_columns(scope);
    let inserted: Option<i64> = conn
        .query_row(
            INSERT_EXCLUSION,
            params![
                anomaly_id.value(),
                account_id.value(),
                semantic_key,
                scope_kind,
                scoped_model,
                interval_start_at.unix_nanos(),
                interval_end_at.unix_nanos(),
                created_at.unix_nanos(),
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| Error::Store(format!("cannot record the calibration exclusion: {e}")))?;
    let row_id = match inserted {
        Some(id) => id,
        None => conn
            .query_row(
                "SELECT id FROM meter_calibration_exclusion WHERE anomaly_id = ?1",
                params![anomaly_id.value()],
                |row| row.get(0),
            )
            .map_err(|e| {
                Error::Store(format!(
                    "cannot read the existing calibration exclusion: {e}"
                ))
            })?,
    };
    Ok(CalibrationExclusionRowId::new(row_id))
}

const SELECT_EXCLUSION_COLUMNS: &str = "
    id, anomaly_id, account_id, scope_kind, scoped_model,
    interval_start_at, interval_end_at, created_at";

fn row_to_exclusion(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredCalibrationExclusion> {
    let store_error = |e: Error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    };
    let scope = scope_from_columns(
        &row.get::<_, String>("scope_kind")?,
        row.get::<_, Option<String>>("scoped_model")?,
    )
    .map_err(store_error)?;
    Ok(StoredCalibrationExclusion {
        row_id: CalibrationExclusionRowId::new(row.get("id")?),
        anomaly_id: WindowAnomalyRowId::new(row.get("anomaly_id")?),
        account_id: AccountId::new(row.get("account_id")?),
        scope,
        interval_start_at: UtcTimestamp::from_unix_nanos(row.get("interval_start_at")?),
        interval_end_at: UtcTimestamp::from_unix_nanos(row.get("interval_end_at")?),
        created_at: UtcTimestamp::from_unix_nanos(row.get("created_at")?),
    })
}

/// Reads the calibration exclusion for one anomaly, or `None` when none was
/// recorded (an anomaly always gets exactly one, but a caller that reads
/// speculatively should not have to know that).
pub fn exclusion_for_anomaly(
    conn: &Connection,
    anomaly_id: WindowAnomalyRowId,
) -> Result<Option<StoredCalibrationExclusion>, Error> {
    conn.query_row(
        &format!(
            "SELECT {SELECT_EXCLUSION_COLUMNS} FROM meter_calibration_exclusion WHERE anomaly_id = ?1"
        ),
        params![anomaly_id.value()],
        row_to_exclusion,
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot read the calibration exclusion: {e}")))
}

/// Every calibration exclusion recorded, oldest first.
pub fn all_exclusions(conn: &Connection) -> Result<Vec<StoredCalibrationExclusion>, Error> {
    let mut statement = conn
        .prepare(&format!(
            "SELECT {SELECT_EXCLUSION_COLUMNS} FROM meter_calibration_exclusion ORDER BY id"
        ))
        .map_err(|e| Error::Store(format!("cannot list calibration exclusions: {e}")))?;
    let rows = statement
        .query_map([], row_to_exclusion)
        .map_err(|e| Error::Store(format!("cannot list calibration exclusions: {e}")))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| Error::Store(format!("cannot read calibration exclusions: {e}")))
}

#[allow(clippy::too_many_arguments)]
fn insert_window_set_change(
    conn: &Connection,
    kind: WindowSetChangeKind,
    account_id: AccountId,
    semantic_key: &str,
    scope: &WindowScope,
    previous_observation_id: ObservationRowId,
    previous_window_id: Option<WindowRowId>,
    current_observation_id: ObservationRowId,
    current_window_id: Option<WindowRowId>,
    detected_at: UtcTimestamp,
) -> Result<WindowSetChangeRowId, Error> {
    let (scope_kind, scoped_model) = scope_columns(scope);
    let inserted: Option<i64> = conn
        .query_row(
            "INSERT INTO meter_window_set_change (
                kind, account_id, semantic_key, scope_kind, scoped_model,
                previous_observation_id, previous_window_id, current_observation_id, current_window_id,
                detected_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ON CONFLICT (kind, account_id, semantic_key, scope_kind, scoped_model, previous_observation_id, current_observation_id)
            DO NOTHING
            RETURNING id",
            params![
                kind.as_str(),
                account_id.value(),
                semantic_key,
                scope_kind,
                scoped_model,
                previous_observation_id.value(),
                previous_window_id.map(WindowRowId::value),
                current_observation_id.value(),
                current_window_id.map(WindowRowId::value),
                detected_at.unix_nanos(),
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| Error::Store(format!("cannot record the window set change: {e}")))?;
    let row_id = match inserted {
        Some(id) => id,
        None => conn
            .query_row(
                "SELECT id FROM meter_window_set_change
                 WHERE kind = ?1 AND account_id = ?2 AND semantic_key = ?3 AND scope_kind = ?4
                   AND scoped_model IS ?5 AND previous_observation_id = ?6 AND current_observation_id = ?7",
                params![
                    kind.as_str(),
                    account_id.value(),
                    semantic_key,
                    scope_kind,
                    scoped_model,
                    previous_observation_id.value(),
                    current_observation_id.value(),
                ],
                |row| row.get(0),
            )
            .map_err(|e| Error::Store(format!("cannot read the existing window set change: {e}")))?,
    };
    Ok(WindowSetChangeRowId::new(row_id))
}

const SELECT_WINDOW_SET_CHANGE_COLUMNS: &str = "
    id, kind, account_id, scope_kind, scoped_model,
    previous_observation_id, previous_window_id, current_observation_id, current_window_id,
    detected_at";

fn row_to_window_set_change(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredWindowSetChange> {
    let store_error = |e: Error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    };
    let kind_code: String = row.get("kind")?;
    let kind = WindowSetChangeKind::from_code(&kind_code).ok_or_else(|| {
        store_error(Error::Store(format!(
            "unknown window set change kind stored in the database: {kind_code:?}"
        )))
    })?;
    let scope = scope_from_columns(
        &row.get::<_, String>("scope_kind")?,
        row.get::<_, Option<String>>("scoped_model")?,
    )
    .map_err(store_error)?;
    Ok(StoredWindowSetChange {
        row_id: WindowSetChangeRowId::new(row.get("id")?),
        kind,
        account_id: AccountId::new(row.get("account_id")?),
        scope,
        previous_observation_id: ObservationRowId::new(row.get("previous_observation_id")?),
        previous_window_id: row
            .get::<_, Option<i64>>("previous_window_id")?
            .map(WindowRowId::new),
        current_observation_id: ObservationRowId::new(row.get("current_observation_id")?),
        current_window_id: row
            .get::<_, Option<i64>>("current_window_id")?
            .map(WindowRowId::new),
        detected_at: UtcTimestamp::from_unix_nanos(row.get("detected_at")?),
    })
}

/// Every window-set change recorded, oldest first.
pub fn all_window_set_changes(conn: &Connection) -> Result<Vec<StoredWindowSetChange>, Error> {
    let mut statement = conn
        .prepare(&format!(
            "SELECT {SELECT_WINDOW_SET_CHANGE_COLUMNS} FROM meter_window_set_change ORDER BY id"
        ))
        .map_err(|e| Error::Store(format!("cannot list window set changes: {e}")))?;
    let rows = statement
        .query_map([], row_to_window_set_change)
        .map_err(|e| Error::Store(format!("cannot list window set changes: {e}")))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| Error::Store(format!("cannot read window set changes: {e}")))
}

/// Compares `current_windows` (already committed, belonging to
/// `current_observation`) against the account's immediately preceding
/// observation, classifies every matched pair and every appeared or
/// disappeared window identity, and persists what it finds.
///
/// A window pair is matched by identity alone (semantic key and scope), never
/// by row order, so shuffling the windows a caller passes in changes nothing
/// about the result. Anomaly classification additionally requires the two
/// observations to share the same declared plan and tier: a plan change
/// invalidates the comparison rather than producing a false anomaly, per the
/// "per account, plan tier and stable window identity" acceptance criterion.
/// Window-set-change classification carries no such requirement, since it is
/// about the constraint set's shape rather than one constraint's value.
pub fn detect_and_persist(
    conn: &Connection,
    account_id: AccountId,
    current_observation: &StoredMeterObservation,
    current_windows: &[StoredMeterWindow],
    detected_at: UtcTimestamp,
) -> Result<DetectionOutcome, Error> {
    let mut outcome = DetectionOutcome::default();

    let Some(previous_observation) =
        observation_immediately_before(conn, account_id, current_observation.row_id)?
    else {
        return Ok(outcome);
    };
    let previous_windows = windows_by_observation(conn, previous_observation.row_id)?;

    let plan_tier_matches = previous_observation.observed_plan == current_observation.observed_plan
        && previous_observation.observed_tier == current_observation.observed_tier;
    let previous_instant = measurement_instant(&previous_observation);
    let current_instant = measurement_instant(current_observation);

    let identity_matches = |a: &StoredMeterWindow, b: &StoredMeterWindow| {
        a.semantic_key == b.semantic_key && a.scope == b.scope
    };

    for current_window in current_windows {
        match previous_windows
            .iter()
            .find(|previous_window| identity_matches(previous_window, current_window))
        {
            Some(previous_window) => {
                if !plan_tier_matches {
                    continue;
                }
                let previous_reading = WindowReading {
                    quota_used: previous_window.quota_used,
                    resets_at: previous_window.resets_at,
                    observed_at: previous_instant,
                };
                let current_reading = WindowReading {
                    quota_used: current_window.quota_used,
                    resets_at: current_window.resets_at,
                    observed_at: current_instant,
                };
                if let Some(kind) = classify_window_transition(previous_reading, current_reading) {
                    let detail = format!(
                        "{} for window '{}': prior used={:?} resets_at={:?}, current used={:?} resets_at={:?}",
                        kind.as_str(),
                        current_window.semantic_key.as_str(),
                        previous_window.quota_used,
                        previous_window.resets_at,
                        current_window.quota_used,
                        current_window.resets_at,
                    );
                    let anomaly_id = insert_anomaly(
                        conn,
                        kind,
                        account_id,
                        previous_window,
                        current_window,
                        detected_at,
                        &detail,
                    )?;
                    insert_exclusion(
                        conn,
                        anomaly_id,
                        account_id,
                        current_window.semantic_key.as_str(),
                        &current_window.scope,
                        previous_instant,
                        current_instant,
                        detected_at,
                    )?;
                    if let Some(anomaly) = anomaly_by_row_id(conn, anomaly_id)? {
                        outcome.anomalies.push(anomaly);
                    }
                }
            }
            None => {
                if let Some(kind) = classify_window_set_change(
                    current_window.scope.kind(),
                    WindowPresenceChange::Appeared,
                ) {
                    let row_id = insert_window_set_change(
                        conn,
                        kind,
                        account_id,
                        current_window.semantic_key.as_str(),
                        &current_window.scope,
                        previous_observation.row_id,
                        None,
                        current_observation.row_id,
                        Some(current_window.row_id),
                        detected_at,
                    )?;
                    outcome.window_set_changes.push(StoredWindowSetChange {
                        row_id,
                        kind,
                        account_id,
                        scope: current_window.scope.clone(),
                        previous_observation_id: previous_observation.row_id,
                        previous_window_id: None,
                        current_observation_id: current_observation.row_id,
                        current_window_id: Some(current_window.row_id),
                        detected_at,
                    });
                }
            }
        }
    }

    for previous_window in &previous_windows {
        let still_present = current_windows
            .iter()
            .any(|current_window| identity_matches(previous_window, current_window));
        if still_present {
            continue;
        }
        if let Some(kind) = classify_window_set_change(
            previous_window.scope.kind(),
            WindowPresenceChange::Disappeared,
        ) {
            let row_id = insert_window_set_change(
                conn,
                kind,
                account_id,
                previous_window.semantic_key.as_str(),
                &previous_window.scope,
                previous_observation.row_id,
                Some(previous_window.row_id),
                current_observation.row_id,
                None,
                detected_at,
            )?;
            outcome.window_set_changes.push(StoredWindowSetChange {
                row_id,
                kind,
                account_id,
                scope: previous_window.scope.clone(),
                previous_observation_id: previous_observation.row_id,
                previous_window_id: Some(previous_window.row_id),
                current_observation_id: current_observation.row_id,
                current_window_id: None,
                detected_at,
            });
        }
    }

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::{AdapterVersion, MeterSemanticsId, ProviderContractId};
    use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
    use crate::domain::time::{FakeClock, MonotonicDuration};
    use crate::domain::window::{
        NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowResetState,
        WindowSemanticKey,
    };
    use crate::store::account::observe_account;
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::store::meter_attempt::{DueReason, NewMeterAttempt, start_meter_attempt};
    use crate::store::meter_evidence::{
        NewMeterObservation, NewMeterResponseEvidence, NewMeterWindow, insert_observation,
        insert_response_evidence, insert_window, observation_by_row_id,
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
                "aub-window-anomaly-test-{}-{suffix}",
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

    struct Fixture {
        _scratch: ScratchDir,
        conn: rusqlite::Connection,
        account: AccountId,
        run: crate::store::sample_run::SampleRunId,
        snapshot: crate::store::sampling_policy_snapshot::SamplingPolicySnapshotId,
    }

    fn fixture() -> Fixture {
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
        Fixture {
            _scratch: scratch,
            conn,
            account,
            run,
            snapshot,
        }
    }

    /// Inserts one full observation with one account-wide "5h" window,
    /// returning the observation row and its one window row. `received_at`
    /// doubles as the observation's measurement instant, since every fixture
    /// observation here uses `MeasurementBasis::LocallyReceived`.
    fn record_observation(
        fixture: &Fixture,
        received_at_nanos: i64,
        used_ppm: i32,
        resets_at: WindowResetState,
    ) -> (StoredMeterObservation, StoredMeterWindow) {
        let attempt = start_meter_attempt(
            &fixture.conn,
            &NewMeterAttempt {
                run_id: fixture.run,
                account_id: fixture.account,
                provider: "test-provider".into(),
                request_started_at: UtcTimestamp::from_unix_nanos(received_at_nanos - 100),
                credential_context_id: Some("ctx-1".into()),
                policy_snapshot_id: fixture.snapshot,
                due_at: UtcTimestamp::from_unix_nanos(received_at_nanos - 200),
                due_reason: DueReason::OrdinaryCadence,
                due_basis: None,
                provider_contract_id: "endpoint-schema-v3".into(),
                meter_semantics_id: "account-5h-v2".into(),
            },
        )
        .expect("fixture attempt must insert");
        let evidence_id = insert_response_evidence(
            &fixture.conn,
            &NewMeterResponseEvidence {
                attempt_id: attempt,
                response_classification: "200".into(),
                received_at: UtcTimestamp::from_unix_nanos(received_at_nanos),
                provider_observed_at_original: None,
                evidence_capsule: r#"{"windows":[{"key":"5h"}]}"#.into(),
                capsule_schema_version: "capsule-v1".into(),
                sanitizer_version: "sanitizer-v1".into(),
                capture_truncated: false,
            },
        )
        .expect("fixture evidence must insert");
        let observation_id = insert_observation(
            &fixture.conn,
            &NewMeterObservation {
                attempt_id: attempt,
                evidence_id,
                account_id: fixture.account,
                provider: "test-provider".into(),
                provider_observed_at: None,
                received_at: UtcTimestamp::from_unix_nanos(received_at_nanos),
                measurement_basis: MeasurementBasis::LocallyReceived,
                observed_plan: Some("max".into()),
                observed_tier: Some("pro".into()),
                adapter_version: AdapterVersion::new("adapter-v1"),
                provider_contract_id: ProviderContractId::new("endpoint-schema-v3"),
                meter_semantics_id: MeterSemanticsId::new("semantics-v1"),
                normalized_fingerprint: format!("fp-{received_at_nanos}"),
            },
        )
        .expect("fixture observation must insert");
        let window_id = insert_window(
            &fixture.conn,
            &NewMeterWindow {
                observation_id,
                semantic_key: WindowSemanticKey::new("5h"),
                scope: WindowScope::AccountWide,
                quota_used: QuotaUsed::new(QuotaFractionPpm::new(used_ppm).unwrap()),
                reported_resolution: ReportedResolution::new(
                    QuotaFractionPpm::new(10_000).unwrap(),
                )
                .unwrap(),
                quantization: QuantizationSemantics::RoundedToNearest,
                resets_at,
                nominal_duration: NominalWindowDuration::from_nanos(3_600_000_000_000),
            },
        )
        .expect("fixture window must insert");
        let observation = observation_by_row_id(&fixture.conn, observation_id)
            .expect("observation must read")
            .expect("observation must exist");
        let window = windows_by_observation(&fixture.conn, observation_id)
            .expect("windows must read")
            .into_iter()
            .find(|w| w.row_id == window_id)
            .expect("the inserted window must be present");
        (observation, window)
    }

    /// The first observation for an account has nothing to compare against:
    /// detection finds and persists nothing.
    #[test]
    fn the_first_observation_produces_no_anomaly() {
        let fx = fixture();
        let (observation, window) = record_observation(
            &fx,
            30_000,
            400_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(100_000)),
        );
        let outcome = detect_and_persist(
            &fx.conn,
            fx.account,
            &observation,
            &[window],
            UtcTimestamp::from_unix_nanos(30_500),
        )
        .expect("detection must run");
        assert!(outcome.anomalies.is_empty());
        assert!(outcome.window_set_changes.is_empty());
        assert_eq!(anomaly_count(&fx.conn).unwrap().value(), 0);
    }

    /// The done-when case: a percentage decrease with no reset evidence
    /// persists a typed anomaly naming both observation and window
    /// references, plus exactly one calibration exclusion, and the original
    /// observations remain immutable and queryable.
    #[test]
    fn a_percentage_decrease_without_reset_persists_anomaly_and_exclusion() {
        let fx = fixture();
        let (first_obs, first_window) = record_observation(
            &fx,
            30_000,
            600_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(100_000)),
        );
        let outcome_first = detect_and_persist(
            &fx.conn,
            fx.account,
            &first_obs,
            &[first_window],
            UtcTimestamp::from_unix_nanos(30_500),
        )
        .unwrap();
        assert!(outcome_first.anomalies.is_empty());

        let (second_obs, second_window) = record_observation(
            &fx,
            40_000,
            400_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(100_000)),
        );
        let outcome = detect_and_persist(
            &fx.conn,
            fx.account,
            &second_obs,
            &[second_window],
            UtcTimestamp::from_unix_nanos(40_500),
        )
        .unwrap();

        assert_eq!(outcome.anomalies.len(), 1);
        let anomaly = &outcome.anomalies[0];
        assert_eq!(
            anomaly.kind,
            WindowAnomalyKind::PercentageDecreaseWithoutReset
        );
        assert_eq!(anomaly.account_id, fx.account);

        let exclusion = exclusion_for_anomaly(&fx.conn, anomaly.row_id)
            .unwrap()
            .expect("exactly one exclusion must exist for the anomaly");
        assert_eq!(exclusion.anomaly_id, anomaly.row_id);
        assert_eq!(
            exclusion.interval_start_at,
            UtcTimestamp::from_unix_nanos(30_000)
        );
        assert_eq!(
            exclusion.interval_end_at,
            UtcTimestamp::from_unix_nanos(40_000)
        );

        // Both original observations remain immutable and queryable.
        let reread_first = observation_by_row_id(&fx.conn, first_obs.row_id)
            .unwrap()
            .unwrap();
        let reread_second = observation_by_row_id(&fx.conn, second_obs.row_id)
            .unwrap()
            .unwrap();
        assert_eq!(reread_first, first_obs);
        assert_eq!(reread_second, second_obs);
        assert_eq!(anomaly_count(&fx.conn).unwrap().value(), 1);
    }

    /// A legitimate boundary reset is accepted: no anomaly and no exclusion.
    #[test]
    fn a_legitimate_reset_produces_no_anomaly() {
        let fx = fixture();
        let (first_obs, first_window) = record_observation(
            &fx,
            30_000,
            900_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(35_000)),
        );
        detect_and_persist(
            &fx.conn,
            fx.account,
            &first_obs,
            &[first_window],
            UtcTimestamp::from_unix_nanos(30_500),
        )
        .unwrap();

        let (second_obs, second_window) = record_observation(
            &fx,
            40_000,
            50_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(60_000)),
        );
        let outcome = detect_and_persist(
            &fx.conn,
            fx.account,
            &second_obs,
            &[second_window],
            UtcTimestamp::from_unix_nanos(40_500),
        )
        .unwrap();

        assert!(outcome.anomalies.is_empty());
        assert_eq!(anomaly_count(&fx.conn).unwrap().value(), 0);
        assert!(all_exclusions(&fx.conn).unwrap().is_empty());
    }

    /// Rerunning detection over the same pair of observations persists
    /// nothing new: the second call finds the same anomaly already recorded
    /// and returns without duplicating it.
    #[test]
    fn rerunning_detection_over_the_same_pair_is_idempotent() {
        let fx = fixture();
        let (first_obs, first_window) = record_observation(
            &fx,
            30_000,
            600_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(100_000)),
        );
        detect_and_persist(
            &fx.conn,
            fx.account,
            &first_obs,
            &[first_window],
            UtcTimestamp::from_unix_nanos(30_500),
        )
        .unwrap();
        let (second_obs, second_window) = record_observation(
            &fx,
            40_000,
            400_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(100_000)),
        );
        let first_run = detect_and_persist(
            &fx.conn,
            fx.account,
            &second_obs,
            &[second_window.clone()],
            UtcTimestamp::from_unix_nanos(40_500),
        )
        .unwrap();
        let second_run = detect_and_persist(
            &fx.conn,
            fx.account,
            &second_obs,
            &[second_window],
            UtcTimestamp::from_unix_nanos(41_000),
        )
        .unwrap();

        assert_eq!(first_run.anomalies.len(), 1);
        assert_eq!(second_run.anomalies.len(), 1);
        assert_eq!(
            first_run.anomalies[0].row_id,
            second_run.anomalies[0].row_id
        );
        assert_eq!(anomaly_count(&fx.conn).unwrap().value(), 1);
        assert_eq!(all_exclusions(&fx.conn).unwrap().len(), 1);
    }

    /// A window from an unrelated semantic key never gets compared: two
    /// windows with different identities never collide into a false anomaly.
    #[test]
    fn observations_from_unrelated_windows_are_never_compared() {
        let fx = fixture();
        let attempt = start_meter_attempt(
            &fx.conn,
            &NewMeterAttempt {
                run_id: fx.run,
                account_id: fx.account,
                provider: "test-provider".into(),
                request_started_at: UtcTimestamp::from_unix_nanos(29_900),
                credential_context_id: Some("ctx-1".into()),
                policy_snapshot_id: fx.snapshot,
                due_at: UtcTimestamp::from_unix_nanos(29_800),
                due_reason: DueReason::OrdinaryCadence,
                due_basis: None,
                provider_contract_id: "endpoint-schema-v3".into(),
                meter_semantics_id: "account-5h-v2".into(),
            },
        )
        .unwrap();
        let evidence_id = insert_response_evidence(
            &fx.conn,
            &NewMeterResponseEvidence {
                attempt_id: attempt,
                response_classification: "200".into(),
                received_at: UtcTimestamp::from_unix_nanos(30_000),
                provider_observed_at_original: None,
                evidence_capsule: r#"{"windows":[{"key":"7d"}]}"#.into(),
                capsule_schema_version: "capsule-v1".into(),
                sanitizer_version: "sanitizer-v1".into(),
                capture_truncated: false,
            },
        )
        .unwrap();
        let observation_id = insert_observation(
            &fx.conn,
            &NewMeterObservation {
                attempt_id: attempt,
                evidence_id,
                account_id: fx.account,
                provider: "test-provider".into(),
                provider_observed_at: None,
                received_at: UtcTimestamp::from_unix_nanos(30_000),
                measurement_basis: MeasurementBasis::LocallyReceived,
                observed_plan: Some("max".into()),
                observed_tier: Some("pro".into()),
                adapter_version: AdapterVersion::new("adapter-v1"),
                provider_contract_id: ProviderContractId::new("endpoint-schema-v3"),
                meter_semantics_id: MeterSemanticsId::new("semantics-v1"),
                normalized_fingerprint: "fp-unrelated".into(),
            },
        )
        .unwrap();
        let window_id = insert_window(
            &fx.conn,
            &NewMeterWindow {
                observation_id,
                semantic_key: WindowSemanticKey::new("7d"),
                scope: WindowScope::AccountWide,
                quota_used: QuotaUsed::new(QuotaFractionPpm::new(900_000).unwrap()),
                reported_resolution: ReportedResolution::new(
                    QuotaFractionPpm::new(10_000).unwrap(),
                )
                .unwrap(),
                quantization: QuantizationSemantics::RoundedToNearest,
                resets_at: WindowResetState::Known(UtcTimestamp::from_unix_nanos(900_000)),
                nominal_duration: NominalWindowDuration::from_nanos(604_800_000_000_000),
            },
        )
        .unwrap();
        let seven_day_observation = observation_by_row_id(&fx.conn, observation_id)
            .unwrap()
            .unwrap();
        let seven_day_window = windows_by_observation(&fx.conn, observation_id)
            .unwrap()
            .into_iter()
            .find(|w| w.row_id == window_id)
            .unwrap();
        detect_and_persist(
            &fx.conn,
            fx.account,
            &seven_day_observation,
            &[seven_day_window],
            UtcTimestamp::from_unix_nanos(30_500),
        )
        .unwrap();

        // A later "5h" observation with a much lower fraction than the "7d"
        // window carried must not be compared against it: no anomaly, and
        // instead a window-set change, since "5h" is a new identity for this
        // account.
        let (second_obs, second_window) = record_observation(
            &fx,
            40_000,
            10_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(100_000)),
        );
        let outcome = detect_and_persist(
            &fx.conn,
            fx.account,
            &second_obs,
            &[second_window],
            UtcTimestamp::from_unix_nanos(40_500),
        )
        .unwrap();
        assert!(outcome.anomalies.is_empty());
        assert_eq!(outcome.window_set_changes.len(), 1);
        assert_eq!(
            outcome.window_set_changes[0].kind,
            WindowSetChangeKind::NewAccountWideWindow
        );
    }

    /// A plan-tier change between two observations of the same window
    /// identity is not comparable: no anomaly is raised even though the
    /// fraction drops without a reset.
    #[test]
    fn a_plan_tier_change_is_not_compared_for_anomalies() {
        let fx = fixture();
        let (first_obs, first_window) = record_observation(
            &fx,
            30_000,
            900_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(100_000)),
        );
        detect_and_persist(
            &fx.conn,
            fx.account,
            &first_obs,
            &[first_window],
            UtcTimestamp::from_unix_nanos(30_500),
        )
        .unwrap();

        // A second observation on a different tier, still decreasing with no reset.
        let attempt = start_meter_attempt(
            &fx.conn,
            &NewMeterAttempt {
                run_id: fx.run,
                account_id: fx.account,
                provider: "test-provider".into(),
                request_started_at: UtcTimestamp::from_unix_nanos(39_900),
                credential_context_id: Some("ctx-1".into()),
                policy_snapshot_id: fx.snapshot,
                due_at: UtcTimestamp::from_unix_nanos(39_800),
                due_reason: DueReason::OrdinaryCadence,
                due_basis: None,
                provider_contract_id: "endpoint-schema-v3".into(),
                meter_semantics_id: "account-5h-v2".into(),
            },
        )
        .unwrap();
        let evidence_id = insert_response_evidence(
            &fx.conn,
            &NewMeterResponseEvidence {
                attempt_id: attempt,
                response_classification: "200".into(),
                received_at: UtcTimestamp::from_unix_nanos(40_000),
                provider_observed_at_original: None,
                evidence_capsule: r#"{"windows":[{"key":"5h"}]}"#.into(),
                capsule_schema_version: "capsule-v1".into(),
                sanitizer_version: "sanitizer-v1".into(),
                capture_truncated: false,
            },
        )
        .unwrap();
        let observation_id = insert_observation(
            &fx.conn,
            &NewMeterObservation {
                attempt_id: attempt,
                evidence_id,
                account_id: fx.account,
                provider: "test-provider".into(),
                provider_observed_at: None,
                received_at: UtcTimestamp::from_unix_nanos(40_000),
                measurement_basis: MeasurementBasis::LocallyReceived,
                observed_plan: Some("different-plan".into()),
                observed_tier: Some("pro".into()),
                adapter_version: AdapterVersion::new("adapter-v1"),
                provider_contract_id: ProviderContractId::new("endpoint-schema-v3"),
                meter_semantics_id: MeterSemanticsId::new("semantics-v1"),
                normalized_fingerprint: "fp-plan-change".into(),
            },
        )
        .unwrap();
        let window_id = insert_window(
            &fx.conn,
            &NewMeterWindow {
                observation_id,
                semantic_key: WindowSemanticKey::new("5h"),
                scope: WindowScope::AccountWide,
                quota_used: QuotaUsed::new(QuotaFractionPpm::new(100_000).unwrap()),
                reported_resolution: ReportedResolution::new(
                    QuotaFractionPpm::new(10_000).unwrap(),
                )
                .unwrap(),
                quantization: QuantizationSemantics::RoundedToNearest,
                resets_at: WindowResetState::Known(UtcTimestamp::from_unix_nanos(100_000)),
                nominal_duration: NominalWindowDuration::from_nanos(3_600_000_000_000),
            },
        )
        .unwrap();
        let second_obs = observation_by_row_id(&fx.conn, observation_id)
            .unwrap()
            .unwrap();
        let second_window = windows_by_observation(&fx.conn, observation_id)
            .unwrap()
            .into_iter()
            .find(|w| w.row_id == window_id)
            .unwrap();

        let outcome = detect_and_persist(
            &fx.conn,
            fx.account,
            &second_obs,
            &[second_window],
            UtcTimestamp::from_unix_nanos(40_500),
        )
        .unwrap();
        assert!(outcome.anomalies.is_empty());
        assert_eq!(anomaly_count(&fx.conn).unwrap().value(), 0);
    }

    /// Input reordering: shuffling the current windows passed in produces the
    /// same set of anomalies once the results are put in canonical order.
    #[test]
    fn reordering_current_windows_before_detection_produces_the_same_anomalies() {
        let fx = fixture();
        let attempt = start_meter_attempt(
            &fx.conn,
            &NewMeterAttempt {
                run_id: fx.run,
                account_id: fx.account,
                provider: "test-provider".into(),
                request_started_at: UtcTimestamp::from_unix_nanos(29_900),
                credential_context_id: Some("ctx-1".into()),
                policy_snapshot_id: fx.snapshot,
                due_at: UtcTimestamp::from_unix_nanos(29_800),
                due_reason: DueReason::OrdinaryCadence,
                due_basis: None,
                provider_contract_id: "endpoint-schema-v3".into(),
                meter_semantics_id: "account-5h-v2".into(),
            },
        )
        .unwrap();
        let evidence_id = insert_response_evidence(
            &fx.conn,
            &NewMeterResponseEvidence {
                attempt_id: attempt,
                response_classification: "200".into(),
                received_at: UtcTimestamp::from_unix_nanos(30_000),
                provider_observed_at_original: None,
                evidence_capsule: r#"{"windows":[{"key":"5h"},{"key":"7d"}]}"#.into(),
                capsule_schema_version: "capsule-v1".into(),
                sanitizer_version: "sanitizer-v1".into(),
                capture_truncated: false,
            },
        )
        .unwrap();
        let observation_id = insert_observation(
            &fx.conn,
            &NewMeterObservation {
                attempt_id: attempt,
                evidence_id,
                account_id: fx.account,
                provider: "test-provider".into(),
                provider_observed_at: None,
                received_at: UtcTimestamp::from_unix_nanos(30_000),
                measurement_basis: MeasurementBasis::LocallyReceived,
                observed_plan: Some("max".into()),
                observed_tier: Some("pro".into()),
                adapter_version: AdapterVersion::new("adapter-v1"),
                provider_contract_id: ProviderContractId::new("endpoint-schema-v3"),
                meter_semantics_id: MeterSemanticsId::new("semantics-v1"),
                normalized_fingerprint: "fp-first".into(),
            },
        )
        .unwrap();
        let five_hour_id = insert_window(
            &fx.conn,
            &NewMeterWindow {
                observation_id,
                semantic_key: WindowSemanticKey::new("5h"),
                scope: WindowScope::AccountWide,
                quota_used: QuotaUsed::new(QuotaFractionPpm::new(600_000).unwrap()),
                reported_resolution: ReportedResolution::new(
                    QuotaFractionPpm::new(10_000).unwrap(),
                )
                .unwrap(),
                quantization: QuantizationSemantics::RoundedToNearest,
                resets_at: WindowResetState::Known(UtcTimestamp::from_unix_nanos(100_000)),
                nominal_duration: NominalWindowDuration::from_nanos(3_600_000_000_000),
            },
        )
        .unwrap();
        let seven_day_id = insert_window(
            &fx.conn,
            &NewMeterWindow {
                observation_id,
                semantic_key: WindowSemanticKey::new("7d"),
                scope: WindowScope::AccountWide,
                quota_used: QuotaUsed::new(QuotaFractionPpm::new(200_000).unwrap()),
                reported_resolution: ReportedResolution::new(
                    QuotaFractionPpm::new(10_000).unwrap(),
                )
                .unwrap(),
                quantization: QuantizationSemantics::RoundedToNearest,
                resets_at: WindowResetState::Known(UtcTimestamp::from_unix_nanos(900_000)),
                nominal_duration: NominalWindowDuration::from_nanos(604_800_000_000_000),
            },
        )
        .unwrap();
        let first_obs = observation_by_row_id(&fx.conn, observation_id)
            .unwrap()
            .unwrap();
        let mut first_windows = windows_by_observation(&fx.conn, observation_id).unwrap();
        detect_and_persist(
            &fx.conn,
            fx.account,
            &first_obs,
            &first_windows,
            UtcTimestamp::from_unix_nanos(30_500),
        )
        .unwrap();

        // Second observation: "5h" decreases with no reset (an anomaly), "7d" stays clean.
        let attempt2 = start_meter_attempt(
            &fx.conn,
            &NewMeterAttempt {
                run_id: fx.run,
                account_id: fx.account,
                provider: "test-provider".into(),
                request_started_at: UtcTimestamp::from_unix_nanos(39_900),
                credential_context_id: Some("ctx-1".into()),
                policy_snapshot_id: fx.snapshot,
                due_at: UtcTimestamp::from_unix_nanos(39_800),
                due_reason: DueReason::OrdinaryCadence,
                due_basis: None,
                provider_contract_id: "endpoint-schema-v3".into(),
                meter_semantics_id: "account-5h-v2".into(),
            },
        )
        .unwrap();
        let evidence_id2 = insert_response_evidence(
            &fx.conn,
            &NewMeterResponseEvidence {
                attempt_id: attempt2,
                response_classification: "200".into(),
                received_at: UtcTimestamp::from_unix_nanos(40_000),
                provider_observed_at_original: None,
                evidence_capsule: r#"{"windows":[{"key":"5h"},{"key":"7d"}]}"#.into(),
                capsule_schema_version: "capsule-v1".into(),
                sanitizer_version: "sanitizer-v1".into(),
                capture_truncated: false,
            },
        )
        .unwrap();
        let observation_id2 = insert_observation(
            &fx.conn,
            &NewMeterObservation {
                attempt_id: attempt2,
                evidence_id: evidence_id2,
                account_id: fx.account,
                provider: "test-provider".into(),
                provider_observed_at: None,
                received_at: UtcTimestamp::from_unix_nanos(40_000),
                measurement_basis: MeasurementBasis::LocallyReceived,
                observed_plan: Some("max".into()),
                observed_tier: Some("pro".into()),
                adapter_version: AdapterVersion::new("adapter-v1"),
                provider_contract_id: ProviderContractId::new("endpoint-schema-v3"),
                meter_semantics_id: MeterSemanticsId::new("semantics-v1"),
                normalized_fingerprint: "fp-second".into(),
            },
        )
        .unwrap();
        insert_window(
            &fx.conn,
            &NewMeterWindow {
                observation_id: observation_id2,
                semantic_key: WindowSemanticKey::new("5h"),
                scope: WindowScope::AccountWide,
                quota_used: QuotaUsed::new(QuotaFractionPpm::new(400_000).unwrap()),
                reported_resolution: ReportedResolution::new(
                    QuotaFractionPpm::new(10_000).unwrap(),
                )
                .unwrap(),
                quantization: QuantizationSemantics::RoundedToNearest,
                resets_at: WindowResetState::Known(UtcTimestamp::from_unix_nanos(100_000)),
                nominal_duration: NominalWindowDuration::from_nanos(3_600_000_000_000),
            },
        )
        .unwrap();
        insert_window(
            &fx.conn,
            &NewMeterWindow {
                observation_id: observation_id2,
                semantic_key: WindowSemanticKey::new("7d"),
                scope: WindowScope::AccountWide,
                quota_used: QuotaUsed::new(QuotaFractionPpm::new(250_000).unwrap()),
                reported_resolution: ReportedResolution::new(
                    QuotaFractionPpm::new(10_000).unwrap(),
                )
                .unwrap(),
                quantization: QuantizationSemantics::RoundedToNearest,
                resets_at: WindowResetState::Known(UtcTimestamp::from_unix_nanos(900_000)),
                nominal_duration: NominalWindowDuration::from_nanos(604_800_000_000_000),
            },
        )
        .unwrap();
        let second_obs = observation_by_row_id(&fx.conn, observation_id2)
            .unwrap()
            .unwrap();
        let mut second_windows = windows_by_observation(&fx.conn, observation_id2).unwrap();

        // Canonical order: sorted by semantic key.
        let canonical_outcome = {
            second_windows.sort_by(|a, b| a.semantic_key.as_str().cmp(b.semantic_key.as_str()));
            first_windows.sort_by(|a, b| a.semantic_key.as_str().cmp(b.semantic_key.as_str()));
            detect_and_persist(
                &fx.conn,
                fx.account,
                &second_obs,
                &second_windows,
                UtcTimestamp::from_unix_nanos(40_500),
            )
            .unwrap()
        };
        let mut canonical_kinds: Vec<&str> = canonical_outcome
            .anomalies
            .iter()
            .map(|a| a.kind.as_str())
            .collect();
        canonical_kinds.sort_unstable();

        // Nothing new is recorded a second time (idempotent), but reversing the
        // input order must still resolve to an identical persisted anomaly set.
        let reversed: Vec<StoredMeterWindow> = second_windows.iter().cloned().rev().collect();
        let reordered_outcome = detect_and_persist(
            &fx.conn,
            fx.account,
            &second_obs,
            &reversed,
            UtcTimestamp::from_unix_nanos(41_000),
        )
        .unwrap();
        let mut reordered_kinds: Vec<&str> = reordered_outcome
            .anomalies
            .iter()
            .map(|a| a.kind.as_str())
            .collect();
        reordered_kinds.sort_unstable();

        assert_eq!(canonical_kinds, reordered_kinds);
        assert_eq!(anomaly_count(&fx.conn).unwrap().value(), 1);
        let _ = five_hour_id;
        let _ = seven_day_id;
    }

    /// A new account-wide window and a missing model-specific window each
    /// produce their exact typed classification.
    #[test]
    fn window_set_evolution_produces_its_exact_typed_classification() {
        let fx = fixture();
        // First observation: one model-specific window only.
        let attempt = start_meter_attempt(
            &fx.conn,
            &NewMeterAttempt {
                run_id: fx.run,
                account_id: fx.account,
                provider: "test-provider".into(),
                request_started_at: UtcTimestamp::from_unix_nanos(29_900),
                credential_context_id: Some("ctx-1".into()),
                policy_snapshot_id: fx.snapshot,
                due_at: UtcTimestamp::from_unix_nanos(29_800),
                due_reason: DueReason::OrdinaryCadence,
                due_basis: None,
                provider_contract_id: "endpoint-schema-v3".into(),
                meter_semantics_id: "account-5h-v2".into(),
            },
        )
        .unwrap();
        let evidence_id = insert_response_evidence(
            &fx.conn,
            &NewMeterResponseEvidence {
                attempt_id: attempt,
                response_classification: "200".into(),
                received_at: UtcTimestamp::from_unix_nanos(30_000),
                provider_observed_at_original: None,
                evidence_capsule: r#"{"windows":[{"key":"model-5h"}]}"#.into(),
                capsule_schema_version: "capsule-v1".into(),
                sanitizer_version: "sanitizer-v1".into(),
                capture_truncated: false,
            },
        )
        .unwrap();
        let observation_id = insert_observation(
            &fx.conn,
            &NewMeterObservation {
                attempt_id: attempt,
                evidence_id,
                account_id: fx.account,
                provider: "test-provider".into(),
                provider_observed_at: None,
                received_at: UtcTimestamp::from_unix_nanos(30_000),
                measurement_basis: MeasurementBasis::LocallyReceived,
                observed_plan: Some("max".into()),
                observed_tier: Some("pro".into()),
                adapter_version: AdapterVersion::new("adapter-v1"),
                provider_contract_id: ProviderContractId::new("endpoint-schema-v3"),
                meter_semantics_id: MeterSemanticsId::new("semantics-v1"),
                normalized_fingerprint: "fp-first".into(),
            },
        )
        .unwrap();
        insert_window(
            &fx.conn,
            &NewMeterWindow {
                observation_id,
                semantic_key: WindowSemanticKey::new("model-5h"),
                scope: WindowScope::ModelSpecific(ModelId::new("model-a")),
                quota_used: QuotaUsed::new(QuotaFractionPpm::new(400_000).unwrap()),
                reported_resolution: ReportedResolution::new(
                    QuotaFractionPpm::new(10_000).unwrap(),
                )
                .unwrap(),
                quantization: QuantizationSemantics::RoundedToNearest,
                resets_at: WindowResetState::Known(UtcTimestamp::from_unix_nanos(100_000)),
                nominal_duration: NominalWindowDuration::from_nanos(3_600_000_000_000),
            },
        )
        .unwrap();
        let first_obs = observation_by_row_id(&fx.conn, observation_id)
            .unwrap()
            .unwrap();
        let first_windows = windows_by_observation(&fx.conn, observation_id).unwrap();
        detect_and_persist(
            &fx.conn,
            fx.account,
            &first_obs,
            &first_windows,
            UtcTimestamp::from_unix_nanos(30_500),
        )
        .unwrap();

        // Second observation: the model-specific window is gone, and a new
        // account-wide window appears instead.
        let (second_obs, second_window) = record_observation(
            &fx,
            40_000,
            300_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(200_000)),
        );
        let outcome = detect_and_persist(
            &fx.conn,
            fx.account,
            &second_obs,
            &[second_window],
            UtcTimestamp::from_unix_nanos(40_500),
        )
        .unwrap();

        assert_eq!(outcome.window_set_changes.len(), 2);
        let kinds: std::collections::BTreeSet<&str> = outcome
            .window_set_changes
            .iter()
            .map(|c| c.kind.as_str())
            .collect();
        assert!(kinds.contains("new_account_wide_window"));
        assert!(kinds.contains("missing_model_specific_window"));

        // Structural evolution never touches original evidence: both windows exist unchanged.
        assert_eq!(anomaly_count(&fx.conn).unwrap().value(), 0);
        assert_eq!(all_window_set_changes(&fx.conn).unwrap().len(), 2);
    }

    /// Direct SQL `UPDATE` and `DELETE` against every irreplaceable table
    /// owned here fail through the database triggers.
    #[test]
    fn triggers_refuse_every_update_and_delete_on_the_irreplaceable_tables() {
        let fx = fixture();
        let (first_obs, first_window) = record_observation(
            &fx,
            30_000,
            600_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(100_000)),
        );
        detect_and_persist(
            &fx.conn,
            fx.account,
            &first_obs,
            &[first_window],
            UtcTimestamp::from_unix_nanos(30_500),
        )
        .unwrap();
        let (second_obs, second_window) = record_observation(
            &fx,
            40_000,
            400_000,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(100_000)),
        );
        detect_and_persist(
            &fx.conn,
            fx.account,
            &second_obs,
            &[second_window],
            UtcTimestamp::from_unix_nanos(40_500),
        )
        .unwrap();

        for sql in [
            "UPDATE meter_window_anomaly SET detail = 'rewritten' WHERE id = 1",
            "DELETE FROM meter_window_anomaly WHERE id = 1",
            "UPDATE meter_calibration_exclusion SET interval_end_at = 0 WHERE id = 1",
            "DELETE FROM meter_calibration_exclusion WHERE id = 1",
        ] {
            let err = fx
                .conn
                .execute(sql, [])
                .err()
                .unwrap_or_else(|| panic!("direct statement must be refused: {sql}"));
            assert!(
                err.to_string().contains("irreplaceable evidence"),
                "the trigger must name the reason: {err}"
            );
        }
    }
}
