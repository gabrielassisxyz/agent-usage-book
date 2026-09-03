//! Reading the published projection back for `aub status` (`aub-me5.6`).
//!
//! The reader is the status path's only data source, so it is built to the
//! status contract's own standard (PLAN.md section 16.2): one bounded local
//! file read, no SQLite, no network, no parsing beyond what the schema version
//! licenses. A file that is missing, malformed, oversized, or written by a
//! schema version this binary does not understand is reported as unavailable
//! with a compact reason, and its content is never interpreted optimistically:
//! the schema version is checked before any account state is parsed, so a
//! newer or older format can never be misread field by field.
//!
//! Every value the reader returns is a stored input, never a stored result.
//! Freshness is recomputed here through the same pure state machine the
//! sampler uses, so the file's absence of a freshness boolean stays true on
//! the read side too.

use std::fs;
use std::path::Path;

use serde_json::Value;

use super::{
    LatestAttempt, Projection, ProjectedAccount, ProjectedWindow, SuccessfulObservation,
    TerminalOutcome, PROJECTION_SCHEMA_VERSION,
};
use crate::domain::attempt::{AttemptId, AttemptOutcome, AttemptResult, AttemptStarted};
use crate::domain::freshness::{compute_freshness, Freshness, FreshnessInput, Observed};
use crate::domain::ids::CredentialContextId;
use crate::domain::quota::{QuotaFractionPpm, QuotaRemaining, QuotaUsed};
use crate::domain::time::{
    Clock, ClockSkewEnvelope, MonotonicDuration, ProviderObservedAt, ReceivedAt, UtcTimestamp,
};
use crate::domain::window::{ModelId, NominalWindowDuration, ReportedResolution, WindowScope};
use crate::store::account::AccountId;
use crate::store::ledger_generation::Generation;
use crate::store::meter_attempt::failure_class_sql;
use crate::store::meter_evidence::{measurement_basis_sql, quantization_sql, ObservationRowId};

/// The read bound for one projection file. A projection is a bounded document
/// by construction: one record per configured account. A file larger than this
/// is not a projection this binary wrote, and reading it unbounded would make
/// the status path's latency depend on an arbitrary file's size.
pub const MAX_PROJECTION_BYTES: u64 = 8 * 1024 * 1024;

/// The credential context substituted when the projection records no context
/// for the latest attempt. It never reaches a user-visible surface: the
/// freshness machine compares contexts only against other recorded contexts,
/// and a projection without one has nothing to compare against.
const UNRECORDED_CREDENTIAL_CONTEXT: &str = "unrecorded";

/// The outcome of one projection read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionRead {
    Available(Projection),
    Unavailable(ProjectionUnavailable),
}

/// Why the projection could not be read. Each variant renders as the question
/// mark with its own compact reason; none is ever substituted with a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionUnavailable {
    Missing,
    TooLarge { limit_bytes: u64 },
    Malformed { detail: String },
    UnsupportedSchema { found: u64, supported: u32 },
}

impl ProjectionUnavailable {
    /// The machine-readable state name, the same vocabulary the JSON contract
    /// uses for a projection that could not be read.
    pub fn state_name(&self) -> &'static str {
        match self {
            ProjectionUnavailable::Missing => "missing",
            ProjectionUnavailable::TooLarge { .. } => "too_large",
            ProjectionUnavailable::Malformed { .. } => "malformed",
            ProjectionUnavailable::UnsupportedSchema { .. } => "unsupported_schema",
        }
    }

    /// The compact human reason, rendered where the output mode permits.
    pub fn reason(&self) -> String {
        match self {
            ProjectionUnavailable::Missing => "projection not found".to_string(),
            ProjectionUnavailable::TooLarge { limit_bytes } => format!(
                "projection exceeds the read bound of {limit_bytes} bytes"
            ),
            ProjectionUnavailable::Malformed { detail } => {
                format!("projection malformed: {detail}")
            }
            ProjectionUnavailable::UnsupportedSchema { found, supported } => {
                if *found < u64::from(*supported) {
                    format!(
                        "projection schema version {found} is older than the supported version {supported}"
                    )
                } else {
                    format!(
                        "projection schema version {found} is newer than the supported version {supported}"
                    )
                }
            }
        }
    }
}

/// Reads the projection at `path` with one bounded read.
///
/// The bound is checked on the file's size before the read, so an oversized
/// file is refused without ever being pulled into memory. A read or decode
/// failure is a malformed projection, not a panic and not a fallback to some
/// previous value: there is no previous value in this path to fall back to.
pub fn read_projection(path: &Path) -> ProjectionRead {
    let Ok(metadata) = fs::metadata(path) else {
        return ProjectionRead::Unavailable(ProjectionUnavailable::Missing);
    };
    if metadata.len() > MAX_PROJECTION_BYTES {
        return ProjectionRead::Unavailable(ProjectionUnavailable::TooLarge {
            limit_bytes: MAX_PROJECTION_BYTES,
        });
    }
    let Ok(text) = fs::read_to_string(path) else {
        return ProjectionRead::Unavailable(ProjectionUnavailable::Malformed {
            detail: "unreadable or not UTF-8".to_string(),
        });
    };
    match parse_projection(&text) {
        Ok(projection) => ProjectionRead::Available(projection),
        Err(unavailable) => ProjectionRead::Unavailable(unavailable),
    }
}

/// Parses the projection text, licensing the content parse only after the
/// schema version has been checked. An unsupported version never reaches the
/// field parser, so a format this binary does not know cannot be misread.
fn parse_projection(text: &str) -> Result<Projection, ProjectionUnavailable> {
    let document: Value = serde_json::from_str(text).map_err(|error| {
        ProjectionUnavailable::Malformed {
            detail: format!("not valid JSON: {error}"),
        }
    })?;
    let Some(version) = document.get("schema_version") else {
        return Err(ProjectionUnavailable::Malformed {
            detail: "no schema version".to_string(),
        });
    };
    let Some(version) = version.as_u64() else {
        return Err(ProjectionUnavailable::Malformed {
            detail: "schema version is not an unsigned integer".to_string(),
        });
    };
    if version != u64::from(PROJECTION_SCHEMA_VERSION) {
        return Err(ProjectionUnavailable::UnsupportedSchema {
            found: version,
            supported: PROJECTION_SCHEMA_VERSION,
        });
    }
    projection_from_document(&document).map_err(|detail| ProjectionUnavailable::Malformed { detail })
}

fn projection_from_document(document: &Value) -> Result<Projection, String> {
    let generation = document
        .get("ledger_generation")
        .and_then(Value::as_u64)
        .ok_or_else(|| "no ledger generation".to_string())?;
    let accounts_json = document
        .get("accounts")
        .and_then(Value::as_array)
        .ok_or_else(|| "no accounts array".to_string())?;
    let accounts = accounts_json
        .iter()
        .map(account_from_document)
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Projection {
        ledger_generation: Generation::new(generation),
        accounts,
    })
}

fn account_from_document(account: &Value) -> Result<ProjectedAccount, String> {
    let object = as_object(account, "account")?;
    Ok(ProjectedAccount {
        account_id: AccountId::new(required_i64(object, "account_id")?),
        logical_name: required_str(object, "logical_name")?.to_string(),
        provider: required_str(object, "provider")?.to_string(),
        last_successful_observation: match object.get("last_successful_observation") {
            None | Some(Value::Null) => None,
            Some(value) => Some(successful_observation_from_document(value)?),
        },
        latest_attempt: match object.get("latest_attempt") {
            None | Some(Value::Null) => None,
            Some(value) => Some(latest_attempt_from_document(value)?),
        },
    })
}

fn successful_observation_from_document(value: &Value) -> Result<SuccessfulObservation, String> {
    let object = as_object(value, "last_successful_observation")?;
    let windows_json = object
        .get("windows")
        .and_then(Value::as_array)
        .ok_or_else(|| "no windows array".to_string())?;
    let windows = windows_json
        .iter()
        .map(window_from_document)
        .collect::<Result<Vec<_>, String>>()?;
    let basis_code = required_str(object, "measurement_basis")?;
    Ok(SuccessfulObservation {
        observation_id: ObservationRowId::new(required_i64(object, "observation_id")?),
        provider_observed_at: optional_nanos(object, "provider_observed_at_nanos")?,
        received_at: UtcTimestamp::from_unix_nanos(required_i64(object, "received_at_nanos")?),
        measurement_basis: measurement_basis_sql::from_sql(basis_code)
            .map_err(|error| error.to_string())?,
        windows,
    })
}

fn window_from_document(value: &Value) -> Result<ProjectedWindow, String> {
    let object = as_object(value, "window")?;
    let scope = match required_str(object, "scope_kind")? {
        "account_wide" => {
            if object.get("scoped_model").map(Value::is_null) != Some(true) {
                return Err("an account_wide window carries a scoped model".to_string());
            }
            WindowScope::AccountWide
        }
        "model_specific" => {
            let model = required_str(object, "scoped_model")?;
            WindowScope::ModelSpecific(ModelId::new(model.to_string()))
        }
        other => return Err(format!("unknown scope kind {other:?}")),
    };
    Ok(ProjectedWindow {
        semantic_key: required_str(object, "semantic_key")?.to_string(),
        scope,
        quota_used_ppm: quota_used(object, "quota_used_ppm")?,
        reported_resolution_ppm: reported_resolution(object, "reported_resolution_ppm")?,
        quantization: quantization_sql::from_sql(required_str(object, "quantization")?)
            .map_err(|error| error.to_string())?,
        resets_at: UtcTimestamp::from_unix_nanos(required_i64(object, "resets_at_nanos")?),
        nominal_duration_nanos: NominalWindowDuration::from_nanos(required_u64(
            object,
            "nominal_duration_nanos",
        )?),
    })
}

fn latest_attempt_from_document(value: &Value) -> Result<LatestAttempt, String> {
    let object = as_object(value, "latest_attempt")?;
    let result = match object.get("result") {
        None | Some(Value::Null) => None,
        Some(value) => Some(terminal_outcome_from_document(value)?),
    };
    Ok(LatestAttempt {
        attempt_id: AttemptId::new(required_u64(object, "attempt_id")?),
        request_started_at: UtcTimestamp::from_unix_nanos(required_i64(
            object,
            "request_started_at_nanos",
        )?),
        credential_context_id: match object.get("credential_context_id") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_str()
                    .ok_or_else(|| "credential context id is not a string".to_string())?
                    .to_string(),
            ),
        },
        result,
    })
}

fn terminal_outcome_from_document(value: &Value) -> Result<TerminalOutcome, String> {
    let object = as_object(value, "result")?;
    let outcome = match required_str(object, "outcome")? {
        "success" => AttemptOutcome::Success,
        "auth_required" => AttemptOutcome::AuthRequired,
        "unreachable" => {
            let class = object
                .get("failure_class")
                .and_then(Value::as_str)
                .ok_or_else(|| "an unreachable outcome carries no failure class".to_string())?;
            AttemptOutcome::Unreachable(failure_class_sql::from_sql(class).map_err(|error| {
                error.to_string()
            })?)
        }
        other => return Err(format!("unknown attempt outcome {other:?}")),
    };
    Ok(TerminalOutcome {
        completed_at: UtcTimestamp::from_unix_nanos(required_i64(object, "completed_at_nanos")?),
        outcome,
    })
}

fn as_object<'a>(value: &'a Value, what: &str) -> Result<&'a serde_json::Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{what} is not an object"))
}

fn required_u64(object: &serde_json::Map<String, Value>, key: &str) -> Result<u64, String> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{key} is not an unsigned integer"))
}

fn required_i64(object: &serde_json::Map<String, Value>, key: &str) -> Result<i64, String> {
    object
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("{key} is not an integer"))
}

fn required_str<'a>(object: &'a serde_json::Map<String, Value>, key: &str) -> Result<&'a str, String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{key} is not a string"))
}

fn optional_nanos(
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<UtcTimestamp>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let nanos = value
                .as_i64()
                .ok_or_else(|| format!("{key} is not an integer"))?;
            Ok(Some(UtcTimestamp::from_unix_nanos(nanos)))
        }
    }
}

fn quota_used(object: &serde_json::Map<String, Value>, key: &str) -> Result<QuotaUsed, String> {
    let ppm = required_i64(object, key)?;
    let fraction = QuotaFractionPpm::new(ppm as i32).ok_or_else(|| {
        format!("{key} {ppm} is outside the representable parts-per-million range")
    })?;
    Ok(QuotaUsed::new(fraction))
}

fn reported_resolution(
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<ReportedResolution, String> {
    let ppm = required_i64(object, key)?;
    let fraction = QuotaFractionPpm::new(ppm as i32).ok_or_else(|| {
        format!("{key} {ppm} is outside the representable parts-per-million range")
    })?;
    ReportedResolution::new(fraction).ok_or_else(|| format!("{key} {ppm} has no resolution"))
}

/// The windows of one observation that apply under a model selector.
///
/// Without a selector, every window applies: the account line reports the
/// account's most constrained reading. With a selector, the account-wide
/// constraints still apply and only the chosen model's own windows join them;
/// a window scoped to any other model constrains that other model, not this
/// reading, and is excluded by name.
pub fn applicable_windows<'a>(
    windows: &'a [ProjectedWindow],
    model: Option<&str>,
) -> Vec<&'a ProjectedWindow> {
    windows
        .iter()
        .filter(|window| match (&window.scope, model) {
            (WindowScope::AccountWide, _) => true,
            (WindowScope::ModelSpecific(_), None) => true,
            (WindowScope::ModelSpecific(window_model), Some(selected)) => {
                window_model.as_str() == selected
            }
        })
        .collect()
}

/// One account's status reading, computed from the projection's stored inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedReading {
    pub freshness: Freshness<QuotaRemaining>,
    /// The window whose remaining fraction the reading reports: the applicable
    /// window with the least remaining quota.
    pub limiting_window: Option<LimitingWindowRef>,
    /// Every window scope included in the reading, in first-seen order.
    pub included_scopes: Vec<WindowScope>,
}

/// The limiting window behind a reading: which scope it belongs to and the
/// nominal length the design's status line shows beside a fresh value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitingWindowRef {
    pub scope: WindowScope,
    pub nominal_duration: NominalWindowDuration,
}

/// Computes one account's status reading from its projected state.
///
/// The reading value is the minimum remaining fraction across the applicable
/// windows, so the number shown is the account's most constrained window. An
/// observation whose applicable window set is empty yields no reading value at
/// all: the freshness machine then reports no successful observation rather
/// than a number nothing justifies. Freshness itself comes from the same pure
/// state machine every other reader uses, so the projection cannot grow a
/// second, diverging freshness implementation.
pub fn account_reading(
    projected: Option<&ProjectedAccount>,
    model: Option<&str>,
    freshness_horizon: MonotonicDuration,
    command_horizon: MonotonicDuration,
    clock_skew_envelope: ClockSkewEnvelope,
    clock: &impl Clock,
) -> ProjectedReading {
    let mut last_good: Option<Observed<QuotaRemaining>> = None;
    let mut limiting_window: Option<LimitingWindowRef> = None;
    let mut included_scopes: Vec<WindowScope> = Vec::new();

    if let Some(account) = projected {
        if let Some(success) = &account.last_successful_observation {
            let applicable = applicable_windows(&success.windows, model);
            for window in &applicable {
                if !included_scopes.contains(&window.scope) {
                    included_scopes.push(window.scope.clone());
                }
            }
            if let Some(limit) = applicable
                .iter()
                .max_by_key(|window| window.quota_used_ppm.as_ppm().get())
            {
                let remaining: QuotaRemaining = limit.quota_used_ppm.complement();
                last_good = Some(Observed::new(
                    remaining,
                    success.provider_observed_at.map(ProviderObservedAt::new),
                    ReceivedAt::new(success.received_at),
                    success.measurement_basis,
                ));
                limiting_window = Some(LimitingWindowRef {
                    scope: limit.scope.clone(),
                    nominal_duration: limit.nominal_duration_nanos,
                });
            }
        }
    }

    let credential_context = CredentialContextId::new(
        projected
            .and_then(|account| account.latest_attempt.as_ref())
            .and_then(|attempt| attempt.credential_context_id.clone())
            .unwrap_or_else(|| UNRECORDED_CREDENTIAL_CONTEXT.to_string()),
    );
    let latest = projected
        .and_then(|account| account.latest_attempt.as_ref())
        .map(|attempt: &LatestAttempt| {
            let result = attempt.result.as_ref().map(|terminal| {
                let started_nanos = attempt.request_started_at.unix_nanos();
                let finished_nanos = terminal.completed_at.unix_nanos();
                let elapsed_nanos = (finished_nanos - started_nanos).max(0) as u64;
                AttemptResult::new(
                    attempt.attempt_id,
                    terminal.completed_at,
                    MonotonicDuration::from_nanos(elapsed_nanos),
                    terminal.outcome,
                )
            });
            crate::domain::freshness::LatestAttempt::new(
                AttemptStarted::new(attempt.attempt_id, attempt.request_started_at),
                result,
                &credential_context,
            )
        });

    let input = FreshnessInput::new(
        last_good,
        None,
        latest,
        None,
        None,
        freshness_horizon,
        command_horizon,
        clock_skew_envelope,
    );
    let freshness = compute_freshness(&input, clock);
    ProjectedReading {
        freshness,
        limiting_window,
        included_scopes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::quota::QuotaUsed;
    use crate::domain::time::{FakeClock, MeasurementBasis};
    use crate::domain::window::{NominalWindowDuration, QuantizationSemantics};

    fn window(used_ppm: i32, model: Option<&str>) -> ProjectedWindow {
        ProjectedWindow {
            semantic_key: "five_hour".to_string(),
            scope: match model {
                None => WindowScope::AccountWide,
                Some(name) => WindowScope::ModelSpecific(ModelId::new(name.to_string())),
            },
            quota_used_ppm: QuotaUsed::new(QuotaFractionPpm::new(used_ppm).unwrap()),
            reported_resolution_ppm: ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap())
                .unwrap(),
            quantization: QuantizationSemantics::Exact,
            resets_at: UtcTimestamp::from_unix_nanos(9_000),
            nominal_duration_nanos: NominalWindowDuration::from_nanos(18_000_000_000_000),
        }
    }

    /// Selector composition: account only, model only, both together, unknown
    /// model. Without a selector every window applies; with one, the
    /// account-wide constraints remain and only the chosen model's windows
    /// join, so an unrelated model's window is excluded by name.
    #[test]
    fn selector_composition_covers_every_shape() {
        let windows = vec![
            window(100_000, None),
            window(600_000, Some("claude-model-x")),
            window(900_000, Some("claude-model-y")),
        ];

        let no_selector = applicable_windows(&windows, None);
        assert_eq!(no_selector.len(), 3, "no selector: every window applies");

        let model_only = applicable_windows(&windows, Some("claude-model-x"));
        assert_eq!(model_only.len(), 2, "the chosen model joins the account-wide windows");
        assert!(
            model_only
                .iter()
                .all(|w| !matches!(&w.scope, WindowScope::ModelSpecific(m) if m.as_str() == "claude-model-y")),
            "an unrelated model's window is excluded"
        );

        let both = applicable_windows(&windows, Some("claude-model-y"));
        assert_eq!(both.len(), 2);

        let unknown_model = applicable_windows(&windows, Some("model-nobody-reported"));
        assert_eq!(
            unknown_model.len(),
            1,
            "an unknown model leaves only the account-wide windows"
        );
        assert!(matches!(
            unknown_model[0].scope,
            WindowScope::AccountWide
        ));
    }

    /// The limiting window is the applicable window with the least remaining
    /// quota, and the reading value is that window's remaining fraction.
    #[test]
    fn the_reading_reports_the_least_remaining_applicable_window() {
        let account = ProjectedAccount {
            account_id: AccountId::new(1),
            logical_name: "work".to_string(),
            provider: "anthropic".to_string(),
            last_successful_observation: Some(SuccessfulObservation {
                observation_id: ObservationRowId::new(1),
                provider_observed_at: Some(UtcTimestamp::from_unix_nanos(1_000)),
                received_at: UtcTimestamp::from_unix_nanos(1_100),
                measurement_basis: MeasurementBasis::ProviderObserved,
                windows: vec![window(100_000, None), window(620_000, Some("claude-model-x"))],
            }),
            latest_attempt: Some(LatestAttempt {
                attempt_id: AttemptId::new(2),
                request_started_at: UtcTimestamp::from_unix_nanos(1_200),
                credential_context_id: Some("ctx".to_string()),
                result: Some(TerminalOutcome {
                    completed_at: UtcTimestamp::from_unix_nanos(1_500),
                    outcome: AttemptOutcome::Success,
                }),
            }),
        };
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(2_000));
        let reading = account_reading(
            Some(&account),
            None,
            MonotonicDuration::from_seconds(720),
            MonotonicDuration::from_seconds(8),
            ClockSkewEnvelope::new(MonotonicDuration::from_seconds(60)),
            &clock,
        );

        let Freshness::Fresh { observed, .. } = &reading.freshness else {
            panic!("an observation one second old within the horizon is fresh");
        };
        assert_eq!(observed.value().as_ppm().get(), 380_000);
        let limit = reading.limiting_window.expect("a windowed observation names its limit");
        assert_eq!(limit.nominal_duration.as_nanos(), 18_000_000_000_000);
        assert_eq!(reading.included_scopes.len(), 2);
    }

    /// An observation whose applicable window set is empty yields no reading
    /// value: the machine reports no successful observation rather than a
    /// number nothing justifies.
    #[test]
    fn an_observation_with_no_applicable_windows_yields_no_value() {
        let account = ProjectedAccount {
            account_id: AccountId::new(1),
            logical_name: "work".to_string(),
            provider: "anthropic".to_string(),
            last_successful_observation: Some(SuccessfulObservation {
                observation_id: ObservationRowId::new(1),
                provider_observed_at: None,
                received_at: UtcTimestamp::from_unix_nanos(1_000),
                measurement_basis: MeasurementBasis::ProviderObserved,
                windows: vec![window(400_000, Some("claude-model-x"))],
            }),
            latest_attempt: None,
        };
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(2_000));
        let reading = account_reading(
            Some(&account),
            Some("some-other-model"),
            MonotonicDuration::from_seconds(720),
            MonotonicDuration::from_seconds(8),
            ClockSkewEnvelope::new(MonotonicDuration::from_seconds(60)),
            &clock,
        );

        let Freshness::Stale { last_good, reason, .. } = &reading.freshness else {
            panic!("no applicable window means no reading value");
        };
        assert!(last_good.is_none());
        assert!(matches!(reason, StaleReason::NoSuccessfulObservation));
    }

    /// An attempt with no terminal result, started beyond the command horizon,
    /// is a collector interruption, never a network timeout and never an
    /// absent attempt: the design's fourth rendering depends on this route.
    #[test]
    fn a_resultless_attempt_past_the_command_horizon_is_collector_interrupted() {
        let account = ProjectedAccount {
            account_id: AccountId::new(1),
            logical_name: "work".to_string(),
            provider: "anthropic".to_string(),
            last_successful_observation: Some(SuccessfulObservation {
                observation_id: ObservationRowId::new(1),
                provider_observed_at: Some(UtcTimestamp::from_unix_nanos(1_000)),
                received_at: UtcTimestamp::from_unix_nanos(1_100),
                measurement_basis: MeasurementBasis::ProviderObserved,
                windows: vec![window(620_000, None)],
            }),
            latest_attempt: Some(LatestAttempt {
                attempt_id: AttemptId::new(9),
                request_started_at: UtcTimestamp::from_unix_nanos(1_200),
                credential_context_id: Some("ctx".to_string()),
                result: None,
            }),
        };
        // The clock is far past the command horizon, and the observation's age
        // (900 nanos) stays inside the freshness horizon, so the reason can
        // only come from the resultless attempt.
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1_000 + 540_000_000_000));
        let reading = account_reading(
            Some(&account),
            None,
            MonotonicDuration::from_seconds(720),
            MonotonicDuration::from_seconds(8),
            ClockSkewEnvelope::new(MonotonicDuration::from_seconds(60)),
            &clock,
        );

        let Freshness::Stale { reason, last_good, .. } = &reading.freshness else {
            panic!("a resultless attempt past the command horizon is stale");
        };
        assert!(matches!(reason, StaleReason::CollectorInterrupted));
        assert!(last_good.is_some(), "the historical value stays attached");
    }

    use crate::domain::freshness::StaleReason;
}