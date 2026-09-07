//! The Ollama Cloud provider meter adapter (`aub-ud17`).
//!
//! Implements [`ProviderAdapter`] for the Ollama Cloud usage endpoint
//! (`GET https://ollama.com/api/usage`), authenticating with the raw API key
//! in an `Authorization` header carrying no `Bearer` scheme
//! (`bin/ollama-quota:16,118-123` in llm-workflow is the working reference
//! this adapter matches).
//!
//! # Why the reset state is `Scheduled`, not `Known`
//!
//! The response reports usage for a `session` (5 h) and a `weekly` (7 d)
//! window, but never a reset instant for either. The service nonetheless
//! resets on a fixed, measured grid (`bin/ollama-quota:83-85`): a session
//! boundary every [`OllamaAdapter::SESSION_WINDOW_SECS`] seconds from the
//! Unix epoch, and a weekly boundary every
//! [`OllamaAdapter::WEEKLY_WINDOW_SECS`] seconds, offset
//! [`OllamaAdapter::WEEKLY_OFFSET_SECS`] seconds from it (a Monday-00:00-UTC
//! boundary). That grid is evidence about the provider's own behaviour, not
//! about any one sample, so it lives on the adapter rather than being
//! invented per response, and the computed instant is tagged
//! [`crate::domain::window::WindowResetState::Scheduled`] rather than
//! `Known`: reusing `Known` would let the anomaly detector treat this
//! adapter's own arithmetic, recomputed on every observation, as a
//! provider-reported reset event.
//!
//! # `usage` is a fraction, not an integer percent
//!
//! The endpoint's `usage` fields (`.limits.session.usage`,
//! `.limits.weekly.usage`) are floats in `[0, 1]` of the window consumed
//! (`0.163` means 16.3% used), confirmed against `bin/ollama-quota`'s own
//! test fixtures (`scripts/ollama-quota-test.sh:110-120`, which assert
//! `usage_body 0.163 ...` renders as `"16.3%"`). `quota_used_ppm` is
//! therefore `round(usage * 1_000_000)`.
//!
//! # Boundary rules
//!
//! May not depend on:
//! - SQLite directly (rule `03`)
//! - credential or configuration modules (rule `07`)
//! - the ureq transport driver (rule `12`)
//! - write-capable filesystem facilities (rule `17`)
//! - presentation or calibration modules

use crate::domain::failure::{AuthReason, FailureClass, HttpStatusClass};
use crate::domain::ids::{MeterSemanticsId, ProviderContractId};
use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
use crate::domain::time::{Clock, MeasurementBasis, MonotonicDuration, UtcTimestamp};
use crate::domain::window::{
    MeterWindow, NominalWindowDuration, QuantizationSemantics, ReportedResolution, ResetGridId,
    WindowResetState, WindowScope, WindowSemanticKey,
};
use crate::meter::adapter::{
    AdapterDeclarations, CredentialHandle, HttpTransport, MeterRequest, ProviderAdapter,
    ProviderObservation, RequiredWindowKinds,
};
use crate::meter::evidence::{
    CapturedProviderResponse, SensitiveResponseMaterial, capture_json_body, capture_json_response,
    quota_response_from_capsule,
};
use crate::meter::transport::{CommandBudget, HttpRequest, HttpResponse, RequestTimeoutConfig};

/// The typed observation reading produced by [`OllamaAdapter`].
///
/// Deliberately minimal next to [`crate::meter::anthropic::AnthropicReading`]:
/// this contract has no legacy response shape to replay, no per-model
/// scoping, and no provider-declared activity or severity facts, so there is
/// nothing else honest to carry yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OllamaReading {
    pub windows: Vec<MeterWindow>,
}

/// The Ollama Cloud usage provider adapter.
pub struct OllamaAdapter {
    endpoint_url: String,
    declarations: AdapterDeclarations,
}

impl Default for OllamaAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl OllamaAdapter {
    pub const DEFAULT_ENDPOINT: &'static str = "https://ollama.com/api/usage";
    pub const CONTRACT_ID: &'static str = "ollama-cloud-usage-v1";
    pub const SEMANTICS_ID: &'static str = "ollama-cloud-subscription-v1";
    pub const REQUIRED_WINDOW_KINDS: &'static [&'static str] = &["session", "weekly"];

    /// The fixed reset grid this adapter computes `Scheduled` boundaries
    /// from, measured against the live service (`aub-ud17`,
    /// `bin/ollama-quota:83-85` in llm-workflow): a 5-hour session grid
    /// aligned to the Unix epoch.
    pub const SESSION_WINDOW_SECS: i64 = 18_000;
    /// The weekly grid's period: 7 days.
    pub const WEEKLY_WINDOW_SECS: i64 = 604_800;
    /// The weekly grid's offset from the Unix epoch: 4 days, which lands its
    /// boundary on Monday 00:00 UTC.
    pub const WEEKLY_OFFSET_SECS: i64 = 345_600;

    pub fn new() -> Self {
        Self::with_endpoint(Self::DEFAULT_ENDPOINT)
    }

    pub fn with_endpoint(endpoint_url: impl Into<String>) -> Self {
        Self {
            endpoint_url: endpoint_url.into(),
            declarations: AdapterDeclarations::new(
                MeasurementBasis::LocallyReceived,
                ProviderContractId::new(Self::CONTRACT_ID),
                MeterSemanticsId::new(Self::SEMANTICS_ID),
            )
            .with_required_window_kinds(RequiredWindowKinds::from_values(
                Self::REQUIRED_WINDOW_KINDS,
            )),
        }
    }

    pub fn endpoint_url(&self) -> &str {
        &self.endpoint_url
    }
}

/// The next grid boundary strictly after `after_secs`, on a grid of period
/// `window_secs` offset `offset_secs` from the Unix epoch.
///
/// A reading taken exactly on a boundary belongs to the window that just
/// closed, so the boundary itself is never returned: the result is always
/// strictly greater than `after_secs` (`aub-ud17`'s literal grid-arithmetic
/// acceptance case).
fn next_grid_boundary_secs(after_secs: i64, window_secs: i64, offset_secs: i64) -> i64 {
    let phase = (after_secs - offset_secs).rem_euclid(window_secs);
    if phase == 0 {
        after_secs + window_secs
    } else {
        after_secs + (window_secs - phase)
    }
}

/// The `Scheduled` reset state for one window, computed from `received_at`
/// against the named grid.
fn scheduled_reset(
    received_at: UtcTimestamp,
    window_secs: i64,
    offset_secs: i64,
) -> WindowResetState {
    let received_secs = received_at.unix_nanos().div_euclid(1_000_000_000);
    let boundary_secs = next_grid_boundary_secs(received_secs, window_secs, offset_secs);
    WindowResetState::Scheduled {
        at: UtcTimestamp::from_unix_nanos(boundary_secs * 1_000_000_000),
        grid: ResetGridId::OllamaCloudV1,
    }
}

/// Converts a `usage` fraction in `[0, 1]` into parts per million. See the
/// module documentation for why this is a fraction and not an integer
/// percent.
fn usage_fraction_to_ppm(usage: f64) -> Option<QuotaFractionPpm> {
    if !usage.is_finite() || !(0.0..=1.0).contains(&usage) {
        return None;
    }
    QuotaFractionPpm::new((usage * 1_000_000.0).round() as i32)
}

/// Parses one `limits.<kind>` object into a typed, account-wide window.
fn parse_window(
    object: &serde_json::Map<String, serde_json::Value>,
    semantic_key: &'static str,
    window_secs: i64,
    offset_secs: i64,
    received_at: UtcTimestamp,
) -> Result<MeterWindow, FailureClass> {
    let usage = object
        .get("usage")
        .and_then(serde_json::Value::as_f64)
        .ok_or(FailureClass::MissingRequiredField)?;
    let quota_used = usage_fraction_to_ppm(usage).ok_or(FailureClass::MissingRequiredField)?;
    let resolution = ReportedResolution::new(
        QuotaFractionPpm::new(10_000).expect("10_000 is in the quota fraction range"),
    )
    .expect("10_000 is a non-zero resolution");
    let resets_at = scheduled_reset(received_at, window_secs, offset_secs);
    let nominal_duration = NominalWindowDuration::from_nanos(window_secs as u64 * 1_000_000_000);
    Ok(MeterWindow::new(
        WindowSemanticKey::new(semantic_key),
        WindowScope::AccountWide,
        QuotaUsed::new(quota_used),
        resolution,
        QuantizationSemantics::Unknown,
        resets_at,
        nominal_duration,
    ))
}

/// Parses the JSON response body from Ollama Cloud's `/api/usage`.
pub fn parse_ollama_usage_body(
    body: &[u8],
    received_at: UtcTimestamp,
) -> Result<OllamaReading, FailureClass> {
    let capsule = capture_json_body(body, &SensitiveResponseMaterial::default());
    replay_ollama_capsule(capsule.serialized(), received_at)
}

/// Reinterprets retained response evidence with the current Ollama
/// semantics, the same replay seam [`crate::meter::anthropic`] uses.
pub fn replay_ollama_capsule(
    capsule: &str,
    received_at: UtcTimestamp,
) -> Result<OllamaReading, FailureClass> {
    let val = quota_response_from_capsule(capsule).map_err(|message| {
        if message == "capsule does not contain a quota response" {
            FailureClass::MalformedBody
        } else {
            FailureClass::MissingRequiredField
        }
    })?;
    let root = val.as_object().ok_or(FailureClass::MalformedBody)?;
    let limits = root
        .get("limits")
        .and_then(serde_json::Value::as_object)
        .ok_or(FailureClass::MissingRequiredField)?;
    let session_obj = limits
        .get("session")
        .and_then(serde_json::Value::as_object)
        .ok_or(FailureClass::MissingRequiredField)?;
    let weekly_obj = limits
        .get("weekly")
        .and_then(serde_json::Value::as_object)
        .ok_or(FailureClass::MissingRequiredField)?;

    let session_window = parse_window(
        session_obj,
        "session",
        OllamaAdapter::SESSION_WINDOW_SECS,
        0,
        received_at,
    )?;
    let weekly_window = parse_window(
        weekly_obj,
        "weekly",
        OllamaAdapter::WEEKLY_WINDOW_SECS,
        OllamaAdapter::WEEKLY_OFFSET_SECS,
        received_at,
    )?;

    Ok(OllamaReading {
        windows: vec![session_window, weekly_window],
    })
}

fn parse_401_auth_reason(_body: &[u8]) -> AuthReason {
    // Unlike Anthropic, Ollama Cloud's 401 body carries no machine-readable
    // distinction between an invalid and an expired key (`bin/ollama-quota`
    // reports both as a plain HTTP status), so every 401 is a rejected
    // credential rather than a provider-declared expiry.
    AuthReason::CredentialRejected
}

fn parse_retry_after(response: &HttpResponse) -> Option<MonotonicDuration> {
    let header_val = response.header("retry-after")?;
    let secs = header_val.trim().parse::<u64>().ok()?;
    Some(MonotonicDuration::from_seconds(secs))
}

impl ProviderAdapter for OllamaAdapter {
    type Reading = OllamaReading;

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
        _request: &MeterRequest,
        transport: &impl HttpTransport,
        clock: &impl Clock,
    ) -> CapturedProviderResponse<Self::Reading> {
        let material = credential.expose().trim();
        if material.is_empty() {
            return CapturedProviderResponse::without_response(ProviderObservation::AuthRequired(
                AuthReason::CredentialExpired,
            ));
        }

        let timeouts = RequestTimeoutConfig::new(
            MonotonicDuration::from_seconds(5),
            MonotonicDuration::from_seconds(10),
            Some(MonotonicDuration::from_seconds(15)),
        );

        let req = HttpRequest::get(&self.endpoint_url, timeouts)
            .with_header("Authorization", material)
            .with_header("Accept", "application/json")
            .with_header("User-Agent", "agent-usage-book/0.1.0");

        let budget = CommandBudget::new(MonotonicDuration::from_seconds(30), clock);

        let response = match transport.send(&req, &budget, clock) {
            Ok(res) => res,
            Err(failure) => {
                return CapturedProviderResponse::without_response(
                    ProviderObservation::Unreachable(failure),
                );
            }
        };

        let sensitive = SensitiveResponseMaterial::new([credential.expose(), material]);
        let evidence = capture_json_response(&response, &sensitive);
        let received_at = clock.now();
        let observation = match response.status() {
            200 => match replay_ollama_capsule(evidence.serialized(), received_at) {
                Ok(reading) => ProviderObservation::Measured(reading),
                Err(failure) => ProviderObservation::Unreachable(failure),
            },
            401 => ProviderObservation::AuthRequired(parse_401_auth_reason(response.body())),
            429 => {
                let retry_after = parse_retry_after(&response);
                ProviderObservation::Unreachable(FailureClass::RateLimited { retry_after })
            }
            400..=499 => ProviderObservation::Unreachable(FailureClass::HttpStatus(
                HttpStatusClass::ClientError,
            )),
            500..=599 => ProviderObservation::Unreachable(FailureClass::HttpStatus(
                HttpStatusClass::ServerError,
            )),
            _ => ProviderObservation::Unreachable(FailureClass::HttpStatus(
                HttpStatusClass::ClientError,
            )),
        };
        let failed_body = match &observation {
            ProviderObservation::Unreachable(
                FailureClass::MalformedBody | FailureClass::MissingRequiredField,
            ) => evidence
                .sanitized_body_for_failure()
                .map(|body| body.to_vec()),
            ProviderObservation::Measured(_)
            | ProviderObservation::AuthRequired(_)
            | ProviderObservation::Unreachable(_) => None,
        };

        CapturedProviderResponse {
            observation,
            evidence: Some(evidence),
            failed_body,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::FakeClock;
    use crate::domain::window::WindowScope;

    struct MockTransport {
        response: Result<HttpResponse, FailureClass>,
    }

    impl MockTransport {
        fn ok(status: u16, body: impl Into<Vec<u8>>) -> Self {
            Self {
                response: Ok(HttpResponse {
                    status,
                    headers: Vec::new(),
                    body: body.into(),
                }),
            }
        }
    }

    impl HttpTransport for MockTransport {
        fn send(
            &self,
            _request: &HttpRequest,
            _budget: &CommandBudget,
            _clock: &impl Clock,
        ) -> Result<HttpResponse, FailureClass> {
            self.response.clone()
        }
    }

    /// A transport that records every request it was handed, so a test can
    /// assert on the outgoing method, URL and headers rather than only on
    /// the parsed result.
    struct RecordingTransport {
        response: Result<HttpResponse, FailureClass>,
        sent: std::cell::RefCell<Vec<HttpRequest>>,
    }

    impl RecordingTransport {
        fn ok(status: u16, body: impl Into<Vec<u8>>) -> Self {
            Self {
                response: Ok(HttpResponse {
                    status,
                    headers: Vec::new(),
                    body: body.into(),
                }),
                sent: std::cell::RefCell::new(Vec::new()),
            }
        }
    }

    impl HttpTransport for RecordingTransport {
        fn send(
            &self,
            request: &HttpRequest,
            _budget: &CommandBudget,
            _clock: &impl Clock,
        ) -> Result<HttpResponse, FailureClass> {
            self.sent.borrow_mut().push(request.clone());
            self.response.clone()
        }
    }

    const FIXTURE_VALID: &[u8] = include_bytes!("../../tests/fixtures/meter/ollama/valid.json");
    const FIXTURE_ZERO_USAGE: &[u8] =
        include_bytes!("../../tests/fixtures/meter/ollama/zero-usage.json");
    const FIXTURE_MISSING_WEEKLY: &[u8] =
        include_bytes!("../../tests/fixtures/meter/ollama/missing-weekly.json");
    const FIXTURE_ERROR_401: &[u8] =
        include_bytes!("../../tests/fixtures/meter/ollama/error-401.json");
    const FIXTURE_MALFORMED: &[u8] =
        include_bytes!("../../tests/fixtures/meter/ollama/malformed.json");

    fn test_adapter() -> OllamaAdapter {
        OllamaAdapter::new()
    }

    fn test_credential() -> CredentialHandle {
        CredentialHandle::new("ollama-test-key-12345")
    }

    /// A fixed instant well clear of any grid boundary, so a fixture test's
    /// windows are unambiguously "in the middle" of both grids rather than
    /// accidentally landing on one.
    fn test_clock() -> FakeClock {
        FakeClock::new(UtcTimestamp::from_unix_nanos(1_788_728_400_000_000_000))
    }

    /// Destructures a successful reading from the observation. The exhaustive
    /// three-arm match over [`ProviderObservation`] keeps the crate-wide
    /// `clippy::wildcard_enum_match_arm` deny satisfied: each variant is
    /// named.
    fn expect_measured(obs: ProviderObservation<OllamaReading>) -> OllamaReading {
        match obs {
            ProviderObservation::Measured(reading) => reading,
            ProviderObservation::AuthRequired(reason) => {
                panic!("expected Measured, got AuthRequired({reason:?})")
            }
            ProviderObservation::Unreachable(failure) => {
                panic!("expected Measured, got Unreachable({failure:?})")
            }
        }
    }

    #[test]
    fn case_01_valid() {
        let adapter = test_adapter();
        let transport = MockTransport::ok(200, FIXTURE_VALID);
        let clock = test_clock();
        let obs = adapter.observe(
            &test_credential(),
            &MeterRequest::default(),
            &transport,
            &clock,
        );

        let reading = expect_measured(obs);
        assert_eq!(reading.windows.len(), 2);

        let session = &reading.windows[0];
        assert_eq!(session.semantic_key().as_str(), "session");
        assert_eq!(*session.scope(), WindowScope::AccountWide);
        assert_eq!(session.quota_used().as_ppm().get(), 163_000);
        assert_eq!(session.reported_resolution().as_ppm().get(), 10_000);
        assert_eq!(
            session.nominal_duration().as_nanos(),
            18_000 * 1_000_000_000
        );

        let weekly = &reading.windows[1];
        assert_eq!(weekly.semantic_key().as_str(), "weekly");
        assert_eq!(*weekly.scope(), WindowScope::AccountWide);
        assert_eq!(weekly.quota_used().as_ppm().get(), 406_000);
        assert_eq!(
            weekly.nominal_duration().as_nanos(),
            604_800 * 1_000_000_000
        );

        // received_at = 2026-09-06T21:00:00Z (epoch 1_788_728_400): the
        // session grid's next boundary is 2026-09-06T22:00:00Z and the
        // weekly grid's is 2026-09-07T00:00:00Z (aub-ud17's literal cases).
        assert_eq!(
            session.reset_state(),
            WindowResetState::Scheduled {
                at: UtcTimestamp::from_unix_nanos(1_788_732_000_000_000_000),
                grid: ResetGridId::OllamaCloudV1,
            }
        );
        assert_eq!(
            weekly.reset_state(),
            WindowResetState::Scheduled {
                at: UtcTimestamp::from_unix_nanos(1_788_739_200_000_000_000),
                grid: ResetGridId::OllamaCloudV1,
            }
        );
    }

    #[test]
    fn case_02_zero_usage() {
        let adapter = test_adapter();
        let transport = MockTransport::ok(200, FIXTURE_ZERO_USAGE);
        let clock = test_clock();
        let obs = adapter.observe(
            &test_credential(),
            &MeterRequest::default(),
            &transport,
            &clock,
        );

        let reading = expect_measured(obs);
        assert_eq!(reading.windows.len(), 2);
        assert_eq!(reading.windows[0].quota_used().as_ppm().get(), 0);
        assert_eq!(reading.windows[1].quota_used().as_ppm().get(), 0);
    }

    #[test]
    fn case_03_missing_weekly() {
        let adapter = test_adapter();
        let transport = MockTransport::ok(200, FIXTURE_MISSING_WEEKLY);
        let clock = test_clock();
        let obs = adapter.observe(
            &test_credential(),
            &MeterRequest::default(),
            &transport,
            &clock,
        );

        assert_eq!(
            obs,
            ProviderObservation::Unreachable(FailureClass::MissingRequiredField)
        );
    }

    #[test]
    fn case_04_error_401() {
        let adapter = test_adapter();
        let transport = MockTransport::ok(401, FIXTURE_ERROR_401);
        let clock = test_clock();
        let obs = adapter.observe(
            &test_credential(),
            &MeterRequest::default(),
            &transport,
            &clock,
        );

        assert_eq!(
            obs,
            ProviderObservation::AuthRequired(AuthReason::CredentialRejected)
        );
    }

    #[test]
    fn case_05_malformed() {
        let adapter = test_adapter();
        let transport = MockTransport::ok(200, FIXTURE_MALFORMED);
        let clock = test_clock();
        let obs = adapter.observe(
            &test_credential(),
            &MeterRequest::default(),
            &transport,
            &clock,
        );

        assert_eq!(
            obs,
            ProviderObservation::Unreachable(FailureClass::MalformedBody)
        );
    }

    /// The request contract: `GET /api/usage` with `Authorization: <material>`
    /// exactly, no `Bearer` scheme, asserted against the recorded outgoing
    /// request rather than only against the parsed result.
    #[test]
    fn request_is_get_with_bare_authorization_header() {
        let adapter = test_adapter();
        let transport = RecordingTransport::ok(200, FIXTURE_VALID);
        let clock = test_clock();
        let _ = adapter.observe(
            &test_credential(),
            &MeterRequest::default(),
            &transport,
            &clock,
        );

        let sent = transport.sent.borrow();
        assert_eq!(sent.len(), 1);
        let request = &sent[0];
        assert_eq!(request.method, crate::meter::transport::HttpMethod::Get);
        assert_eq!(request.url, OllamaAdapter::DEFAULT_ENDPOINT);
        assert!(
            request.headers.contains(&(
                "Authorization".to_string(),
                "ollama-test-key-12345".to_string()
            )),
            "expected a bare Authorization header, got {:?}",
            request.headers
        );
        assert!(
            !request
                .headers
                .iter()
                .any(|(_, value)| value.starts_with("Bearer ")),
            "the Authorization header must carry no Bearer scheme, got {:?}",
            request.headers
        );
    }

    #[test]
    fn declarations_carry_the_ollama_contract_and_semantics() {
        let adapter = test_adapter();
        let declarations = adapter.declarations();
        assert_eq!(
            declarations.provider_contract_id.as_str(),
            OllamaAdapter::CONTRACT_ID
        );
        assert_eq!(
            declarations.meter_semantics_id.as_str(),
            OllamaAdapter::SEMANTICS_ID
        );
        assert!(declarations.required_window_kinds.contains("session"));
        assert!(declarations.required_window_kinds.contains("weekly"));
    }

    #[test]
    fn rate_limited_status_carries_the_retry_after_header() {
        let adapter = test_adapter();
        let transport = MockTransport {
            response: Ok(HttpResponse {
                status: 429,
                headers: vec![("retry-after".to_string(), "30".to_string())],
                body: Vec::new(),
            }),
        };
        let clock = test_clock();
        let obs = adapter.observe(
            &test_credential(),
            &MeterRequest::default(),
            &transport,
            &clock,
        );

        assert_eq!(
            obs,
            ProviderObservation::Unreachable(FailureClass::RateLimited {
                retry_after: Some(MonotonicDuration::from_seconds(30)),
            })
        );
    }

    #[test]
    fn server_error_status_classifies_as_http_status_server_error() {
        let adapter = test_adapter();
        let transport = MockTransport::ok(503, Vec::new());
        let clock = test_clock();
        let obs = adapter.observe(
            &test_credential(),
            &MeterRequest::default(),
            &transport,
            &clock,
        );

        assert_eq!(
            obs,
            ProviderObservation::Unreachable(FailureClass::HttpStatus(
                HttpStatusClass::ServerError
            ))
        );
    }

    /// Grid arithmetic, with the instants stated literally
    /// (`aub-ud17`'s acceptance case): just before a boundary, on it, and
    /// just after, for the session grid; and the weekly grid at the first
    /// instant.
    #[test]
    fn grid_arithmetic_matches_the_literal_acceptance_instants() {
        let session = |received_secs: i64| {
            scheduled_reset(
                UtcTimestamp::from_unix_nanos(received_secs * 1_000_000_000),
                OllamaAdapter::SESSION_WINDOW_SECS,
                0,
            )
        };
        let weekly = |received_secs: i64| {
            scheduled_reset(
                UtcTimestamp::from_unix_nanos(received_secs * 1_000_000_000),
                OllamaAdapter::WEEKLY_WINDOW_SECS,
                OllamaAdapter::WEEKLY_OFFSET_SECS,
            )
        };

        // 2026-09-06T21:00:00Z: session resets 2026-09-06T22:00:00Z, weekly
        // resets 2026-09-07T00:00:00Z.
        assert_eq!(
            session(1_788_728_400),
            WindowResetState::Scheduled {
                at: UtcTimestamp::from_unix_nanos(1_788_732_000_000_000_000),
                grid: ResetGridId::OllamaCloudV1,
            }
        );
        assert_eq!(
            weekly(1_788_728_400),
            WindowResetState::Scheduled {
                at: UtcTimestamp::from_unix_nanos(1_788_739_200_000_000_000),
                grid: ResetGridId::OllamaCloudV1,
            }
        );

        // One second before the session boundary: still resets at the same
        // boundary.
        assert_eq!(
            session(1_788_731_999),
            WindowResetState::Scheduled {
                at: UtcTimestamp::from_unix_nanos(1_788_732_000_000_000_000),
                grid: ResetGridId::OllamaCloudV1,
            }
        );

        // Exactly on the boundary: belongs to the next window, not the one
        // that just closed.
        assert_eq!(
            session(1_788_732_000),
            WindowResetState::Scheduled {
                at: UtcTimestamp::from_unix_nanos(1_788_750_000_000_000_000),
                grid: ResetGridId::OllamaCloudV1,
            }
        );
    }

    /// The planted negative for the on-boundary case: a naive "boundary
    /// belongs to the window it closes" implementation would return the
    /// instant itself rather than the next one.
    #[test]
    fn next_grid_boundary_on_the_boundary_returns_the_next_one_not_itself() {
        assert_eq!(
            next_grid_boundary_secs(1_788_732_000, OllamaAdapter::SESSION_WINDOW_SECS, 0),
            1_788_750_000
        );
        assert_ne!(
            next_grid_boundary_secs(1_788_732_000, OllamaAdapter::SESSION_WINDOW_SECS, 0),
            1_788_732_000
        );
    }

    #[test]
    fn usage_fraction_out_of_range_is_rejected() {
        assert_eq!(usage_fraction_to_ppm(-0.01), None);
        assert_eq!(usage_fraction_to_ppm(1.01), None);
        assert_eq!(usage_fraction_to_ppm(f64::NAN), None);
    }

    #[test]
    fn usage_fraction_boundaries_round_trip() {
        assert_eq!(usage_fraction_to_ppm(0.0).unwrap().get(), 0);
        assert_eq!(usage_fraction_to_ppm(1.0).unwrap().get(), 1_000_000);
        assert_eq!(usage_fraction_to_ppm(0.163).unwrap().get(), 163_000);
    }
}
