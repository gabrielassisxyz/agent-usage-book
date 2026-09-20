//! The OpenCode Go console status meter adapter (aub-8hu3, aub-id41).
//!
//! The OpenCode Go usage meter is served by the console's own endpoint,
//! `GET https://opencode.ai/console/api/go/status`, which answers JSON for a
//! signed-in browser session. The reference tool
//! (`git.sr.ht/~hrbrmstr/opencode-go-usage`, `usage/usage.go`) reads that
//! endpoint, and this adapter follows the same contract.
//!
//! An earlier revision of this adapter scraped the rendered workspace page,
//! because that was the reference's contract when it was written. The console
//! stopped accepting a single-cookie caller between 2026-09-17 and 2026-09-18
//! and the reference migrated in the same window; measured against the live
//! service on 2026-09-20, the page answers `302` to `/console/login` and the
//! endpoint answers `401` for the `auth` cookie alone. Both cookies are
//! therefore mandatory, and the endpoint is strictly the better surface: it
//! states a refused credential as a status instead of as a redirect to be
//! sniffed, and it reports integer micro-cents and an absolute RFC 3339 reset
//! instant where the markup carried a percentage rounded to one decimal place
//! and a reset sentence floored to whole hours.
//!
//! The response body carries one object per window under `access.meters`,
//! keyed `fiveHour`, `week` and `month`. Each holds `limitMicroCents` and
//! `usedMicroCents` as decimal strings, and `resetsAt` either as an RFC 3339
//! instant, as `null`, or not at all. A window with no reset instant is
//! [`WindowResetState::NotStarted`], which is the meaning `aub-eun.15`
//! decided for a null reset rather than one this adapter invents; nothing
//! here derives a reset from `access.endsAt`, which would be an inference and
//! not a reading.
//!
//! The credential is the `Cookie` header value, which is what `aub-r7k0`
//! decided it was ("a file whose content is the `Cookie:` header value"); the
//! previous revision narrowed it to the bare `auth` value and rebuilt the
//! header itself, which cannot express the pair the console now requires.
//! The adapter sends the material verbatim and reinterprets nothing.
//!
//! A body that is not JSON, or that carries no `access.meters` object, is
//! [`FailureClass::SchemaDrift`], never a silent zero.
//!
//! May not depend on:
//! - SQLite (rule `03`)
//! - credential or configuration modules (rule `07`)
//! - the ureq transport driver (rule `12`)
//! - write-capable filesystem facilities (rule `17`)
//! - presentation or calibration modules

use crate::domain::failure::{AuthReason, FailureClass, HttpStatusClass};
use crate::domain::ids::{MeterSemanticsId, ProviderContractId};
use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
use crate::domain::time::{
    Clock, MeasurementBasis, MonotonicDuration, ProviderObservedAt, UtcTimestamp,
};
use crate::domain::window::{
    MeterWindow, NominalWindowDuration, QuantizationSemantics, ReportedResolution, ResetPrecision,
    WindowResetState, WindowScope, WindowSemanticKey,
};
use crate::meter::adapter::{
    AdapterDeclarations, CredentialHandle, HttpTransport, MeterRequest, ProviderAdapter,
    ProviderObservation, RequiredWindowKinds,
};
use crate::meter::evidence::{
    CapturedProviderResponse, JsonEvidenceCapsule, SensitiveResponseMaterial, capture_json_body,
    error_report_for_observation, quota_response_from_capsule,
};
use crate::meter::transport::{CommandBudget, HttpRequest, HttpResponse, RequestTimeoutConfig};

/// The response-state marker the parser keys on: the object that holds one
/// entry per usage window. A body with no `meters` object under `access` has
/// no usage meters to read, and is schema drift.
pub const STATE_MARKER: &str = "meters";

/// The provider base the status endpoint hangs from. The full request URL is
/// `{DEFAULT_PAGE_BASE}{STATUS_PATH}`; the workspace id rides in a header
/// rather than in the path, which is why the base alone is enough to reach
/// it.
pub const DEFAULT_PAGE_BASE: &str = "https://opencode.ai";

/// The status endpoint's path, appended to the base when no endpoint
/// override is in force.
pub const STATUS_PATH: &str = "/console/api/go/status";

/// The credential header: the console authenticates a browser session with
/// two cookies, `auth` and `__Host-console_session`, and the operator exports
/// the whole header value (decided in `aub-r7k0`). The adapter sends that
/// value verbatim and never builds a cookie pair of its own.
pub const COOKIE_HEADER: &str = "Cookie";

/// The header carrying the workspace id. The endpoint scopes its answer by
/// this header rather than by a path segment, so an observation without a
/// workspace id has nothing to ask about.
pub const ORG_HEADER: &str = "x-org-id";

/// The typed success reading produced by [`OpenCodeAdapter`]: one row per
/// usage window the response carries, with the reset instant the provider
/// stated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCodeReading {
    pub windows: Vec<MeterWindow>,
    pub provider_observed_at: Option<ProviderObservedAt>,
    pub provider_contract_id: ProviderContractId,
}

impl OpenCodeReading {
    pub fn new(windows: Vec<MeterWindow>) -> Self {
        Self {
            windows,
            provider_observed_at: None,
            provider_contract_id: ProviderContractId::new(OpenCodeAdapter::DEFAULT_CONTRACT_ID),
        }
    }
}

/// The OpenCode Go provider adapter.
///
/// The request URL comes from two places, in override order: the endpoint
/// override the caller resolved from the environment (the same channel
/// `AUB_ANTHROPIC_ENDPOINT` uses for the Anthropic adapter, so an end-to-end
/// run can point this adapter at a synthetic server), and otherwise the
/// provider's own base with the status path appended. The adapter never reads
/// configuration or the environment itself (rule `07`).
pub struct OpenCodeAdapter {
    endpoint_override: Option<String>,
    declarations: AdapterDeclarations,
}

impl OpenCodeAdapter {
    pub const DEFAULT_CONTRACT_ID: &'static str = "opencode-go-console-status-v1";
    pub const DEFAULT_SEMANTICS_ID: &'static str = "opencode-go-subscription-v1";
    pub const REQUIRED_WINDOW_KINDS: &'static [&'static str] = &["rolling", "weekly", "monthly"];

    /// The precision of every reset instant this adapter reports, in seconds.
    /// The endpoint states an absolute RFC 3339 instant rather than a rounded
    /// remaining duration, so the declaration is one second: the coarsest
    /// bound the whole-second constructor can express, and three orders of
    /// magnitude tighter than the one hour the page's rendered sentence
    /// forced. The classifier consumes this declaration (`aub-w1a0`).
    pub const RESET_PRECISION_SECONDS: u64 = 1;

    /// Builds the adapter with an optional full status-endpoint URL override.
    pub fn new(endpoint_override: Option<String>) -> Self {
        Self {
            endpoint_override,
            declarations: AdapterDeclarations::new(
                // The response documents no provider measurement time: the
                // reading's basis is the local receive instant. Unlike the
                // page revision, no reset arithmetic hangs off that anchor,
                // because every reset the provider states is absolute.
                MeasurementBasis::LocallyReceived,
                ProviderContractId::new(Self::DEFAULT_CONTRACT_ID),
                MeterSemanticsId::new(Self::DEFAULT_SEMANTICS_ID),
            )
            .with_required_window_kinds(RequiredWindowKinds::from_values(
                Self::REQUIRED_WINDOW_KINDS,
            ))
            .with_reset_precision(
                // One second, statically non-zero, so the constructor's
                // refusal is unreachable for this constant.
                ResetPrecision::from_seconds(Self::RESET_PRECISION_SECONDS)
                    .expect("a one-second precision is a non-zero second count"),
            ),
        }
    }

    /// The status endpoint URL for one observation: the override when the
    /// caller resolved one, otherwise the provider's base with the status
    /// path appended.
    fn status_url(&self) -> String {
        match &self.endpoint_override {
            Some(url) => url.clone(),
            None => format!("{DEFAULT_PAGE_BASE}{STATUS_PATH}"),
        }
    }

    pub fn endpoint_override(&self) -> Option<&str> {
        self.endpoint_override.as_deref()
    }
}

/// One part per million of the quota window, as the numerator of the
/// used-over-limit ratio: the fraction is computed in integers, because the
/// provider states both sides as exact micro-cent counts and a float round
/// trip would reintroduce the rounding the page revision was stuck with.
const PPM_SCALE: i128 = 1_000_000;

/// Converts an exact used-over-limit micro-cent ratio into parts per
/// million, rounded half away from zero. `None` when the limit is not a
/// usable denominator: a window whose limit is zero states no ceiling, and
/// reporting it as zero usage would print a number the provider did not
/// state.
fn ppm_from_micro_cents(used: i128, limit: i128) -> Option<i32> {
    if limit <= 0 || used < 0 {
        return None;
    }
    let scaled = used.checked_mul(PPM_SCALE)?;
    let rounded = (scaled.checked_add(limit / 2)?).checked_div(limit)?;
    i32::try_from(rounded).ok()
}

/// Parses one decimal micro-cent string. The endpoint states both counts as
/// strings rather than as numbers, which is what keeps them exact past the
/// range a JSON number is guaranteed to survive, so they are read as strings
/// here and never through a float.
fn micro_cents(value: Option<&serde_json::Value>) -> Option<i128> {
    value?.as_str()?.trim().parse::<i128>().ok()
}

/// Builds the raw-evidence JSON object from the response body: one entry per
/// recognized window, carrying the provider's own micro-cent strings and its
/// reset instant exactly as stated. This is the object the evidence capsule
/// is captured from, and the object [`parse_state`] later re-reads to derive
/// the typed windows, so the retained evidence is exactly what the reading
/// was derived from.
///
/// The projection is deliberately narrower than the response. The body also
/// carries a subscriber id and a payment-method id, and neither belongs in an
/// evidence store that exists to prove a quota number; keeping the projection
/// narrow means they are never captured in the first place, which is a
/// stronger guarantee than redacting them afterwards.
fn build_raw_state(body: &[u8]) -> Result<serde_json::Value, FailureClass> {
    let root: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| FailureClass::MalformedBody)?;
    let meters = root
        .get("access")
        .and_then(|access| access.get(STATE_MARKER))
        .and_then(|meters| meters.as_object())
        .ok_or(FailureClass::SchemaDrift)?;
    let mut object = serde_json::Map::new();
    for (name, meter) in meters {
        let Some(kind) = WindowKind::from_meter_name(name) else {
            continue;
        };
        let Some(meter) = meter.as_object() else {
            return Err(FailureClass::MalformedBody);
        };
        object.insert(
            kind.semantic_key().as_str().to_string(),
            serde_json::json!({
                "used_micro_cents": meter.get("usedMicroCents").cloned(),
                "limit_micro_cents": meter.get("limitMicroCents").cloned(),
                "resets_at": meter.get("resetsAt").cloned(),
            }),
        );
    }
    if object.is_empty() {
        return Err(FailureClass::SchemaDrift);
    }
    Ok(serde_json::Value::Object(object))
}

/// Parses the raw-evidence JSON object (as re-read from the capsule) into
/// the reading's windows. Every required window must be present, with a used
/// and a limit that parse as micro-cent counts; a reset instant that the
/// provider states is carried through as stated, and one it omits or states
/// as null is the not-started state.
fn parse_state(root: &serde_json::Value) -> Result<Vec<MeterWindow>, FailureClass> {
    let mut windows = Vec::new();
    for kind in [WindowKind::Rolling, WindowKind::Weekly, WindowKind::Monthly] {
        windows.push(kind.parse_window(root)?);
    }
    Ok(windows)
}

/// The three usage windows the response's `access.meters` keys distinguish,
/// each with its semantic key and the nominal length this adapter stores for
/// it: rolling matches the endpoint's own `fiveHour` key, and the two
/// calendar windows are the bead's 7-day and 30-day decisions.
#[derive(Debug, Clone, Copy)]
enum WindowKind {
    Rolling,
    Weekly,
    Monthly,
}

impl WindowKind {
    /// Maps an `access.meters` key to the window it names, matching the
    /// reference tool's own mapping; anything else is not one of the three
    /// required windows and is skipped rather than treated as an error, so a
    /// window the provider adds later cannot fail an observation.
    fn from_meter_name(name: &str) -> Option<Self> {
        match name {
            "fiveHour" => Some(Self::Rolling),
            "week" => Some(Self::Weekly),
            "month" => Some(Self::Monthly),
            _ => None,
        }
    }

    fn semantic_key(self) -> WindowSemanticKey {
        WindowSemanticKey::new(match self {
            WindowKind::Rolling => "rolling",
            WindowKind::Weekly => "weekly",
            WindowKind::Monthly => "monthly",
        })
    }

    fn nominal_duration(self) -> NominalWindowDuration {
        let seconds = match self {
            WindowKind::Rolling => 5 * 3600,
            WindowKind::Weekly => 7 * 24 * 3600,
            WindowKind::Monthly => 30 * 24 * 3600,
        };
        NominalWindowDuration::from_nanos(seconds * 1_000_000_000)
    }

    /// Parses one window object from the raw-evidence root: the provider's
    /// own micro-cent strings and reset instant, validated and converted into
    /// the typed window.
    fn parse_window(self, root: &serde_json::Value) -> Result<MeterWindow, FailureClass> {
        let key = self.semantic_key();
        let object = root
            .get(key.as_str())
            .and_then(|value| value.as_object())
            .ok_or(FailureClass::MissingRequiredField)?;
        let used =
            micro_cents(object.get("used_micro_cents")).ok_or(FailureClass::MalformedBody)?;
        let limit =
            micro_cents(object.get("limit_micro_cents")).ok_or(FailureClass::MalformedBody)?;
        // A limit of zero is a window with no stated ceiling, and a used count
        // above its limit is a ratio the quota fraction cannot hold; both
        // refuse here rather than print a number the provider did not state.
        let ppm = ppm_from_micro_cents(used, limit).ok_or(FailureClass::MalformedBody)?;
        let used_fraction = QuotaFractionPpm::new(ppm).ok_or(FailureClass::MalformedBody)?;
        let reset_state = match object.get("resets_at") {
            None | Some(serde_json::Value::Null) => WindowResetState::NotStarted,
            Some(value) => {
                let text = value.as_str().ok_or(FailureClass::MalformedBody)?;
                let instant =
                    UtcTimestamp::parse_rfc3339(text).ok_or(FailureClass::MalformedBody)?;
                WindowResetState::Known(instant)
            }
        };
        Ok(MeterWindow::new(
            key,
            WindowScope::AccountWide,
            QuotaUsed::new(used_fraction),
            exact_micro_cent_resolution(),
            QuantizationSemantics::Exact,
            reset_state,
            self.nominal_duration(),
        ))
    }
}

/// The reported resolution of an exact micro-cent ratio: one part per
/// million, the finest step the quota fraction can hold. The provider states
/// both sides of the ratio as integers, so the only rounding left is the one
/// this crate performs converting to ppm.
fn exact_micro_cent_resolution() -> ReportedResolution {
    // 1 ppm is statically inside the fraction's domain and non-zero, so both
    // constructors' refusals are unreachable for this constant.
    ReportedResolution::new(QuotaFractionPpm::new(1).expect("one ppm is a valid fraction"))
        .expect("one ppm is a non-zero resolution")
}

impl ProviderAdapter for OpenCodeAdapter {
    type Reading = OpenCodeReading;

    fn declarations(&self) -> AdapterDeclarations {
        self.declarations.clone()
    }

    fn observe(
        &self,
        credential: &CredentialHandle,
        request: &MeterRequest,
        transport: &impl HttpTransport,
        clock: &impl Clock,
    ) -> ProviderObservation<Self::Reading> {
        self.observe_with_evidence(credential, request, transport, clock)
            .observation
    }

    fn observe_with_evidence(
        &self,
        credential: &CredentialHandle,
        request: &MeterRequest,
        transport: &impl HttpTransport,
        clock: &impl Clock,
    ) -> CapturedProviderResponse<Self::Reading> {
        // The credential is the whole `Cookie` header value, decided in
        // `aub-r7k0`: no trimming, no reinterpretation, and an empty material
        // is an expired credential rather than an unauthenticated request.
        let material = credential.expose();
        if material.is_empty() {
            return CapturedProviderResponse::without_response(ProviderObservation::AuthRequired(
                AuthReason::CredentialExpired,
            ));
        }
        // The workspace id is required even under an endpoint override: the
        // endpoint scopes its answer by the header, so an observation without
        // one asks about no workspace at all.
        let Some(workspace_id) = request
            .workspace_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            return CapturedProviderResponse::without_response(ProviderObservation::Unreachable(
                FailureClass::MissingRequiredField,
            ));
        };
        let status_url = self.status_url();

        let timeouts = RequestTimeoutConfig::new(
            MonotonicDuration::from_seconds(5),
            MonotonicDuration::from_seconds(10),
            Some(MonotonicDuration::from_seconds(15)),
        );
        let req = HttpRequest::get(&status_url, timeouts)
            .with_header(COOKIE_HEADER, material.to_string())
            .with_header(ORG_HEADER, workspace_id.to_string())
            .with_header("Accept", "application/json")
            .with_header("User-Agent", "agent-usage-book/0.1.0")
            // A signed-out caller is answered with a status, but the console
            // also redirects some paths to its sign-in page, and a redirect
            // followed would arrive here as a login page with a 200 on it.
            .without_redirects();
        let budget = CommandBudget::new(MonotonicDuration::from_seconds(30), clock);

        let response = match transport.send(&req, &budget, clock) {
            Ok(res) => res,
            Err(failure) => {
                return CapturedProviderResponse::without_response(
                    ProviderObservation::Unreachable(failure),
                );
            }
        };
        // No reset is derived from the receive instant any more: the provider
        // states every reset as an absolute instant, so nothing here needs a
        // local anchor and the clock is left to the transport's own budget.
        let (observation, evidence) = match response.status() {
            200 => reading_from_response(&response, material),
            // A redirect still means the sign-in page, which is why the
            // request refuses to follow one. Measured 2026-09-20, the
            // endpoint answers a refused credential with 401 instead, but a
            // redirect arriving here has no other meaning.
            300..=399 => (
                ProviderObservation::AuthRequired(AuthReason::CredentialExpired),
                None,
            ),
            401 => (
                ProviderObservation::AuthRequired(AuthReason::CredentialRejected),
                None,
            ),
            // A 403 is an ambiguous client error, never a classification:
            // section 34.8 reserves authentication conclusions for providers
            // that state them, and this one states 401 when it means one.
            403 => (
                ProviderObservation::Unreachable(FailureClass::HttpStatus(
                    HttpStatusClass::ClientError,
                )),
                None,
            ),
            429 => (
                ProviderObservation::Unreachable(FailureClass::RateLimited { retry_after: None }),
                None,
            ),
            400..=499 => (
                ProviderObservation::Unreachable(FailureClass::HttpStatus(
                    HttpStatusClass::ClientError,
                )),
                None,
            ),
            500..=599 => (
                ProviderObservation::Unreachable(FailureClass::HttpStatus(
                    HttpStatusClass::ServerError,
                )),
                None,
            ),
            _ => (
                ProviderObservation::Unreachable(FailureClass::HttpStatus(
                    HttpStatusClass::ClientError,
                )),
                None,
            ),
        };
        // On a parse failure the sanitized raw-state body rides along for
        // the bounded failure store (`aub-2r3`); on every other outcome
        // there is no failed body to keep.
        let failed_body = match &observation {
            ProviderObservation::Unreachable(
                FailureClass::MalformedBody | FailureClass::MissingRequiredField,
            ) => evidence.as_ref().and_then(|capsule| {
                capsule
                    .sanitized_body_for_failure()
                    .map(|body| body.to_vec())
            }),
            ProviderObservation::Measured(_)
            | ProviderObservation::AuthRequired(_)
            | ProviderObservation::Unreachable(_) => None,
        };
        let failed_error = error_report_for_observation(
            &observation,
            response.body(),
            response.status(),
            &SensitiveResponseMaterial::new([material]),
        );
        CapturedProviderResponse {
            observation,
            evidence,
            failed_body,
            failed_error,
        }
    }
}

/// Reads one successful status response: the raw-state object is projected
/// out of the body and sanitized into the evidence capsule first, then the
/// windows are parsed from the capsule's own quota subtree, so the retained
/// evidence is exactly what the reading was derived from. A body that is not
/// JSON, or that carries no `access.meters` object, never reaches the capsule
/// at all: it is schema drift with nothing to retain.
fn reading_from_response(
    response: &HttpResponse,
    cookie_material: &str,
) -> (
    ProviderObservation<OpenCodeReading>,
    Option<JsonEvidenceCapsule>,
) {
    // A 200 carrying HTML is the sign-in page served in place of the answer,
    // which is a different state from a JSON body this adapter cannot read.
    if let Some(content_type) = response.header("Content-Type")
        && !content_type.to_ascii_lowercase().contains("json")
    {
        return (
            ProviderObservation::Unreachable(FailureClass::SchemaDrift),
            None,
        );
    }
    let raw_state = match build_raw_state(response.body()) {
        Ok(state) => state,
        Err(FailureClass::SchemaDrift) => {
            return (
                ProviderObservation::Unreachable(FailureClass::SchemaDrift),
                None,
            );
        }
        Err(failure) => {
            return (ProviderObservation::Unreachable(failure), None);
        }
    };
    let sensitive = SensitiveResponseMaterial::new([cookie_material]);
    let raw_state_bytes = serde_json::to_vec(&raw_state).expect("a JSON value always serializes");
    let evidence = capture_json_body(&raw_state_bytes, &sensitive);
    let quota = match quota_response_from_capsule(evidence.serialized()) {
        Ok(quota) => quota,
        Err(_) => {
            return (
                ProviderObservation::Unreachable(FailureClass::MalformedBody),
                Some(evidence),
            );
        }
    };
    let observation = match parse_state(&quota) {
        Ok(windows) => ProviderObservation::Measured(OpenCodeReading::new(windows)),
        Err(failure) => ProviderObservation::Unreachable(failure),
    };
    (observation, Some(evidence))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::FakeClock;
    use std::cell::Cell;

    /// The fixtures under test, included verbatim so the unit cases and the
    /// end-to-end run read the same files the reviewer audits.
    const FIXTURE_VALID: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/meter/opencode/valid.json"
    ));
    const FIXTURE_LOGIN_REDIRECT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/meter/opencode/login-redirect.html"
    ));
    const FIXTURE_NO_MARKER: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/meter/opencode/no-state-marker.json"
    ));
    const FIXTURE_MALFORMED: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/meter/opencode/malformed-state.json"
    ));

    /// The fixture credential material: the whole `Cookie` header value the
    /// adapter receives, distinctive on purpose so the leak grep over the
    /// capsule matches nothing but a leak, and free of every shared forbidden
    /// pattern (no credential-shaped prefix, no at sign, no path).
    const FIXTURE_COOKIE: &str = "auth=fixture-session-cookie-9f2c-not-a-real-value; __Host-console_session=fixture-console-7b1d";
    /// The workspace id the request-shape case serves, matching the
    /// reference's own id shape.
    const FIXTURE_WORKSPACE_ID: &str = "wrk_2345ABCDEFGHJKLMNOPQRSTuvwx";

    /// A scripted transport: one programmed response, plus the record of the
    /// request the adapter built, so a case can assert the URL and the
    /// headers the adapter actually sent.
    struct SyntheticTransport {
        response: Result<HttpResponse, FailureClass>,
        seen_request: Cell<Option<HttpRequest>>,
    }

    impl SyntheticTransport {
        fn serving(body: &[u8]) -> Self {
            Self::serving_as("application/json", body)
        }

        fn serving_as(content_type: &str, body: &[u8]) -> Self {
            Self {
                response: Ok(HttpResponse {
                    status: 200,
                    headers: vec![("Content-Type".to_string(), content_type.to_string())],
                    body: body.to_vec(),
                }),
                seen_request: Cell::new(None),
            }
        }

        fn with_status(status: u16, location: &str, body: &[u8]) -> Self {
            Self {
                response: Ok(HttpResponse {
                    status,
                    headers: vec![("Location".to_string(), location.to_string())],
                    body: body.to_vec(),
                }),
                seen_request: Cell::new(None),
            }
        }

        fn request(&self) -> HttpRequest {
            self.seen_request
                .take()
                .expect("the adapter must issue exactly one request per observation")
        }
    }

    impl HttpTransport for SyntheticTransport {
        fn send(
            &self,
            request: &HttpRequest,
            _budget: &CommandBudget,
            _clock: &impl Clock,
        ) -> Result<HttpResponse, FailureClass> {
            self.seen_request.set(Some(request.clone()));
            self.response.clone()
        }
    }

    fn meter_request(workspace_id: Option<&str>) -> MeterRequest {
        MeterRequest {
            model: None,
            workspace_id: workspace_id.map(str::to_string),
            local_home: None,
            codex_sessions_owned: false,
            anthropic_statusline: None,
        }
    }

    fn observing(transport: &SyntheticTransport) -> CapturedProviderResponse<OpenCodeReading> {
        observing_with(transport, Some(FIXTURE_WORKSPACE_ID), FIXTURE_COOKIE)
    }

    fn observing_with(
        transport: &SyntheticTransport,
        workspace_id: Option<&str>,
        cookie: &str,
    ) -> CapturedProviderResponse<OpenCodeReading> {
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1_000_000_000));
        let adapter = OpenCodeAdapter::new(None);
        adapter.observe_with_evidence(
            &CredentialHandle::new(cookie),
            &meter_request(workspace_id),
            transport,
            &clock,
        )
    }

    fn window_of<'a>(reading: &'a OpenCodeReading, key: &str) -> &'a MeterWindow {
        reading
            .windows
            .iter()
            .find(|window| window.semantic_key().as_str() == key)
            .unwrap_or_else(|| panic!("the {key} window must be present"))
    }

    /// Case 01: the valid body parses to the three required windows, each
    /// carrying the exact used-over-limit ratio in parts per million. The
    /// expected values are the arithmetic written out, so a change to the
    /// rounding rule fails here rather than drifting silently: 137731547 of
    /// 3000000000 is 45910.5 ppm and rounds half away from zero to 45911.
    #[test]
    fn case_01_valid_body_yields_three_windows_at_exact_micro_cent_resolution() {
        let transport = SyntheticTransport::serving(FIXTURE_VALID.as_bytes());
        let captured = observing(&transport);
        let ProviderObservation::Measured(reading) = captured.observation else {
            panic!("the valid fixture must measure: {:?}", captured.observation);
        };
        assert_eq!(reading.windows.len(), 3);
        for (key, expected_ppm) in [("rolling", 0), ("weekly", 45_911), ("monthly", 115_963)] {
            let window = window_of(&reading, key);
            assert_eq!(
                window.quota_used().as_ppm().get(),
                expected_ppm,
                "the {key} micro-cent ratio converts to ppm exactly"
            );
            assert_eq!(
                window.reported_resolution().as_ppm().get(),
                1,
                "an integer ratio is reported at one ppm, not at the page's one tenth of a percent"
            );
            assert_eq!(window.quantization(), QuantizationSemantics::Exact);
        }
        assert!(
            captured.evidence.is_some(),
            "a measured reading retains its capsule"
        );
    }

    /// Case 02: the reset instant the provider states is carried through as
    /// stated, and a window that states none is the not-started state rather
    /// than an instant derived from anything else on the response. This is
    /// `aub-eun.15`'s decision applied, and the negative half is what stops a
    /// later implementation inferring a monthly reset from the subscription
    /// period.
    #[test]
    fn case_02_a_stated_reset_is_known_and_an_absent_one_is_not_started() {
        let transport = SyntheticTransport::serving(FIXTURE_VALID.as_bytes());
        let captured = observing(&transport);
        let ProviderObservation::Measured(reading) = captured.observation else {
            panic!("the valid fixture must measure: {:?}", captured.observation);
        };
        let weekly = window_of(&reading, "weekly");
        assert_eq!(
            weekly.resets_at(),
            UtcTimestamp::parse_rfc3339("2026-09-21T00:00:00.000Z"),
            "the weekly reset is the instant the provider stated"
        );
        assert!(weekly.reset_state().is_known());
        for key in ["rolling", "monthly"] {
            let window = window_of(&reading, key);
            assert!(
                window.reset_state().is_not_started(),
                "the {key} window states no reset instant, so it is not started"
            );
            assert_eq!(window.resets_at(), None);
        }
    }

    /// Case 03: a redirect is still the sign-in redirect. The endpoint
    /// answers a refused credential with a status, but the request refuses to
    /// follow a redirect precisely so one cannot arrive as a 200 carrying a
    /// login page.
    #[test]
    fn case_03_login_redirect_is_auth_required() {
        let transport = SyntheticTransport::with_status(
            302,
            "https://opencode.ai/console/login",
            FIXTURE_LOGIN_REDIRECT.as_bytes(),
        );
        let captured = observing(&transport);
        assert!(
            matches!(
                captured.observation,
                ProviderObservation::AuthRequired(AuthReason::CredentialExpired)
            ),
            "a redirect is the sign-in redirect: {:?}",
            captured.observation
        );
        assert!(captured.evidence.is_none());
    }

    /// Case 04: a stated 401 is the authentication conclusion this endpoint
    /// gives, and it is `CredentialRejected` rather than `CredentialExpired`
    /// because the provider states a refusal and not a reason for it.
    #[test]
    fn case_04_status_401_is_a_rejected_credential() {
        let transport = SyntheticTransport::with_status(401, "", b"{\"error\":\"unauthorized\"}");
        let captured = observing(&transport);
        assert!(
            matches!(
                captured.observation,
                ProviderObservation::AuthRequired(AuthReason::CredentialRejected)
            ),
            "a stated 401 is a rejected credential: {:?}",
            captured.observation
        );
    }

    /// Case 05: a JSON body carrying no `access.meters` object is schema
    /// drift, never a silent zero.
    #[test]
    fn case_05_body_without_the_meters_object_is_schema_drift() {
        let transport = SyntheticTransport::serving(FIXTURE_NO_MARKER.as_bytes());
        let captured = observing(&transport);
        assert!(
            matches!(
                captured.observation,
                ProviderObservation::Unreachable(FailureClass::SchemaDrift)
            ),
            "a body with no meters object is schema drift: {:?}",
            captured.observation
        );
        assert!(
            captured.evidence.is_none(),
            "schema drift has no quota evidence to retain"
        );
    }

    /// Case 06: a 200 whose content type is not JSON is the sign-in page
    /// served in place of the answer. It is schema drift and it never reaches
    /// the body parser, which would otherwise call it a malformed body and
    /// hide what actually happened.
    #[test]
    fn case_06_a_two_hundred_carrying_html_is_schema_drift() {
        let transport =
            SyntheticTransport::serving_as("text/html; charset=utf-8", b"<html>sign in</html>");
        let captured = observing(&transport);
        assert!(
            matches!(
                captured.observation,
                ProviderObservation::Unreachable(FailureClass::SchemaDrift)
            ),
            "an HTML 200 is schema drift: {:?}",
            captured.observation
        );
    }

    /// Case 07: a meter whose micro-cent counts do not parse is the parse
    /// failure class, and its sanitized body is retained for the bounded
    /// failure store.
    #[test]
    fn case_07_malformed_state_is_the_parse_failure_class() {
        let transport = SyntheticTransport::serving(FIXTURE_MALFORMED.as_bytes());
        let captured = observing(&transport);
        assert!(
            matches!(
                captured.observation,
                ProviderObservation::Unreachable(FailureClass::MalformedBody)
            ),
            "an unparsable micro-cent count is a malformed body: {:?}",
            captured.observation
        );
        assert!(
            captured.failed_body.is_some(),
            "a parse failure retains its sanitized body"
        );
    }

    /// Case 08: a window whose limit is zero states no ceiling, and the ratio
    /// is undefined rather than zero. The planted negative: an implementation
    /// that divides and falls back to zero on a zero denominator would report
    /// a comfortable 0% for a window nobody can spend against, which is the
    /// one wrong answer nothing downstream could detect.
    #[test]
    fn case_08_a_zero_limit_is_not_zero_usage() {
        let body = br#"{"access":{"meters":{
            "fiveHour":{"usedMicroCents":"0","limitMicroCents":"0","resetsAt":null},
            "week":{"usedMicroCents":"1","limitMicroCents":"2","resetsAt":null},
            "month":{"usedMicroCents":"1","limitMicroCents":"2","resetsAt":null}}}}"#;
        let transport = SyntheticTransport::serving(body);
        let captured = observing(&transport);
        assert!(
            matches!(
                captured.observation,
                ProviderObservation::Unreachable(FailureClass::MalformedBody)
            ),
            "a zero limit refuses instead of reading as zero usage: {:?}",
            captured.observation
        );
    }

    /// Case 09: a used count above its limit is a ratio the quota fraction
    /// cannot hold, and it refuses rather than saturating at the ceiling.
    #[test]
    fn case_09_a_used_count_above_its_limit_refuses() {
        let body = br#"{"access":{"meters":{
            "fiveHour":{"usedMicroCents":"3","limitMicroCents":"2","resetsAt":null},
            "week":{"usedMicroCents":"1","limitMicroCents":"2","resetsAt":null},
            "month":{"usedMicroCents":"1","limitMicroCents":"2","resetsAt":null}}}}"#;
        let transport = SyntheticTransport::serving(body);
        let captured = observing(&transport);
        assert!(
            matches!(
                captured.observation,
                ProviderObservation::Unreachable(FailureClass::MalformedBody)
            ),
            "a ratio above one refuses: {:?}",
            captured.observation
        );
    }

    /// Case 10: a required window the response omits is a missing field, not
    /// a window quietly dropped from the reading.
    #[test]
    fn case_10_a_missing_required_window_is_a_missing_field() {
        let body = br#"{"access":{"meters":{
            "fiveHour":{"usedMicroCents":"1","limitMicroCents":"2","resetsAt":null},
            "week":{"usedMicroCents":"1","limitMicroCents":"2","resetsAt":null}}}}"#;
        let transport = SyntheticTransport::serving(body);
        let captured = observing(&transport);
        assert!(
            matches!(
                captured.observation,
                ProviderObservation::Unreachable(FailureClass::MissingRequiredField)
            ),
            "an absent monthly window is a missing required field: {:?}",
            captured.observation
        );
    }

    #[test]
    fn the_declarations_carry_the_contract_semantics_and_required_windows() {
        let declarations = OpenCodeAdapter::new(None).declarations();
        assert_eq!(
            declarations.provider_contract_id.as_str(),
            "opencode-go-console-status-v1",
            "the contract id names the JSON surface, so evidence from the page revision is \
             distinguishable by contract id alone"
        );
        assert_eq!(
            declarations.meter_semantics_id.as_str(),
            "opencode-go-subscription-v1"
        );
        for kind in ["rolling", "weekly", "monthly"] {
            assert!(
                declarations.required_window_kinds.contains(kind),
                "the {kind} window is required"
            );
        }
        assert_eq!(
            declarations.measurement_basis,
            MeasurementBasis::LocallyReceived
        );
    }

    #[test]
    fn the_declarations_carry_the_one_second_reset_precision() {
        let declarations = OpenCodeAdapter::new(None).declarations();
        // The literal, not the constant: the value is the pin. A test that
        // reads the declaration back through the constant it was written from
        // would pass for any constant the constructor happened to carry.
        assert_eq!(
            declarations.reset_precision,
            Some(ResetPrecision::from_seconds(1).expect("one second is a non-zero second count")),
            "an absolute instant is precise to the second, not to the hour the page text forced"
        );
    }

    #[test]
    fn the_request_carries_both_cookies_the_workspace_header_and_the_status_url() {
        let transport = SyntheticTransport::serving(FIXTURE_VALID.as_bytes());
        let _ = observing(&transport);
        let request = transport.request();
        assert_eq!(request.url, "https://opencode.ai/console/api/go/status");
        let mut cookie_headers = 0;
        let mut org_headers = 0;
        for (name, value) in &request.headers {
            if name.eq_ignore_ascii_case(COOKIE_HEADER) {
                cookie_headers += 1;
                assert_eq!(
                    value, FIXTURE_COOKIE,
                    "the material is sent verbatim, with no cookie pair rebuilt by the adapter"
                );
            } else if name.eq_ignore_ascii_case(ORG_HEADER) {
                org_headers += 1;
                assert_eq!(value, FIXTURE_WORKSPACE_ID);
            } else {
                assert!(
                    !value.contains("fixture-session-cookie"),
                    "no header but {COOKIE_HEADER} may carry the cookie material"
                );
            }
        }
        assert_eq!(cookie_headers, 1, "exactly one Cookie header goes out");
        assert_eq!(org_headers, 1, "exactly one workspace header goes out");
    }

    #[test]
    fn the_endpoint_override_replaces_the_status_url() {
        let transport = SyntheticTransport::serving(FIXTURE_VALID.as_bytes());
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1_000_000_000));
        let adapter = OpenCodeAdapter::new(Some("http://127.0.0.1:8081".to_string()));
        let _ = adapter.observe_with_evidence(
            &CredentialHandle::new(FIXTURE_COOKIE),
            &meter_request(Some(FIXTURE_WORKSPACE_ID)),
            &transport,
            &clock,
        );
        assert_eq!(transport.request().url, "http://127.0.0.1:8081");
    }

    /// The workspace id rides in a header rather than in the path, so an
    /// endpoint override cannot stand in for it the way it could when the id
    /// was part of the page URL.
    #[test]
    fn an_observation_without_a_workspace_id_is_a_missing_field() {
        let transport = SyntheticTransport::serving(FIXTURE_VALID.as_bytes());
        let captured = observing_with(&transport, None, FIXTURE_COOKIE);
        assert!(
            matches!(
                captured.observation,
                ProviderObservation::Unreachable(FailureClass::MissingRequiredField)
            ),
            "no workspace id is a missing required field: {:?}",
            captured.observation
        );
    }

    #[test]
    fn an_empty_credential_is_an_expired_one() {
        let transport = SyntheticTransport::serving(FIXTURE_VALID.as_bytes());
        let captured = observing_with(&transport, Some(FIXTURE_WORKSPACE_ID), "");
        assert!(matches!(
            captured.observation,
            ProviderObservation::AuthRequired(AuthReason::CredentialExpired)
        ));
    }

    /// The capsule keeps the provider's own micro-cent strings and its reset
    /// instants, which is what makes the reading auditable against the
    /// evidence rather than against a number this crate computed. It is also
    /// the fact `aub-4z5c` turns on: the currency the domain discards at
    /// parse time survives here, so deciding later costs nothing.
    #[test]
    fn the_capsule_holds_the_micro_cent_counts_and_never_the_cookie() {
        let transport = SyntheticTransport::serving(FIXTURE_VALID.as_bytes());
        let captured = observing(&transport);
        let capsule = captured.evidence.expect("a measured reading has a capsule");
        let serialized = capsule.serialized();
        for needle in [
            "137731547",
            "3000000000",
            "695777296",
            "2026-09-21T00:00:00.000Z",
        ] {
            assert!(
                serialized.contains(needle),
                "the capsule retains {needle} exactly as the provider stated it"
            );
        }
        assert!(
            !serialized.contains("fixture-session-cookie"),
            "the capsule never carries the credential"
        );
    }

    /// The response also carries a subscriber id and a payment-method id. The
    /// projection is what keeps them out of the capsule, so the assertion is
    /// over a body that contains both.
    #[test]
    fn the_capsule_never_carries_the_account_identifiers() {
        let body =
            br#"{"subscriberUserId":"sub_fixture_not_real","paymentMethodId":"pm_fixture_not_real",
            "access":{"meters":{
            "fiveHour":{"usedMicroCents":"1","limitMicroCents":"2","resetsAt":null},
            "week":{"usedMicroCents":"1","limitMicroCents":"2","resetsAt":null},
            "month":{"usedMicroCents":"1","limitMicroCents":"2","resetsAt":null}}}}"#;
        let transport = SyntheticTransport::serving(body);
        let captured = observing(&transport);
        let capsule = captured.evidence.expect("a measured reading has a capsule");
        let serialized = capsule.serialized();
        for needle in ["sub_fixture_not_real", "pm_fixture_not_real"] {
            assert!(
                !serialized.contains(needle),
                "the capsule must not retain {needle}"
            );
        }
    }

    #[test]
    fn the_sanitizer_removes_the_cookie_even_when_the_body_echoes_it() {
        let body = format!(
            r#"{{"access":{{"meters":{{
                "fiveHour":{{"usedMicroCents":"1","limitMicroCents":"2","resetsAt":null,"echo":"{FIXTURE_COOKIE}"}},
                "week":{{"usedMicroCents":"1","limitMicroCents":"2","resetsAt":null}},
                "month":{{"usedMicroCents":"1","limitMicroCents":"2","resetsAt":null}}}}}}}}"#
        );
        let transport = SyntheticTransport::serving(body.as_bytes());
        let captured = observing(&transport);
        let capsule = captured.evidence.expect("a measured reading has a capsule");
        let serialized = capsule.serialized();
        assert!(
            !serialized.contains("fixture-session-cookie"),
            "an echoed credential is sanitized out of the capsule"
        );
    }

    #[test]
    fn case_11_error_429_stores_the_status_spelling() {
        let transport = SyntheticTransport::with_status(429, "", b"{\"error\":\"slow down\"}");
        let captured = observing(&transport);
        assert!(matches!(
            captured.observation,
            ProviderObservation::Unreachable(FailureClass::RateLimited { retry_after: None })
        ));
        let report = captured
            .failed_error
            .as_ref()
            .expect("a 429 response stores the provider's error report");
        assert_eq!(report.classification, "http_429");
        assert_eq!(report.message, "");
    }
}
