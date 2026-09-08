//! The Antigravity (Google `agy`) quota-summary provider meter adapter
//! (`aub-n8yx`).
//!
//! Implements [`ProviderAdapter`] for
//! `POST https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary`
//! with an empty JSON body and three headers: `Authorization: Bearer <token>`,
//! `Content-Type: application/json`, and `User-Agent: antigravity-cli`
//! (`bin/quota-bars:300-316` in llm-workflow is the working reference this
//! adapter matches).
//!
//! # Why the `User-Agent` is a contract, not cosmetics
//!
//! Without `User-Agent: antigravity-cli` the endpoint answers 403 "You do
//! not have a valid license of this product" (`bin/quota-bars:308-310`),
//! which names no header and reads as an account problem. The header is
//! therefore part of the request the adapter asserts on, and a 403 maps to
//! [`FailureClass::HttpStatus`], not to an authentication conclusion: an
//! ambiguous 403 is never classified as `AuthRequired` (section 34.8), and
//! the sticky auth conclusion is reserved for the 401 the endpoint answers
//! with an expired credential.
//!
//! # Scopes: a group, not a model
//!
//! The response carries `.groups[]`, each with a `displayName` and
//! `.buckets[]` per window. Every bucket becomes one [`MeterWindow`] scoped
//! to its group (`WindowScope::ModelGroup`): the two groups each share one
//! weekly and one 5-hour budget across the models inside them. The response
//! reports `resetTime` for every bucket, so every reset state is `Known`;
//! the grid-recomputation story of `aub-ud17` does not apply here.
//!
//! # Resolution
//!
//! `remainingFraction` is a float in `[0, 1]`; used =
//! `round((1 - remainingFraction) * 1_000_000)` ppm, and the reported
//! resolution is treated as 1 ppm (the adapter-semantics row records that
//! the surface shows whole percent).
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
    GroupName, MeterWindow, NominalWindowDuration, QuantizationSemantics, ReportedResolution,
    WindowResetState, WindowScope, WindowSemanticKey,
};
use crate::meter::adapter::{
    AdapterDeclarations, CredentialHandle, HttpTransport, MeterRequest, ProviderAdapter,
    ProviderObservation, RequiredWindowKinds,
};
use crate::meter::evidence::{
    CapturedProviderResponse, SensitiveResponseMaterial, capture_json_body, capture_json_response,
    error_report_for_observation, quota_response_from_capsule,
};
use crate::meter::transport::{CommandBudget, HttpRequest, RequestTimeoutConfig};

/// The typed observation reading produced by [`AgyAdapter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgyReading {
    pub windows: Vec<MeterWindow>,
}

/// The Antigravity quota-summary provider adapter.
pub struct AgyAdapter {
    endpoint_url: String,
    declarations: AdapterDeclarations,
}

impl Default for AgyAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AgyAdapter {
    pub const DEFAULT_ENDPOINT: &'static str =
        "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary";
    pub const CONTRACT_ID: &'static str = "google-antigravity-quota-summary-v1";
    pub const SEMANTICS_ID: &'static str = "google-antigravity-subscription-v1";
    /// The window identifiers the real response carries (`aub-n8yx`): one
    /// 5-hour bucket and one weekly bucket per group.
    pub const REQUIRED_WINDOW_KINDS: &'static [&'static str] = &["5h", "weekly"];
    /// The header the endpoint demands: without it the service answers 403.
    pub const USER_AGENT: &'static str = "antigravity-cli";
    /// The 5-hour window's nominal length, in seconds.
    pub const FIVE_HOUR_WINDOW_SECS: i64 = 18_000;
    /// The weekly window's nominal length, in seconds.
    pub const WEEKLY_WINDOW_SECS: i64 = 604_800;

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

/// The nominal window length a known `window` literal names.
fn window_secs(window: &str) -> Option<i64> {
    match window {
        "5h" => Some(AgyAdapter::FIVE_HOUR_WINDOW_SECS),
        "weekly" => Some(AgyAdapter::WEEKLY_WINDOW_SECS),
        _ => None,
    }
}

/// Converts a `remainingFraction` in `[0, 1]` into a used fraction in parts
/// per million. See the module documentation for why this is the reported
/// resolution of 1 ppm and not an integer percent.
fn remaining_fraction_to_used_ppm(fraction: f64) -> Option<QuotaFractionPpm> {
    if !fraction.is_finite() || !(0.0..=1.0).contains(&fraction) {
        return None;
    }
    QuotaFractionPpm::new(((1.0 - fraction) * 1_000_000.0).round() as i32)
}

/// Parses one bucket object into a typed, group-scoped window. Every bucket
/// becomes a row: a `disabled` bucket still carries its fraction and is
/// marked inactive, because "the 5-hour limit does not currently apply" is a
/// provider fact about this window, not a reason to drop the evidence.
fn parse_bucket(
    object: &serde_json::Map<String, serde_json::Value>,
    group: &GroupName,
) -> Result<MeterWindow, FailureClass> {
    let window = object
        .get("window")
        .and_then(serde_json::Value::as_str)
        .ok_or(FailureClass::MissingRequiredField)?;
    let window_secs = window_secs(window).ok_or(FailureClass::SchemaDrift)?;
    let remaining = object
        .get("remainingFraction")
        .and_then(serde_json::Value::as_f64)
        .ok_or(FailureClass::MissingRequiredField)?;
    let quota_used =
        remaining_fraction_to_used_ppm(remaining).ok_or(FailureClass::MissingRequiredField)?;
    let reset_time = object
        .get("resetTime")
        .and_then(serde_json::Value::as_str)
        .ok_or(FailureClass::MissingRequiredField)?;
    let resets_at = UtcTimestamp::parse_rfc3339(reset_time)
        .map(WindowResetState::Known)
        .ok_or(FailureClass::MissingRequiredField)?;
    let is_active = !object
        .get("disabled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    Ok(MeterWindow::new_with_facts(
        WindowSemanticKey::new(window),
        WindowScope::ModelGroup(group.clone()),
        QuotaUsed::new(quota_used),
        ReportedResolution::new(QuotaFractionPpm::new(1).expect("1 is in the fraction range"))
            .expect("1 is a non-zero resolution"),
        QuantizationSemantics::Unknown,
        resets_at,
        NominalWindowDuration::from_nanos(window_secs as u64 * 1_000_000_000),
        is_active,
        crate::domain::window::WindowSeverity::unknown(),
    ))
}

/// Parses the JSON response body from the quota-summary endpoint.
pub fn parse_agy_quota_body(
    body: &[u8],
    received_at: UtcTimestamp,
) -> Result<AgyReading, FailureClass> {
    let capsule = capture_json_body(body, &SensitiveResponseMaterial::default());
    replay_agy_capsule(capsule.serialized(), received_at)
}

/// Reinterprets retained response evidence with the current Antigravity
/// semantics, the same replay seam [`crate::meter::anthropic`] uses.
pub fn replay_agy_capsule(
    capsule: &str,
    _received_at: UtcTimestamp,
) -> Result<AgyReading, FailureClass> {
    let val = quota_response_from_capsule(capsule).map_err(|message| {
        if message == "capsule does not contain a quota response" {
            FailureClass::MalformedBody
        } else {
            FailureClass::MissingRequiredField
        }
    })?;
    let root = val.as_object().ok_or(FailureClass::MalformedBody)?;
    let groups = root
        .get("groups")
        .and_then(serde_json::Value::as_array)
        .ok_or(FailureClass::MissingRequiredField)?;
    if groups.is_empty() {
        return Err(FailureClass::MissingRequiredField);
    }

    let mut windows = Vec::new();
    let mut seen_windows: Vec<&str> = Vec::new();
    for group in groups {
        let object = group.as_object().ok_or(FailureClass::MalformedBody)?;
        let display_name = object
            .get("displayName")
            .and_then(serde_json::Value::as_str)
            .ok_or(FailureClass::MissingRequiredField)?;
        let group_name = GroupName::new(display_name);
        let buckets = object
            .get("buckets")
            .and_then(serde_json::Value::as_array)
            .ok_or(FailureClass::MissingRequiredField)?;
        for bucket in buckets {
            let object = bucket.as_object().ok_or(FailureClass::MalformedBody)?;
            windows.push(parse_bucket(object, &group_name)?);
            let window = object
                .get("window")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if !seen_windows.contains(&window) {
                seen_windows.push(window);
            }
        }
    }

    // Every window kind the contract names must have appeared somewhere in
    // the response, the same global check the Anthropic adapter makes: a
    // response missing a whole window kind is a contract change, refused
    // rather than silently measured short.
    if AgyAdapter::REQUIRED_WINDOW_KINDS
        .iter()
        .any(|kind| !seen_windows.contains(kind))
    {
        return Err(FailureClass::MissingRequiredField);
    }

    Ok(AgyReading { windows })
}

/// Extracts the access token from the resolved credential material: the
/// Antigravity token file's JSON, field `.token.access_token`. A raw token
/// is not accepted here because the credential for this provider is always
/// that file (`aub-n8yx`); anything unparseable or missing the field is the
/// credential's own defect and reads as an expired credential.
fn extract_access_token(credential: &CredentialHandle) -> Result<String, AuthReason> {
    let material = credential.expose().trim();
    let val: serde_json::Value =
        serde_json::from_str(material).map_err(|_| AuthReason::CredentialExpired)?;
    val.get("token")
        .and_then(|token| token.get("access_token"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .filter(|token| !token.is_empty())
        .ok_or(AuthReason::CredentialExpired)
}

fn parse_401_auth_reason(_body: &[u8]) -> AuthReason {
    // The 401 body carries Google's generic UNAUTHENTICATED message with no
    // machine-readable distinction between an invalid and an expired token,
    // so every 401 is the same sticky conclusion: the credential was
    // rejected. The CLI renews the token on its own use; no refresh here.
    AuthReason::CredentialRejected
}

impl ProviderAdapter for AgyAdapter {
    type Reading = AgyReading;

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
        let access_token = match extract_access_token(credential) {
            Ok(token) => token,
            Err(reason) => {
                return CapturedProviderResponse::without_response(
                    ProviderObservation::AuthRequired(reason),
                );
            }
        };

        let timeouts = RequestTimeoutConfig::new(
            MonotonicDuration::from_seconds(5),
            MonotonicDuration::from_seconds(10),
            Some(MonotonicDuration::from_seconds(15)),
        );

        // The body is the empty JSON object, exactly the bytes quota-bars
        // sends; `Content-Type` names what they are.
        let req = HttpRequest::post(&self.endpoint_url, b"{}".to_vec(), timeouts)
            .with_header("Authorization", format!("Bearer {access_token}"))
            .with_header("Content-Type", "application/json")
            .with_header("User-Agent", AgyAdapter::USER_AGENT);

        let budget = CommandBudget::new(MonotonicDuration::from_seconds(30), clock);

        let response = match transport.send(&req, &budget, clock) {
            Ok(res) => res,
            Err(failure) => {
                return CapturedProviderResponse::without_response(
                    ProviderObservation::Unreachable(failure),
                );
            }
        };

        // The access token is the only secret this request holds, and the
        // endpoint does not echo it back; the sanitizer is still given the
        // material so an echoed token can never survive into evidence.
        let sensitive = SensitiveResponseMaterial::new([access_token.as_str()]);
        let evidence = capture_json_response(&response, &sensitive);
        let received_at = clock.now();
        let observation = match response.status() {
            200 => match replay_agy_capsule(evidence.serialized(), received_at) {
                Ok(reading) => ProviderObservation::Measured(reading),
                Err(failure) => ProviderObservation::Unreachable(failure),
            },
            401 => ProviderObservation::AuthRequired(parse_401_auth_reason(response.body())),
            429 => {
                let retry_after = response
                    .header("retry-after")
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .map(MonotonicDuration::from_seconds);
                ProviderObservation::Unreachable(FailureClass::RateLimited { retry_after })
            }
            // The 403 the endpoint answers with a missing user agent is a
            // client error, never an auth conclusion (section 34.8): an
            // ambiguous 403 arriving as AuthRequired would make a sticky
            // auth conclusion out of a header bug.
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

        let failed_error = error_report_for_observation(
            &observation,
            response.body(),
            response.status(),
            &sensitive,
        );
        CapturedProviderResponse {
            observation,
            evidence: Some(evidence),
            failed_body,
            failed_error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::FakeClock;
    use crate::meter::transport::{HttpMethod, HttpResponse};
    use test_support::sanitization::matched_patterns;

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
    /// assert on the outgoing method, URL, headers and body rather than only
    /// on the parsed result.
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

    const FIXTURE_VALID: &[u8] = include_bytes!("../../tests/fixtures/meter/agy/valid.json");
    const FIXTURE_ERROR_403: &[u8] =
        include_bytes!("../../tests/fixtures/meter/agy/error-403-no-user-agent.json");
    const FIXTURE_ERROR_401: &[u8] =
        include_bytes!("../../tests/fixtures/meter/agy/error-401.json");
    const FIXTURE_MALFORMED: &[u8] =
        include_bytes!("../../tests/fixtures/meter/agy/malformed.json");
    const FIXTURE_EMPTY_GROUPS: &[u8] =
        include_bytes!("../../tests/fixtures/meter/agy/empty-groups.json");

    fn test_adapter() -> AgyAdapter {
        AgyAdapter::new()
    }

    /// The resolved credential the adapter receives: the token file's JSON
    /// material, the same shape `~/.gemini/antigravity-cli/antigravity-oauth-token`
    /// carries.
    fn test_credential() -> CredentialHandle {
        CredentialHandle::new(r#"{"token":{"access_token":"agy-test-token-12345"}}"#)
    }

    fn test_clock() -> FakeClock {
        FakeClock::new(UtcTimestamp::from_unix_nanos(1_788_728_400_000_000_000))
    }

    fn expect_measured(obs: ProviderObservation<AgyReading>) -> AgyReading {
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

    /// The sanitized real response (`aub-n8yx`): two groups, each with one
    /// weekly and one 5-hour bucket, parsed into one group-scoped window
    /// per bucket with `Known` resets from the reported `resetTime`.
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
        assert_eq!(reading.windows.len(), 4, "two groups, two buckets each");

        let gemini_weekly = &reading.windows[0];
        assert_eq!(gemini_weekly.semantic_key().as_str(), "weekly");
        assert_eq!(
            *gemini_weekly.scope(),
            WindowScope::ModelGroup(GroupName::new("Gemini Models"))
        );
        // 1 - 0.06455878 = 0.93544122 -> 935441 ppm used.
        assert_eq!(gemini_weekly.quota_used().as_ppm().get(), 935_441);
        assert_eq!(gemini_weekly.reported_resolution().as_ppm().get(), 1);
        assert_eq!(
            gemini_weekly.nominal_duration().as_nanos(),
            604_800 * 1_000_000_000
        );
        assert_eq!(
            gemini_weekly.reset_state(),
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(1_789_096_201_000_000_000))
        );
        assert!(gemini_weekly.is_active());

        let gemini_5h = &reading.windows[1];
        assert_eq!(gemini_5h.semantic_key().as_str(), "5h");
        assert_eq!(
            *gemini_5h.scope(),
            WindowScope::ModelGroup(GroupName::new("Gemini Models"))
        );
        assert_eq!(gemini_5h.quota_used().as_ppm().get(), 85_528);
        assert_eq!(
            gemini_5h.nominal_duration().as_nanos(),
            18_000 * 1_000_000_000
        );

        let thirdp_weekly = &reading.windows[2];
        assert_eq!(thirdp_weekly.semantic_key().as_str(), "weekly");
        assert_eq!(
            *thirdp_weekly.scope(),
            WindowScope::ModelGroup(GroupName::new("Claude and GPT models"))
        );
        assert_eq!(thirdp_weekly.quota_used().as_ppm().get(), 1_000_000);

        // The disabled bucket keeps its evidence and its fraction, and
        // carries the provider's own inactive fact rather than being dropped.
        let thirdp_5h = &reading.windows[3];
        assert_eq!(thirdp_5h.semantic_key().as_str(), "5h");
        assert_eq!(thirdp_5h.quota_used().as_ppm().get(), 0);
        assert!(!thirdp_5h.is_active());
        assert_eq!(
            thirdp_5h.reset_state(),
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(1_788_767_637_000_000_000))
        );
    }

    /// The 403 the endpoint answers when the user agent is missing: a
    /// client-error class, never the sticky auth conclusion (section 34.8).
    #[test]
    fn case_02_error_403_no_user_agent_is_a_client_error_not_auth() {
        let adapter = test_adapter();
        let transport = MockTransport::ok(403, FIXTURE_ERROR_403);
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
                HttpStatusClass::ClientError
            ))
        );
    }

    /// An expired or rejected credential: the sticky auth conclusion.
    #[test]
    fn case_03_error_401_is_auth_required() {
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

    /// Unparseable bytes: a malformed body, not a missing field.
    #[test]
    fn case_04_malformed() {
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

    /// An empty groups array: the response parses but measures nothing, so
    /// it is refused rather than persisted as a measured nothing (and the
    /// required window kinds never appeared either way).
    #[test]
    fn case_05_empty_groups() {
        let adapter = test_adapter();
        let transport = MockTransport::ok(200, FIXTURE_EMPTY_GROUPS);
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

    /// The request contract: one POST to the quota-summary endpoint with the
    /// three headers and the empty JSON body, asserted against the recorded
    /// outgoing request rather than only against the parsed result.
    #[test]
    fn request_is_post_with_the_three_headers_and_an_empty_json_body() {
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
        assert_eq!(request.method, HttpMethod::Post);
        assert_eq!(request.url, AgyAdapter::DEFAULT_ENDPOINT);
        assert_eq!(request.body.as_deref(), Some(b"{}".as_slice()));
        assert!(
            request.headers.contains(&(
                "Authorization".to_string(),
                "Bearer agy-test-token-12345".to_string()
            )),
            "expected the bearer Authorization header, got {:?}",
            request.headers
        );
        assert!(
            request
                .headers
                .contains(&("Content-Type".to_string(), "application/json".to_string())),
            "expected the JSON content type, got {:?}",
            request.headers
        );
        assert!(
            request
                .headers
                .contains(&("User-Agent".to_string(), "antigravity-cli".to_string())),
            "expected the antigravity-cli user agent, got {:?}",
            request.headers
        );
    }

    #[test]
    fn declarations_carry_the_antigravity_contract_and_semantics() {
        let adapter = test_adapter();
        let declarations = adapter.declarations();
        assert_eq!(
            declarations.provider_contract_id.as_str(),
            AgyAdapter::CONTRACT_ID
        );
        assert_eq!(
            declarations.meter_semantics_id.as_str(),
            AgyAdapter::SEMANTICS_ID
        );
        assert!(declarations.required_window_kinds.contains("5h"));
        assert!(declarations.required_window_kinds.contains("weekly"));
    }

    /// A response missing one whole window kind is a contract change: the
    /// same global required-kinds check the Anthropic adapter makes.
    #[test]
    fn a_missing_window_kind_is_refused_not_measured_short() {
        let body = br#"{"groups":[{"displayName":"Gemini Models","buckets":[
            {"window":"5h","remainingFraction":0.5,"resetTime":"2026-09-07T07:45:16Z"}
        ]}]}"#;
        let obs = test_adapter().observe(
            &test_credential(),
            &MeterRequest::default(),
            &MockTransport::ok(200, body),
            &test_clock(),
        );
        assert_eq!(
            obs,
            ProviderObservation::Unreachable(FailureClass::MissingRequiredField)
        );
    }

    /// The planted negative for the fraction arithmetic: a naive
    /// "remaining is used" reading turns the numbers upside down, and a
    /// percent-scaled reading misses by a factor of ten thousand.
    #[test]
    fn used_fraction_is_the_complement_of_the_remaining_fraction() {
        assert_eq!(
            remaining_fraction_to_used_ppm(0.9144718).unwrap().get(),
            85_528
        );
        assert_eq!(
            remaining_fraction_to_used_ppm(0.0).unwrap().get(),
            1_000_000
        );
        assert_eq!(remaining_fraction_to_used_ppm(1.0).unwrap().get(), 0);
        // 1 - 0.5 must not read as 500_000 * something-else: the only value
        // it can honestly be is exactly half.
        assert_eq!(remaining_fraction_to_used_ppm(0.5).unwrap().get(), 500_000);
        assert_eq!(remaining_fraction_to_used_ppm(-0.01), None);
        assert_eq!(remaining_fraction_to_used_ppm(1.01), None);
        assert_eq!(remaining_fraction_to_used_ppm(f64::NAN), None);
    }

    /// An unknown window literal is schema drift, refused rather than
    /// silently turned into a row with no honest nominal length.
    #[test]
    fn an_unknown_window_literal_is_schema_drift() {
        let body = br#"{"groups":[{"displayName":"Gemini Models","buckets":[
            {"window":"1m","remainingFraction":0.5,"resetTime":"2026-09-07T07:45:16Z"},
            {"window":"5h","remainingFraction":0.5,"resetTime":"2026-09-07T07:45:16Z"},
            {"window":"weekly","remainingFraction":0.5,"resetTime":"2026-09-10T04:47:34Z"}
        ]}]}"#;
        let obs = test_adapter().observe(
            &test_credential(),
            &MeterRequest::default(),
            &MockTransport::ok(200, body),
            &test_clock(),
        );
        assert_eq!(
            obs,
            ProviderObservation::Unreachable(FailureClass::SchemaDrift)
        );
    }

    /// The credential material: the token file's JSON, and nothing else. A
    /// raw token, an unparseable file and a missing field are all the same
    /// conclusion, the credential's own defect.
    #[test]
    fn credential_material_is_the_token_files_json() {
        let json = CredentialHandle::new(r#"{"token":{"access_token":"tok-1"}}"#);
        assert_eq!(extract_access_token(&json).unwrap(), "tok-1");
        let raw = CredentialHandle::new("raw-token-not-json");
        assert_eq!(
            extract_access_token(&raw).unwrap_err(),
            AuthReason::CredentialExpired
        );
        let missing = CredentialHandle::new(r#"{"token":{}}"#);
        assert_eq!(
            extract_access_token(&missing).unwrap_err(),
            AuthReason::CredentialExpired
        );
        let empty = CredentialHandle::new(r#"{"token":{"access_token":""}}"#);
        assert_eq!(
            extract_access_token(&empty).unwrap_err(),
            AuthReason::CredentialExpired
        );
        let empty_material = CredentialHandle::new("");
        assert_eq!(
            extract_access_token(&empty_material).unwrap_err(),
            AuthReason::CredentialExpired
        );
    }

    /// The committed fixture carries no credential-shaped material, and a
    /// capsule built from it carries none either: sanitization is proven,
    /// not assumed (`aub-n8yx`).
    #[test]
    fn the_committed_fixture_carries_no_credential_material() {
        let text = std::str::from_utf8(FIXTURE_VALID).unwrap();
        for forbidden in ["ya29.", "Bearer", "access_token", "refresh_token", "eyJ"] {
            assert!(!text.contains(forbidden), "fixture carries {forbidden}");
        }
        // The serialized capsule of that fixture, built with a token the
        // sanitizer is told about, never carries the token back.
        let capsule = capture_json_body(
            FIXTURE_VALID,
            &SensitiveResponseMaterial::new(["agy-test-token-12345"]),
        );
        assert!(!capsule.serialized().contains("agy-test-token-12345"));
        assert!(capsule.serialized().contains("remainingFraction"));
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

    /// Case 06 (aub-rfot): a 429 whose body carries no `error.type` (the
    /// Google shape reports a status code and a message instead) stores the
    /// status spelling as the classification and the provider's own message,
    /// sanitized, beside it. The message is the useful half here: it is the
    /// provider's own words about the refusal.
    #[test]
    fn case_06_error_429_without_a_type_stores_the_status_spelling_and_the_message() {
        let adapter = test_adapter();
        let transport = MockTransport::ok(
            429,
            br#"{"error":{"code":429,"message":"Quota exceeded for the group.","status":"RESOURCE_EXHAUSTED"}}"#.as_slice(),
        );
        let captured = adapter.observe_with_evidence(
            &test_credential(),
            &MeterRequest::default(),
            &transport,
            &test_clock(),
        );
        let report = captured
            .failed_error
            .as_ref()
            .expect("a 429 response stores the provider's error report");
        assert_eq!(report.classification, "http_429");
        assert_eq!(report.message, "Quota exceeded for the group.");
        assert!(matched_patterns(&report.classification).is_empty());
        assert!(matched_patterns(&report.message).is_empty());
    }

    /// Case 07 (aub-rfot): the 401 fixture's own words are stored beside the
    /// status spelling, and neither field matches a forbidden pattern.
    #[test]
    fn case_07_error_401_stores_the_status_spelling_and_the_provider_s_message() {
        let adapter = test_adapter();
        let transport = MockTransport::ok(401, FIXTURE_ERROR_401);
        let captured = adapter.observe_with_evidence(
            &test_credential(),
            &MeterRequest::default(),
            &transport,
            &test_clock(),
        );
        let report = captured
            .failed_error
            .as_ref()
            .expect("a 401 response stores the provider's error report");
        assert_eq!(report.classification, "http_401");
        assert_eq!(
            report.message,
            "Request had invalid authentication credentials. Expected OAuth 2 access token, login cookie or other valid authentication credential. See https://developers.google.com/identity/sign-in/web/devconsole-project."
        );
        assert!(matched_patterns(&report.classification).is_empty());
        assert!(matched_patterns(&report.message).is_empty());
    }
}
