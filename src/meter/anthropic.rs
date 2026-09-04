//! The Anthropic provider meter adapter (`aub-eun.4`).
//!
//! Implements [`ProviderAdapter`] for the Anthropic subscription OAuth usage endpoint
//! (`GET https://api.anthropic.com/api/oauth/usage`), authenticating via a bearer token
//! with the `anthropic-beta: oauth-2025-04-20` header.
//!
//! # Boundary rules
//!
//! May not depend on:
//! - SQLite directly (rule `03`)
//! - credential or configuration modules (rule `07`)
//! - the ureq transport driver (rule `12`)
//! - write-capable filesystem facilities (rule `17`)
//! - presentation or calibration modules
//!
//! # 403 Status Classification Rationale
//!
//! For Anthropic, a 403 Forbidden status indicates workspace/organization permissions,
//! IP restrictions, or enterprise policy denial, rather than token expiration or credential
//! revocation (which Anthropic signals via 401 Unauthorized with a structured error body).
//! Classifying a 403 as authentication failure would incorrectly mark valid credentials
//! as expired. Thus, a 403 status is classified as [`FailureClass::HttpStatus(HttpStatusClass::ClientError)`].

use crate::domain::failure::{AuthReason, FailureClass, HttpStatusClass};
use crate::domain::ids::{MeterSemanticsId, ProviderContractId};
use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
use crate::domain::time::{
    Clock, MeasurementBasis, MonotonicDuration, ProviderObservedAt, UtcTimestamp,
};
use crate::domain::window::{
    MeterWindow, ModelId, NominalWindowDuration, QuantizationSemantics, ReportedResolution,
    WindowResetState, WindowScope, WindowSemanticKey,
};
use crate::meter::adapter::{
    AdapterDeclarations, CredentialHandle, HttpTransport, MeterRequest, ProviderAdapter,
    ProviderObservation,
};
use crate::meter::evidence::{
    CapturedProviderResponse, SensitiveResponseMaterial, capture_json_body, capture_json_response,
    quota_response_from_capsule,
};
use crate::meter::transport::{CommandBudget, HttpRequest, HttpResponse, RequestTimeoutConfig};

/// Extra usage configuration from an Anthropic response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicExtraUsage {
    pub is_enabled: bool,
    pub monthly_limit: Option<u64>,
    pub used_credits: Option<u64>,
    pub utilization: Option<QuotaFractionPpm>,
}

/// A window dropped from an observation because of an invalid or missing field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedWindow {
    pub semantic_key: WindowSemanticKey,
    pub reason: FailureClass,
    pub field: String,
    pub payload_fragment: String,
}

/// The typed observation reading produced by [`AnthropicAdapter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicReading {
    pub windows: Vec<MeterWindow>,
    pub provider_observed_at: Option<ProviderObservedAt>,
    pub extra_usage: Option<AnthropicExtraUsage>,
    pub dropped_windows: Vec<DroppedWindow>,
}

impl AnthropicReading {
    pub fn new(windows: Vec<MeterWindow>) -> Self {
        Self {
            windows,
            provider_observed_at: None,
            extra_usage: None,
            dropped_windows: Vec::new(),
        }
    }
}

/// The Anthropic OAuth provider adapter.
pub struct AnthropicAdapter {
    endpoint_url: String,
    declarations: AdapterDeclarations,
}

impl Default for AnthropicAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AnthropicAdapter {
    pub const DEFAULT_ENDPOINT: &'static str = "https://api.anthropic.com/api/oauth/usage";
    pub const ANTHROPIC_BETA_HEADER: &'static str = "oauth-2025-04-20";
    pub const DEFAULT_CONTRACT_ID: &'static str = "anthropic-oauth-usage-v1";
    pub const DEFAULT_SEMANTICS_ID: &'static str = "anthropic-subscription-v1";

    pub fn new() -> Self {
        Self::with_endpoint(Self::DEFAULT_ENDPOINT)
    }

    pub fn with_endpoint(endpoint_url: impl Into<String>) -> Self {
        Self {
            endpoint_url: endpoint_url.into(),
            declarations: AdapterDeclarations::new(
                MeasurementBasis::LocallyReceived,
                ProviderContractId::new(Self::DEFAULT_CONTRACT_ID),
                MeterSemanticsId::new(Self::DEFAULT_SEMANTICS_ID),
            ),
        }
    }

    pub fn with_declarations(
        endpoint_url: impl Into<String>,
        declarations: AdapterDeclarations,
    ) -> Self {
        Self {
            endpoint_url: endpoint_url.into(),
            declarations,
        }
    }

    pub fn endpoint_url(&self) -> &str {
        &self.endpoint_url
    }
}

/// Extracts the bearer token from a credential handle.
/// Handles raw token strings, JSON `{ "claudeAiOauth": { "accessToken": "..." } }`,
/// and JSON `{ "accessToken": "..." }`.
fn extract_bearer_token(credential: &CredentialHandle) -> Result<String, AuthReason> {
    let raw = credential.expose().trim();
    if raw.is_empty() {
        return Err(AuthReason::CredentialExpired);
    }

    if raw.starts_with('{')
        && raw.ends_with('}')
        && let Ok(val) = serde_json::from_str::<serde_json::Value>(raw)
    {
        if let Some(token) = val
            .get("claudeAiOauth")
            .and_then(|o| o.get("accessToken"))
            .and_then(|t| t.as_str())
        {
            let trimmed = token.trim();
            if trimmed.is_empty() {
                return Err(AuthReason::CredentialExpired);
            }
            return Ok(trimmed.to_string());
        }

        if let Some(token) = val.get("accessToken").and_then(|t| t.as_str()) {
            let trimmed = token.trim();
            if trimmed.is_empty() {
                return Err(AuthReason::CredentialExpired);
            }
            return Ok(trimmed.to_string());
        }

        if let Some(token) = val.get("access_token").and_then(|t| t.as_str()) {
            let trimmed = token.trim();
            if trimmed.is_empty() {
                return Err(AuthReason::CredentialExpired);
            }
            return Ok(trimmed.to_string());
        }

        return Err(AuthReason::CredentialExpired);
    }

    Ok(raw.to_string())
}

/// Parses the JSON response body from Anthropic `/api/oauth/usage`.
pub fn parse_anthropic_usage_body(
    body: &[u8],
    request: &MeterRequest,
) -> Result<AnthropicReading, FailureClass> {
    let capsule = capture_json_body(body, &SensitiveResponseMaterial::default());
    replay_anthropic_capsule(capsule.serialized(), request)
}

/// Reinterprets retained response evidence with the current Anthropic
/// semantics. The stored capsule remains unchanged, so callers can persist a
/// corrected observation beside the original interpretation.
pub fn replay_anthropic_capsule(
    capsule: &str,
    _request: &MeterRequest,
) -> Result<AnthropicReading, FailureClass> {
    let val = quota_response_from_capsule(capsule).map_err(|message| {
        if message == "capsule does not contain a quota response" {
            FailureClass::MalformedBody
        } else {
            FailureClass::MissingRequiredField
        }
    })?;

    let root = val.as_object().ok_or(FailureClass::MalformedBody)?;

    let five_hour_obj = root
        .get("five_hour")
        .and_then(|v| v.as_object())
        .ok_or(FailureClass::MissingRequiredField)?;

    let seven_day_obj = root
        .get("seven_day")
        .and_then(|v| v.as_object())
        .ok_or(FailureClass::MissingRequiredField)?;

    let mut windows = Vec::new();
    let mut dropped_windows = Vec::new();

    match parse_window(
        "five_hour",
        WindowScope::AccountWide,
        five_hour_obj,
        NominalWindowDuration::from_nanos(5 * 3600 * 1_000_000_000),
    ) {
        Ok(window) => windows.push(window),
        Err(err) => dropped_windows.push(DroppedWindow {
            semantic_key: WindowSemanticKey::new("five_hour"),
            reason: err.failure_class,
            field: err.field.to_string(),
            payload_fragment: err.payload_fragment,
        }),
    }

    match parse_window(
        "seven_day",
        WindowScope::AccountWide,
        seven_day_obj,
        NominalWindowDuration::from_nanos(7 * 24 * 3600 * 1_000_000_000),
    ) {
        Ok(window) => windows.push(window),
        Err(err) => dropped_windows.push(DroppedWindow {
            semantic_key: WindowSemanticKey::new("seven_day"),
            reason: err.failure_class,
            field: err.field.to_string(),
            payload_fragment: err.payload_fragment,
        }),
    }

    for (key, v) in root {
        if key.starts_with("seven_day_")
            && let Some(obj) = v.as_object()
        {
            let model_name = key.trim_start_matches("seven_day_");
            if !model_name.is_empty() {
                match parse_window(
                    key,
                    WindowScope::ModelSpecific(ModelId::new(model_name)),
                    obj,
                    NominalWindowDuration::from_nanos(7 * 24 * 3600 * 1_000_000_000),
                ) {
                    Ok(window) => windows.push(window),
                    Err(err) => dropped_windows.push(DroppedWindow {
                        semantic_key: WindowSemanticKey::new(key),
                        reason: err.failure_class,
                        field: err.field.to_string(),
                        payload_fragment: err.payload_fragment,
                    }),
                }
            }
        }
    }

    if windows.is_empty() {
        return Err(FailureClass::MissingRequiredField);
    }

    let extra_usage = root.get("extra_usage").and_then(parse_extra_usage);

    Ok(AnthropicReading {
        windows,
        provider_observed_at: None,
        extra_usage,
        dropped_windows,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WindowParseError {
    field: &'static str,
    payload_fragment: String,
    failure_class: FailureClass,
}

fn parse_window(
    key: &str,
    scope: WindowScope,
    obj: &serde_json::Map<String, serde_json::Value>,
    nominal_duration: NominalWindowDuration,
) -> Result<MeterWindow, WindowParseError> {
    let fragment = serde_json::to_string(obj).unwrap_or_default();

    let util_val = obj.get("utilization").ok_or_else(|| WindowParseError {
        field: "utilization",
        payload_fragment: fragment.clone(),
        failure_class: FailureClass::MissingRequiredField,
    })?;
    let util_num = util_val.as_f64().ok_or_else(|| WindowParseError {
        field: "utilization",
        payload_fragment: fragment.clone(),
        failure_class: FailureClass::MissingRequiredField,
    })?;

    if !(0.0..=100.0).contains(&util_num) || !util_num.is_finite() {
        return Err(WindowParseError {
            field: "utilization",
            payload_fragment: fragment.clone(),
            failure_class: FailureClass::MissingRequiredField,
        });
    }

    let ppm_raw = (util_num * 10_000.0).round() as i32;
    let ppm = QuotaFractionPpm::new(ppm_raw).ok_or_else(|| WindowParseError {
        field: "utilization",
        payload_fragment: fragment.clone(),
        failure_class: FailureClass::MissingRequiredField,
    })?;
    let quota_used = QuotaUsed::new(ppm);

    let resets_val = obj.get("resets_at").ok_or_else(|| WindowParseError {
        field: "resets_at",
        payload_fragment: fragment.clone(),
        failure_class: FailureClass::MissingRequiredField,
    })?;

    let reset_state = if resets_val.is_null() {
        WindowResetState::NotStarted
    } else if let Some(resets_str) = resets_val.as_str() {
        let ts = UtcTimestamp::parse_rfc3339(resets_str).ok_or_else(|| WindowParseError {
            field: "resets_at",
            payload_fragment: fragment.clone(),
            failure_class: FailureClass::MissingRequiredField,
        })?;
        WindowResetState::Known(ts)
    } else {
        return Err(WindowParseError {
            field: "resets_at",
            payload_fragment: fragment.clone(),
            failure_class: FailureClass::MissingRequiredField,
        });
    };

    let resolution_ppm = QuotaFractionPpm::new(100).expect("100 ppm is valid non-zero");
    let reported_resolution =
        ReportedResolution::new(resolution_ppm).expect("100 ppm is valid non-zero resolution");

    Ok(MeterWindow::new(
        WindowSemanticKey::new(key),
        scope,
        quota_used,
        reported_resolution,
        QuantizationSemantics::Exact,
        reset_state,
        nominal_duration,
    ))
}

fn parse_extra_usage(val: &serde_json::Value) -> Option<AnthropicExtraUsage> {
    let obj = val.as_object()?;
    let is_enabled = obj
        .get("is_enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let monthly_limit = obj.get("monthly_limit").and_then(|v| v.as_u64());
    let used_credits = obj.get("used_credits").and_then(|v| v.as_u64());
    let utilization = obj
        .get("utilization")
        .and_then(|v| v.as_f64())
        .and_then(|u| {
            if (0.0..=100.0).contains(&u) && u.is_finite() {
                let ppm = (u * 10_000.0).round() as i32;
                QuotaFractionPpm::new(ppm)
            } else {
                None
            }
        });

    Some(AnthropicExtraUsage {
        is_enabled,
        monthly_limit,
        used_credits,
        utilization,
    })
}

fn parse_401_auth_reason(body: &[u8]) -> AuthReason {
    if let Ok(val) = serde_json::from_slice::<serde_json::Value>(body) {
        let msg = val
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .or_else(|| val.get("message").and_then(|m| m.as_str()))
            .unwrap_or("");

        let msg_lower = msg.to_lowercase();
        if msg_lower.contains("expired") {
            return AuthReason::ProviderDeclaredExpiry;
        }
    }
    AuthReason::CredentialRejected
}

fn parse_retry_after(response: &HttpResponse) -> Option<MonotonicDuration> {
    let header_val = response.header("retry-after")?;
    let secs = header_val.trim().parse::<u64>().ok()?;
    Some(MonotonicDuration::from_seconds(secs))
}

impl ProviderAdapter for AnthropicAdapter {
    type Reading = AnthropicReading;

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
        let token = match extract_bearer_token(credential) {
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

        let req = HttpRequest::get(&self.endpoint_url, timeouts)
            .with_header("Authorization", format!("Bearer {token}"))
            .with_header("anthropic-beta", Self::ANTHROPIC_BETA_HEADER)
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

        let sensitive = SensitiveResponseMaterial::new([credential.expose(), token.as_str()]);
        let evidence = capture_json_response(&response, &sensitive);
        let observation = match response.status() {
            200 => match replay_anthropic_capsule(evidence.serialized(), request) {
                Ok(reading) => ProviderObservation::Measured(reading),
                Err(failure) => ProviderObservation::Unreachable(failure),
            },
            401 => {
                let reason = parse_401_auth_reason(response.body());
                ProviderObservation::AuthRequired(reason)
            }
            403 => {
                // For Anthropic, a 403 Forbidden is a permission or policy rejection,
                // not an authentication credential expiry. Documented in module header.
                ProviderObservation::Unreachable(FailureClass::HttpStatus(
                    HttpStatusClass::ClientError,
                ))
            }
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

        fn ok_with_header(
            status: u16,
            header_name: &str,
            header_val: &str,
            body: impl Into<Vec<u8>>,
        ) -> Self {
            Self {
                response: Ok(HttpResponse {
                    status,
                    headers: vec![(header_name.to_string(), header_val.to_string())],
                    body: body.into(),
                }),
            }
        }

        fn err(failure: FailureClass) -> Self {
            Self {
                response: Err(failure),
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

    const FIXTURE_VALID_SUCCESS: &[u8] =
        include_bytes!("../../tests/fixtures/meter/anthropic/valid-success.json");
    const FIXTURE_ZERO_PERCENTAGE: &[u8] =
        include_bytes!("../../tests/fixtures/meter/anthropic/zero-percentage.json");
    const FIXTURE_MULTIPLE_WINDOWS: &[u8] =
        include_bytes!("../../tests/fixtures/meter/anthropic/multiple-windows.json");
    const FIXTURE_MODEL_SPECIFIC: &[u8] =
        include_bytes!("../../tests/fixtures/meter/anthropic/model-specific.json");
    const FIXTURE_ERROR_401_INVALID: &[u8] =
        include_bytes!("../../tests/fixtures/meter/anthropic/error-401-invalid.json");
    const FIXTURE_ERROR_401_EXPIRED: &[u8] =
        include_bytes!("../../tests/fixtures/meter/anthropic/error-401-expired.json");
    const FIXTURE_ERROR_403_AMBIGUOUS: &[u8] =
        include_bytes!("../../tests/fixtures/meter/anthropic/error-403-ambiguous.json");
    const FIXTURE_ERROR_429: &[u8] =
        include_bytes!("../../tests/fixtures/meter/anthropic/error-429.json");
    const FIXTURE_MALFORMED: &[u8] =
        include_bytes!("../../tests/fixtures/meter/anthropic/malformed.json");
    const FIXTURE_MISSING_FIELD: &[u8] =
        include_bytes!("../../tests/fixtures/meter/anthropic/missing-field.json");
    const FIXTURE_UNKNOWN_FIELDS: &[u8] =
        include_bytes!("../../tests/fixtures/meter/anthropic/unknown-fields.json");
    const FIXTURE_STALE_TIMESTAMP: &[u8] =
        include_bytes!("../../tests/fixtures/meter/anthropic/stale-timestamp.json");
    const FIXTURE_RESET_CHANGED_A: &[u8] =
        include_bytes!("../../tests/fixtures/meter/anthropic/reset-changed-a.json");
    const FIXTURE_RESET_CHANGED_B: &[u8] =
        include_bytes!("../../tests/fixtures/meter/anthropic/reset-changed-b.json");
    const FIXTURE_IDLE_FIVE_HOUR: &[u8] =
        include_bytes!("../../tests/fixtures/meter/anthropic/idle-five-hour.json");

    fn test_adapter() -> AnthropicAdapter {
        AnthropicAdapter::new()
    }

    fn test_credential() -> CredentialHandle {
        CredentialHandle::new("test-oauth-token-12345")
    }

    fn test_clock() -> FakeClock {
        FakeClock::new(UtcTimestamp::from_unix_nanos(1_700_000_000_000_000_000))
    }

    /// Destructures a successful reading from the observation. The exhaustive
    /// three-arm match over [`ProviderObservation`] is what keeps the crate-wide
    /// `clippy::wildcard_enum_match_arm` deny satisfied: each variant is named.
    fn expect_measured(obs: ProviderObservation<AnthropicReading>) -> AnthropicReading {
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
    fn case_01_valid_success() {
        let adapter = test_adapter();
        let transport = MockTransport::ok(200, FIXTURE_VALID_SUCCESS);
        let clock = test_clock();
        let obs = adapter.observe(
            &test_credential(),
            &MeterRequest::default(),
            &transport,
            &clock,
        );

        let reading = expect_measured(obs);
        assert_eq!(reading.windows.len(), 3);

        let five_hour = &reading.windows[0];
        assert_eq!(five_hour.semantic_key().as_str(), "five_hour");
        assert_eq!(*five_hour.scope(), WindowScope::AccountWide);
        assert_eq!(five_hour.quota_used().as_ppm().get(), 80_000);
        assert_eq!(five_hour.nominal_duration().as_nanos(), 18_000_000_000_000);

        let seven_day = &reading.windows[1];
        assert_eq!(seven_day.semantic_key().as_str(), "seven_day");
        assert_eq!(*seven_day.scope(), WindowScope::AccountWide);
        assert_eq!(seven_day.quota_used().as_ppm().get(), 910_000);
        assert_eq!(seven_day.nominal_duration().as_nanos(), 604_800_000_000_000);

        let sonnet = &reading.windows[2];
        assert_eq!(sonnet.semantic_key().as_str(), "seven_day_sonnet");
        assert_eq!(
            *sonnet.scope(),
            WindowScope::ModelSpecific(ModelId::new("sonnet"))
        );
        assert_eq!(sonnet.quota_used().as_ppm().get(), 0);

        let extra = reading.extra_usage.expect("extra_usage is present");
        assert!(!extra.is_enabled);
    }

    #[test]
    fn case_02_zero_percentage() {
        let adapter = test_adapter();
        let transport = MockTransport::ok(200, FIXTURE_ZERO_PERCENTAGE);
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
    fn case_03_multiple_windows() {
        let adapter = test_adapter();
        let transport = MockTransport::ok(200, FIXTURE_MULTIPLE_WINDOWS);
        let clock = test_clock();
        let obs = adapter.observe(
            &test_credential(),
            &MeterRequest::default(),
            &transport,
            &clock,
        );

        let reading = expect_measured(obs);
        assert_eq!(reading.windows.len(), 4);
        let keys: Vec<&str> = reading
            .windows
            .iter()
            .map(|w| w.semantic_key().as_str())
            .collect();
        assert!(keys.contains(&"five_hour"));
        assert!(keys.contains(&"seven_day"));
        assert!(keys.contains(&"seven_day_sonnet"));
        assert!(keys.contains(&"seven_day_opus"));
    }

    #[test]
    fn case_04_model_specific_windows() {
        let adapter = test_adapter();
        let transport = MockTransport::ok(200, FIXTURE_MODEL_SPECIFIC);
        let clock = test_clock();
        let obs = adapter.observe(
            &test_credential(),
            &MeterRequest::default(),
            &transport,
            &clock,
        );

        let reading = expect_measured(obs);
        let sonnet = reading
            .windows
            .iter()
            .find(|w| w.semantic_key().as_str() == "seven_day_sonnet")
            .expect("seven_day_sonnet must be present");
        assert_eq!(
            *sonnet.scope(),
            WindowScope::ModelSpecific(ModelId::new("sonnet"))
        );
        assert_eq!(sonnet.quota_used().as_ppm().get(), 180_000);
    }

    #[test]
    fn case_05_401_invalid_credential() {
        let adapter = test_adapter();
        let transport = MockTransport::ok(401, FIXTURE_ERROR_401_INVALID);
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
    fn case_06_provider_declared_expiry() {
        let adapter = test_adapter();
        let transport = MockTransport::ok(401, FIXTURE_ERROR_401_EXPIRED);
        let clock = test_clock();
        let obs = adapter.observe(
            &test_credential(),
            &MeterRequest::default(),
            &transport,
            &clock,
        );
        assert_eq!(
            obs,
            ProviderObservation::AuthRequired(AuthReason::ProviderDeclaredExpiry)
        );
    }

    #[test]
    fn case_07_403_ambiguous_semantics() {
        let adapter = test_adapter();
        let transport = MockTransport::ok(403, FIXTURE_ERROR_403_AMBIGUOUS);
        let clock = test_clock();
        let obs = adapter.observe(
            &test_credential(),
            &MeterRequest::default(),
            &transport,
            &clock,
        );
        // Ambiguous 403 is classified as HttpStatus ClientError, never AuthRequired
        assert_eq!(
            obs,
            ProviderObservation::Unreachable(FailureClass::HttpStatus(
                HttpStatusClass::ClientError
            ))
        );
    }

    #[test]
    fn case_08_429_rate_limited() {
        let adapter = test_adapter();
        let transport = MockTransport::ok_with_header(429, "Retry-After", "30", FIXTURE_ERROR_429);
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
    fn case_09_timeout() {
        let adapter = test_adapter();
        let transport = MockTransport::err(FailureClass::ReadTimeout);
        let clock = test_clock();
        let obs = adapter.observe(
            &test_credential(),
            &MeterRequest::default(),
            &transport,
            &clock,
        );
        assert_eq!(
            obs,
            ProviderObservation::Unreachable(FailureClass::ReadTimeout)
        );
    }

    #[test]
    fn case_10_malformed_json() {
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

    #[test]
    fn case_11_missing_expected_field() {
        let adapter = test_adapter();
        let transport = MockTransport::ok(200, FIXTURE_MISSING_FIELD);
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
    fn case_12_unknown_additional_field() {
        let adapter = test_adapter();
        let transport = MockTransport::ok(200, FIXTURE_UNKNOWN_FIELDS);
        let clock = test_clock();
        let captured = adapter.observe_with_evidence(
            &test_credential(),
            &MeterRequest::default(),
            &transport,
            &clock,
        );

        let reading = expect_measured(captured.observation);
        assert_eq!(reading.windows.len(), 2);
        // Unknown-field retention is the capsule's job (aub-eun.5), not the
        // normalized reading's: the field survives in the evidence capsule
        // without ever becoming a required normalization field.
        let capsule = captured
            .evidence
            .expect("a 200 response must carry an evidence capsule");
        // Checked inside the canonical quota_response subtree specifically,
        // not just anywhere in the serialized capsule: the raw-lexeme map
        // also carries the field's JSON pointer path in its keys, so a
        // substring check against the whole capsule would still pass even if
        // the sanitizer dropped the field from quota_response itself.
        let parsed: serde_json::Value = serde_json::from_str(capsule.serialized()).unwrap();
        assert!(
            parsed["quota_response"]
                .get("unknown_top_level_metric")
                .is_some(),
            "the capsule's quota_response must retain the unknown field: {}",
            capsule.serialized()
        );
    }

    #[test]
    fn case_13_stale_server_timestamp() {
        let adapter = test_adapter();
        let transport = MockTransport::ok(200, FIXTURE_STALE_TIMESTAMP);
        let clock = test_clock();
        let obs = adapter.observe(
            &test_credential(),
            &MeterRequest::default(),
            &transport,
            &clock,
        );

        let reading = expect_measured(obs);
        assert_eq!(reading.windows.len(), 2);
        let expected_ts = UtcTimestamp::parse_rfc3339("2020-01-01T00:00:00.000Z")
            .expect("valid RFC3339 timestamp");
        assert_eq!(reading.windows[0].resets_at(), Some(expected_ts));
    }

    #[test]
    fn case_14_reset_change() {
        let adapter = test_adapter();
        let clock = test_clock();

        let transport_a = MockTransport::ok(200, FIXTURE_RESET_CHANGED_A);
        let obs_a = adapter.observe(
            &test_credential(),
            &MeterRequest::default(),
            &transport_a,
            &clock,
        );

        let transport_b = MockTransport::ok(200, FIXTURE_RESET_CHANGED_B);
        let obs_b = adapter.observe(
            &test_credential(),
            &MeterRequest::default(),
            &transport_b,
            &clock,
        );

        let reading_a = expect_measured(obs_a);
        let reading_b = expect_measured(obs_b);

        assert_ne!(
            reading_a.windows[0].resets_at(),
            reading_b.windows[0].resets_at()
        );
    }

    #[test]
    fn case_15_idle_five_hour_window() {
        let adapter = test_adapter();
        let transport = MockTransport::ok(200, FIXTURE_IDLE_FIVE_HOUR);
        let clock = test_clock();
        let obs = adapter.observe(
            &test_credential(),
            &MeterRequest::default(),
            &transport,
            &clock,
        );

        let reading = expect_measured(obs);
        assert_eq!(reading.windows.len(), 2);
        assert_eq!(reading.windows[0].semantic_key().as_str(), "five_hour");
        assert!(reading.windows[0].reset_state().is_not_started());
        assert_eq!(reading.windows[0].resets_at(), None);
        assert_eq!(reading.windows[0].quota_used().as_ppm().get(), 0);

        assert_eq!(reading.windows[1].semantic_key().as_str(), "seven_day");
        assert_eq!(
            reading.windows[1].resets_at(),
            Some(UtcTimestamp::parse_rfc3339("2026-09-06T12:00:00.000Z").unwrap())
        );
        assert!(reading.dropped_windows.is_empty());
    }

    #[test]
    fn window_with_missing_field_dropped_and_other_window_stored() {
        let adapter = test_adapter();
        let body = br#"{
            "five_hour": {
                "utilization": 10.0,
                "resets_at": "2026-08-30T19:00:00.000Z"
            },
            "seven_day": {
                "resets_at": "2026-09-06T12:00:00.000Z"
            }
        }"#;
        let transport = MockTransport::ok(200, body);
        let clock = test_clock();
        let obs = adapter.observe(
            &test_credential(),
            &MeterRequest::default(),
            &transport,
            &clock,
        );

        let reading = expect_measured(obs);
        assert_eq!(reading.windows.len(), 1);
        assert_eq!(reading.windows[0].semantic_key().as_str(), "five_hour");
        assert_eq!(reading.dropped_windows.len(), 1);
        assert_eq!(
            reading.dropped_windows[0].semantic_key.as_str(),
            "seven_day"
        );
        assert_eq!(
            reading.dropped_windows[0].reason,
            FailureClass::MissingRequiredField
        );
        assert_eq!(reading.dropped_windows[0].field, "utilization");
    }

    #[test]
    fn missing_top_level_five_hour_fails_entire_parse() {
        let adapter = test_adapter();
        let body = br#"{
            "seven_day": {
                "utilization": 91.0,
                "resets_at": "2026-09-06T12:00:00.000Z"
            }
        }"#;
        let transport = MockTransport::ok(200, body);
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
    fn declarations_and_semantic_identifiers() {
        let adapter = test_adapter();
        let decls = adapter.declarations();
        assert_eq!(decls.measurement_basis, MeasurementBasis::LocallyReceived);
        assert_eq!(
            decls.provider_contract_id.as_str(),
            "anthropic-oauth-usage-v1"
        );
        assert_eq!(
            decls.meter_semantics_id.as_str(),
            "anthropic-subscription-v1"
        );

        // A test adapter declaring changed physical semantics uses a different semantic ID
        let changed_decls = AdapterDeclarations::new(
            MeasurementBasis::LocallyReceived,
            ProviderContractId::new("anthropic-oauth-usage-v1"),
            MeterSemanticsId::new("anthropic-subscription-v2"),
        );
        let changed_adapter = AnthropicAdapter::with_declarations(
            AnthropicAdapter::DEFAULT_ENDPOINT,
            changed_decls.clone(),
        );
        assert_ne!(
            adapter.declarations().meter_semantics_id,
            changed_adapter.declarations().meter_semantics_id
        );
    }

    #[test]
    fn credential_extraction_formats() {
        // Raw token
        let cred_raw = CredentialHandle::new("raw-token-123");
        assert_eq!(extract_bearer_token(&cred_raw).unwrap(), "raw-token-123");

        // JSON with claudeAiOauth.accessToken
        let cred_json_oauth =
            CredentialHandle::new(r#"{"claudeAiOauth":{"accessToken":"oauth-tok-456"}}"#);
        assert_eq!(
            extract_bearer_token(&cred_json_oauth).unwrap(),
            "oauth-tok-456"
        );

        // JSON with accessToken
        let cred_json_acc = CredentialHandle::new(r#"{"accessToken":"acc-tok-789"}"#);
        assert_eq!(extract_bearer_token(&cred_json_acc).unwrap(), "acc-tok-789");

        // Empty raw credential
        let cred_empty = CredentialHandle::new("");
        assert_eq!(
            extract_bearer_token(&cred_empty).unwrap_err(),
            AuthReason::CredentialExpired
        );

        // JSON with empty token
        let cred_json_empty = CredentialHandle::new(r#"{"accessToken":""}"#);
        assert_eq!(
            extract_bearer_token(&cred_json_empty).unwrap_err(),
            AuthReason::CredentialExpired
        );
    }
}
