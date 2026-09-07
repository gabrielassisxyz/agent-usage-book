//! The OpenCode Go workspace-page meter adapter (aub-8hu3).
//!
//! The OpenCode Go usage meter has no public endpoint: the authoritative
//! surface is the workspace page (`GET https://opencode.ai/workspace/<id>/go`)
//! as seen in a signed-in browser, and the usage meters live in the page's
//! embedded initial state script. This adapter fetches that page with the
//! session cookie the caller resolved (an `env` credential whose value is the
//! `Cookie:` header value verbatim), reads the three usage windows from the
//! state script, and answers with a typed reading. The reference documenting
//! the shape is `https://ai.rud.is/posts/2026-06-06-opencode-go-usage/`; its
//! per-window fields are `percent` (integer 0..100) and `reset_in_sec`, for
//! `rolling`, `weekly` and `monthly`, plus `plan` and `fetched_at`.
//!
//! The request never follows a redirect: the provider answers an expired
//! session by redirecting to its sign-in page, so the redirect response is
//! the authentication signal and arrives here unfollowed (the transport's
//! `without_redirects`). A page whose state script is absent is
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
    MeterWindow, NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
    WindowSemanticKey,
};
use crate::meter::adapter::{
    AdapterDeclarations, CredentialHandle, HttpTransport, MeterRequest, ProviderAdapter,
    ProviderObservation, RequiredWindowKinds,
};
use crate::meter::evidence::{
    CapturedProviderResponse, JsonEvidenceCapsule, SensitiveResponseMaterial, capture_json_body,
    quota_response_from_capsule,
};
use crate::meter::transport::{CommandBudget, HttpRequest, HttpResponse, RequestTimeoutConfig};

/// The page-state marker the parser keys on: the literal field name that only
/// the embedded initial state script carries, one of the field names the
/// reference documents for the store state. A page with no script element
/// containing this marker has no readable state, and is schema drift.
pub const STATE_MARKER: &str = "reset_in_sec";

/// The provider base the workspace page hangs from. The full page URL is
/// `{DEFAULT_PAGE_BASE}/workspace/<id>/go`; the id is account configuration
/// handed across the boundary in [`MeterRequest::workspace_id`].
pub const DEFAULT_PAGE_BASE: &str = "https://opencode.ai";

/// The one cookie-header credential: the reference's client authenticates
/// with a session cookie named `auth`, and the operator exports the whole
/// `Cookie:` header value by hand (decided in `aub-r7k0`).
pub const COOKIE_HEADER: &str = "Cookie";

/// The typed success reading produced by [`OpenCodeAdapter`]: one row per
/// usage window the state script carries, with the reset anchored at the
/// instant the page was received.
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
/// The workspace page URL comes from two places, in override order: the
/// endpoint override the caller resolved from the environment (the same
/// channel `AUB_ANTHROPIC_ENDPOINT` uses for the Anthropic adapter, so an
/// end-to-end run can point this adapter at a synthetic server), and
/// otherwise the account's workspace id, which the caller resolved from
/// configuration and handed over in [`MeterRequest::workspace_id`]. The
/// adapter never reads configuration or the environment itself (rule `07`).
pub struct OpenCodeAdapter {
    endpoint_override: Option<String>,
    declarations: AdapterDeclarations,
}

impl OpenCodeAdapter {
    pub const DEFAULT_CONTRACT_ID: &'static str = "opencode-go-workspace-page-v1";
    pub const DEFAULT_SEMANTICS_ID: &'static str = "opencode-go-subscription-v1";
    pub const REQUIRED_WINDOW_KINDS: &'static [&'static str] = &["rolling", "weekly", "monthly"];

    /// Builds the adapter with an optional full workspace-page URL override.
    pub fn new(endpoint_override: Option<String>) -> Self {
        Self {
            endpoint_override,
            declarations: AdapterDeclarations::new(
                // The page documents no provider measurement time:
                // `fetched_at` is the page generation stamp, so the reading's
                // basis is the local receive instant, the same anchor the
                // reset arithmetic uses.
                MeasurementBasis::LocallyReceived,
                ProviderContractId::new(Self::DEFAULT_CONTRACT_ID),
                MeterSemanticsId::new(Self::DEFAULT_SEMANTICS_ID),
            )
            .with_required_window_kinds(RequiredWindowKinds::from_values(
                Self::REQUIRED_WINDOW_KINDS,
            )),
        }
    }

    /// The workspace page URL for one observation: the override when the
    /// caller resolved one, otherwise the workspace page of the account's
    /// workspace id.
    fn page_url(&self, workspace_id: Option<&str>) -> Result<String, FailureClass> {
        match (&self.endpoint_override, workspace_id) {
            (Some(url), _) => Ok(url.clone()),
            (None, Some(id)) if !id.trim().is_empty() => {
                Ok(format!("{DEFAULT_PAGE_BASE}/workspace/{id}/go"))
            }
            (None, _) => Err(FailureClass::MissingRequiredField),
        }
    }

    pub fn endpoint_override(&self) -> Option<&str> {
        self.endpoint_override.as_deref()
    }
}
/// The integer percent to parts-per-million step: the provider reports whole
/// percentages, so one percent is exactly 10 000 ppm.
const PPM_PER_PERCENT: i64 = 10_000;

/// Extracts the state JSON object from a workspace page body: the first
/// `<script>` element whose text carries [`STATE_MARKER`], and within it the
/// brace-balanced JSON object starting at the first `{`. A page with no such
/// script is [`FailureClass::SchemaDrift`]; a marked script with no
/// extractable object is [`FailureClass::MalformedBody`].
fn extract_state_json(body: &[u8]) -> Result<String, FailureClass> {
    let text = std::str::from_utf8(body).map_err(|_| FailureClass::MalformedBody)?;
    for script in script_elements(text) {
        if script.contains(STATE_MARKER) {
            return extract_json_object(script).ok_or(FailureClass::MalformedBody);
        }
    }
    Err(FailureClass::SchemaDrift)
}

/// Yields the text between every `<script ...>` opening tag and its matching
/// `</script>`, in document order. The scan is delimiter-based rather than a
/// HTML parse on purpose: the reference's client does the same, and the
/// state script is the only element this contract reads.
fn script_elements(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        let opening = rest.find("<script")?;
        let after_tag = rest[opening..].find('>')? + opening + 1;
        let closing = rest[after_tag..].find("</script>")? + after_tag;
        let script = &rest[after_tag..closing];
        rest = &rest[closing + "</script>".len()..];
        Some(script)
    })
}

/// Extracts the brace-balanced JSON object from `script`, starting at the
/// first `{` and respecting double-quoted strings, so a brace inside a state
/// string never ends the object early.
fn extract_json_object(script: &str) -> Option<String> {
    let start = script.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, character) in script[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(script[start..=start + offset].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Parses the state JSON object a workspace page carries into the reading's
/// windows, keyed by the reference's field names. Every required window must
/// be present with an integer percent in 0..=100 and a non-negative integer
/// `reset_in_sec`, and every reset is anchored at the instant the page was
/// received: the provider states the interval, `aub` states the anchor.
fn parse_state(
    root: &serde_json::Value,
    received_at: UtcTimestamp,
) -> Result<Vec<MeterWindow>, FailureClass> {
    let mut windows = Vec::new();
    for kind in [WindowKind::Rolling, WindowKind::Weekly, WindowKind::Monthly] {
        windows.push(kind.parse_window(root, received_at)?);
    }
    Ok(windows)
}

/// The three usage windows the reference documents, each with its semantic
/// key and the nominal length this adapter stores for it. The reference post
/// documents no rolling length, so rolling stores the one length the
/// reference names anywhere (its tool's page labels the rolling meter
/// "5-hour Usage") and reads the bead's measured-between-resets refinement
/// once calibration observes it; the two calendar windows are the bead's own
/// 7-day and 30-day decisions.
#[derive(Debug, Clone, Copy)]
enum WindowKind {
    Rolling,
    Weekly,
    Monthly,
}

impl WindowKind {
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

    /// The quantization each window's percent carries: the two calendar
    /// windows are exact at the provider's integer-percent resolution; the
    /// rolling window's length is provisional until measured, and its
    /// quantization states that honest `Unknown` rather than claiming exact.
    fn quantization(self) -> QuantizationSemantics {
        match self {
            WindowKind::Rolling => QuantizationSemantics::Unknown,
            WindowKind::Weekly | WindowKind::Monthly => QuantizationSemantics::Exact,
        }
    }

    /// Parses one window object from the state root: the raw fields, the
    /// percent-to-ppm step, and the reset anchored at the receive instant.
    fn parse_window(
        self,
        root: &serde_json::Value,
        received_at: UtcTimestamp,
    ) -> Result<MeterWindow, FailureClass> {
        let key = self.semantic_key();
        let object = root
            .get(key.as_str())
            .and_then(|value| value.as_object())
            .ok_or(FailureClass::MissingRequiredField)?;
        let percent = object
            .get("percent")
            .ok_or(FailureClass::MissingRequiredField)?
            .as_i64()
            .ok_or(FailureClass::MalformedBody)?;
        if !(0..=100).contains(&percent) {
            return Err(FailureClass::MalformedBody);
        }
        let reset_in_sec = object
            .get("reset_in_sec")
            .ok_or(FailureClass::MissingRequiredField)?
            .as_i64()
            .ok_or(FailureClass::MalformedBody)?;
        if reset_in_sec < 0 {
            return Err(FailureClass::MalformedBody);
        }
        let reset_nanos = reset_in_sec
            .checked_mul(1_000_000_000)
            .and_then(|nanos| received_at.unix_nanos().checked_add(nanos))
            .ok_or(FailureClass::MalformedBody)?;
        let percent_ppm = percent
            .checked_mul(PPM_PER_PERCENT)
            .and_then(|ppm| i32::try_from(ppm).ok())
            .and_then(QuotaFractionPpm::new)
            .ok_or(FailureClass::MalformedBody)?;
        Ok(MeterWindow::new(
            key,
            WindowScope::AccountWide,
            QuotaUsed::new(percent_ppm),
            integer_percent_resolution(),
            self.quantization(),
            UtcTimestamp::from_unix_nanos(reset_nanos),
            self.nominal_duration(),
        ))
    }
}

/// The reported resolution of an integer percent: one percent, 10 000 ppm.
fn integer_percent_resolution() -> ReportedResolution {
    // 10 000 ppm is one whole percent, statically inside the fraction's
    // domain and non-zero, so both constructors' refusals are unreachable
    // for this constant.
    ReportedResolution::new(QuotaFractionPpm::new(10_000).expect("one percent is a valid fraction"))
        .expect("one percent is a non-zero resolution")
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
        // The credential is the `Cookie:` header value verbatim, decided in
        // `aub-r7k0`: no trimming, no reinterpretation, and an empty material
        // is an expired credential rather than an unauthenticated request.
        let material = credential.expose();
        if material.is_empty() {
            return CapturedProviderResponse::without_response(ProviderObservation::AuthRequired(
                AuthReason::CredentialExpired,
            ));
        }
        let page_url = match self.page_url(request.workspace_id.as_deref()) {
            Ok(url) => url,
            Err(failure) => {
                return CapturedProviderResponse::without_response(
                    ProviderObservation::Unreachable(failure),
                );
            }
        };

        let timeouts = RequestTimeoutConfig::new(
            MonotonicDuration::from_seconds(5),
            MonotonicDuration::from_seconds(10),
            Some(MonotonicDuration::from_seconds(15)),
        );
        let req = HttpRequest::get(&page_url, timeouts)
            .with_header(COOKIE_HEADER, material.to_string())
            .with_header("Accept", "text/html")
            .with_header("User-Agent", "agent-usage-book/0.1.0")
            // The redirect is the authentication signal: an expired session
            // answers with a redirect to the sign-in page, so the 3xx must
            // arrive here as the response it is.
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
        // The anchor for every `reset_in_sec` derivation: the instant the
        // page arrived. The provider states the interval, `aub` states the
        // anchor, and the capsule keeps the raw seconds beside the derived
        // instants so the arithmetic stays auditable.
        let received_at = clock.now();
        let (observation, evidence) = match response.status() {
            200 => reading_from_response(&response, received_at, material),
            // Any redirect is the sign-in redirect: the reference's client
            // checks the redirect target for a sign-in path and reports the
            // session as invalid or expired, and a workspace page that
            // redirects elsewhere is equally not a usable state page.
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
            // that state them.
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
        // On a parse failure the sanitized state body rides along for the
        // bounded failure store (`aub-2r3`); on every other outcome there is
        // no failed body to keep.
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
        CapturedProviderResponse {
            observation,
            evidence,
            failed_body,
        }
    }
}

/// Reads one successful workspace response: the state JSON is extracted and
/// sanitized into the evidence capsule first, then the windows are parsed
/// from the capsule's own quota subtree, so the retained evidence is exactly
/// what the reading was derived from.
fn reading_from_response(
    response: &HttpResponse,
    received_at: UtcTimestamp,
    cookie_material: &str,
) -> (
    ProviderObservation<OpenCodeReading>,
    Option<JsonEvidenceCapsule>,
) {
    let state = match extract_state_json(response.body()) {
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
    let evidence = capture_json_body(state.as_bytes(), &sensitive);
    let quota = match quota_response_from_capsule(evidence.serialized()) {
        Ok(quota) => quota,
        Err(_) => {
            return (
                ProviderObservation::Unreachable(FailureClass::MalformedBody),
                Some(evidence),
            );
        }
    };
    let observation = match parse_state(&quota, received_at) {
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
        "/tests/fixtures/meter/opencode/valid.html"
    ));
    const FIXTURE_LOGIN_REDIRECT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/meter/opencode/login-redirect.html"
    ));
    const FIXTURE_NO_MARKER: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/meter/opencode/no-state-marker.html"
    ));
    const FIXTURE_MALFORMED: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/meter/opencode/malformed-state.html"
    ));

    /// The fixture cookie string: distinctive on purpose, so the leak grep
    /// over the capsule matches nothing but a leak, and free of every shared
    /// forbidden pattern (no credential-shaped prefix, no at sign, no path).
    const FIXTURE_COOKIE: &str = "auth=fixture-session-cookie-9f2c-not-a-real-value";
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
            Self {
                response: Ok(HttpResponse {
                    status: 200,
                    headers: Vec::new(),
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

    fn observing(transport: &SyntheticTransport) -> CapturedProviderResponse<OpenCodeReading> {
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1_000_000_000));
        let adapter = OpenCodeAdapter::new(None);
        let request = MeterRequest {
            model: None,
            workspace_id: Some(FIXTURE_WORKSPACE_ID.to_string()),
        };
        adapter.observe_with_evidence(
            &CredentialHandle::new(FIXTURE_COOKIE),
            &request,
            transport,
            &clock,
        )
    }

    /// Case 01: the valid page parses to the three required windows, each
    /// carrying the fixture's percent as its exact parts-per-million value.
    #[test]
    fn case_01_valid_page_yields_three_windows_at_integer_percent_resolution() {
        let transport = SyntheticTransport::serving(FIXTURE_VALID.as_bytes());
        let captured = observing(&transport);
        let ProviderObservation::Measured(reading) = captured.observation else {
            panic!("the valid fixture must measure: {:?}", captured.observation);
        };
        assert_eq!(reading.windows.len(), 3);
        for (key, percent, reset_in_sec) in [
            ("rolling", 23, 3612),
            ("weekly", 41, 302931),
            ("monthly", 7, 1814211),
        ] {
            let window = reading
                .windows
                .iter()
                .find(|window| window.semantic_key().as_str() == key)
                .unwrap_or_else(|| panic!("the {key} window must be present"));
            assert_eq!(
                window.quota_used().as_ppm().get(),
                percent * 10_000,
                "the {key} percent converts to ppm by the integer-percent step"
            );
            assert_eq!(window.reported_resolution().as_ppm().get(), 10_000);
            // The reset anchor: the receive instant is the fake clock's
            // 1_000_000_000, so each reset is exactly reset_in_sec later,
            // with literal instants the derivation can be audited against.
            assert_eq!(
                window.resets_at(),
                Some(UtcTimestamp::from_unix_nanos(
                    1_000_000_000 + i64::from(reset_in_sec) * 1_000_000_000
                ))
            );
            assert!(window.reset_state().is_known());
        }
        // The two calendar windows are exact; the rolling window's length is
        // provisional until measured, so its quantization stays Unknown.
        let quantization = |key: &str| {
            reading
                .windows
                .iter()
                .find(|window| window.semantic_key().as_str() == key)
                .unwrap()
                .quantization()
        };
        assert_eq!(quantization("weekly"), QuantizationSemantics::Exact);
        assert_eq!(quantization("monthly"), QuantizationSemantics::Exact);
        assert_eq!(quantization("rolling"), QuantizationSemantics::Unknown);
        assert!(
            captured.evidence.is_some(),
            "a measured page keeps its capsule"
        );
        assert!(captured.failed_body.is_none());
    }

    /// Case 02: the sign-in redirect is the authentication conclusion, and
    /// the adapter saw the redirect itself because the request asked the
    /// transport not to follow it.
    #[test]
    fn case_02_login_redirect_is_auth_required() {
        let transport =
            SyntheticTransport::with_status(302, "/signin", FIXTURE_LOGIN_REDIRECT.as_bytes());
        let captured = observing(&transport);
        assert_eq!(
            captured.observation,
            ProviderObservation::AuthRequired(AuthReason::CredentialExpired)
        );
        assert!(captured.evidence.is_none());
        let request = transport.request();
        assert!(
            !request.follow_redirects,
            "the workspace request must not follow redirects"
        );
    }

    /// Case 03: a page with no state script is schema drift, never a silent
    /// zero and never a malformed-body claim about a body that parsed.
    #[test]
    fn case_03_page_without_the_state_marker_is_schema_drift() {
        let transport = SyntheticTransport::serving(FIXTURE_NO_MARKER.as_bytes());
        let captured = observing(&transport);
        assert_eq!(
            captured.observation,
            ProviderObservation::Unreachable(FailureClass::SchemaDrift)
        );
        assert!(captured.evidence.is_none());
    }

    /// Case 04: a marked script whose JSON is truncated is a parse failure.
    /// A truncated object never brace-balances, so there is no state object
    /// to capture and no capsule rides along; the retained-body path is the
    /// planted negative below, where the state parses but a window fails.
    #[test]
    fn case_04_malformed_state_is_the_parse_failure_class() {
        let transport = SyntheticTransport::serving(FIXTURE_MALFORMED.as_bytes());
        let captured = observing(&transport);
        assert_eq!(
            captured.observation,
            ProviderObservation::Unreachable(FailureClass::MalformedBody)
        );
        assert!(
            captured.evidence.is_none() && captured.failed_body.is_none(),
            "a state that never brace-balances has nothing to retain"
        );
    }

    /// A state object that parses as JSON but carries an unusable window
    /// field fails the parse with the sanitized state body retained for the
    /// bounded failure store (`aub-2r3`).
    #[test]
    fn a_state_that_parses_but_fails_the_window_parse_keeps_the_sanitized_body() {
        let bad_percent = br#"<html><body><script>{"rolling":{"percent":23,"reset_in_sec":3612,"status":"ok"},"weekly":{"percent":141,"reset_in_sec":302931,"status":"ok"},"monthly":{"percent":7,"reset_in_sec":1814211,"status":"ok"},"plan":"Go"}</script></body></html>"#;
        let transport = SyntheticTransport::serving(bad_percent);
        let captured = observing(&transport);
        assert_eq!(
            captured.observation,
            ProviderObservation::Unreachable(FailureClass::MalformedBody)
        );
        let failed_body = captured
            .failed_body
            .expect("the sanitized state body rides along on the failure");
        let state: serde_json::Value =
            serde_json::from_slice(&failed_body).expect("the retained body is still valid JSON");
        assert_eq!(state.get("plan"), Some(&serde_json::json!("Go")));
        assert!(
            !String::from_utf8_lossy(&failed_body).contains(FIXTURE_COOKIE),
            "the retained body never carries the cookie material"
        );
    }

    /// The declarations are the calibration-facing identity of the adapter:
    /// the contract and semantics ids the bead names, and the three required
    /// windows, readable without a provider call.
    #[test]
    fn the_declarations_carry_the_contract_semantics_and_required_windows() {
        let adapter = OpenCodeAdapter::new(None);
        let declarations = adapter.declarations();
        assert_eq!(
            declarations.provider_contract_id.as_str(),
            "opencode-go-workspace-page-v1"
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

    /// The protection the capsule contract names is the sanitizer being fed
    /// the credential material: a page that echoes the session value in an
    /// innocuous field must still yield a capsule without it.
    #[test]
    fn the_sanitizer_removes_the_cookie_even_when_the_page_echoes_it() {
        let echoed = "<html><body><script>{\"rolling\":{\"percent\":23,\"reset_in_sec\":3612,\"status\":\"ok\"},\"weekly\":{\"percent\":41,\"reset_in_sec\":302931,\"status\":\"ok\"},\"monthly\":{\"percent\":7,\"reset_in_sec\":1814211,\"status\":\"ok\"},\"plan\":\"Go\",\"session_id\":\"".to_string()
            + FIXTURE_COOKIE
            + "\"}</script></body></html>";
        let transport = SyntheticTransport::serving(echoed.as_bytes());
        let captured = observing(&transport);
        let ProviderObservation::Measured(_) = captured.observation else {
            panic!(
                "the echoed page must still measure: {:?}",
                captured.observation
            );
        };
        let serialized = captured
            .evidence
            .as_ref()
            .expect("a measured page captures a capsule")
            .serialized()
            .to_string();
        assert!(
            !serialized.contains(FIXTURE_COOKIE),
            "the echoed session value must be sanitized out of the capsule"
        );
    }

    /// The request contract: the workspace page of the account's workspace
    /// id, the cookie material in the `Cookie` header and nowhere else.
    #[test]
    fn the_request_carries_the_cookie_header_and_the_workspace_url_alone() {
        let transport = SyntheticTransport::serving(FIXTURE_VALID.as_bytes());
        let _ = observing(&transport);
        let request = transport.request();
        assert_eq!(
            request.url,
            format!("{DEFAULT_PAGE_BASE}/workspace/{FIXTURE_WORKSPACE_ID}/go")
        );
        assert!(!request.follow_redirects);
        let mut cookie_headers = 0;
        for (name, value) in &request.headers {
            if name.eq_ignore_ascii_case(COOKIE_HEADER) {
                cookie_headers += 1;
                assert_eq!(value, FIXTURE_COOKIE, "the material goes out verbatim");
            } else {
                assert!(
                    !value.contains(FIXTURE_COOKIE),
                    "no header but {COOKIE_HEADER} may carry the cookie material"
                );
            }
        }
        assert_eq!(cookie_headers, 1, "exactly one Cookie header goes out");
    }

    /// The endpoint override wins over the workspace-id construction, which
    /// is how an end-to-end run points this adapter at a synthetic server.
    #[test]
    fn the_endpoint_override_replaces_the_workspace_url() {
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(0));
        let adapter =
            OpenCodeAdapter::new(Some("http://127.0.0.1:9/workspace/wrk_x/go".to_string()));
        let transport = SyntheticTransport::serving(FIXTURE_VALID.as_bytes());
        let request = MeterRequest {
            model: None,
            workspace_id: Some("wrk_unused".to_string()),
        };
        let _ = adapter.observe_with_evidence(
            &CredentialHandle::new(FIXTURE_COOKIE),
            &request,
            &transport,
            &clock,
        );
        assert_eq!(
            transport.request().url,
            "http://127.0.0.1:9/workspace/wrk_x/go"
        );
    }

    /// An observation without a workspace id and without an override has no
    /// page to fetch, and says so instead of hitting a half-built URL.
    #[test]
    fn an_observation_without_a_workspace_id_is_a_missing_required_field() {
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(0));
        let adapter = OpenCodeAdapter::new(None);
        let transport = SyntheticTransport::serving(FIXTURE_VALID.as_bytes());
        let request = MeterRequest {
            model: None,
            workspace_id: None,
        };
        let captured = adapter.observe_with_evidence(
            &CredentialHandle::new(FIXTURE_COOKIE),
            &request,
            &transport,
            &clock,
        );
        assert_eq!(
            captured.observation,
            ProviderObservation::Unreachable(FailureClass::MissingRequiredField)
        );
        assert!(
            transport.seen_request.take().is_none(),
            "no request may go out for an unresolvable page URL"
        );
    }

    /// The evidence capsule carries the raw state the reading came from, and
    /// never the cookie material: the serialized capsule is grepped for the
    /// fixture cookie string, which must match nothing.
    #[test]
    fn the_capsule_holds_the_raw_state_and_never_the_cookie() {
        let transport = SyntheticTransport::serving(FIXTURE_VALID.as_bytes());
        let captured = observing(&transport);
        let capsule = captured
            .evidence
            .as_ref()
            .expect("a measured page captures a capsule");
        let serialized = capsule.serialized();
        assert!(
            !serialized.contains(FIXTURE_COOKIE),
            "the cookie material must never enter the capsule"
        );
        // The raw provider facts ride along per window: the capsule's quota
        // subtree is the state object, so every percent and reset_in_sec is
        // readable in it, keeping the derived instants auditable.
        let quota = quota_response_from_capsule(serialized).expect("the capsule holds the state");
        for (key, percent, reset_in_sec) in [
            ("rolling", 23, 3612),
            ("weekly", 41, 302931),
            ("monthly", 7, 1814211),
        ] {
            let window = quota.get(key).expect("the raw window object");
            assert_eq!(window.get("percent"), Some(&serde_json::json!(percent)));
            assert_eq!(
                window.get("reset_in_sec"),
                Some(&serde_json::json!(reset_in_sec))
            );
        }
        // The `plan` and `fetched_at` fields the reference documents survive
        // into the capsule too, though the reading derives nothing from them.
        assert_eq!(quota.get("plan"), Some(&serde_json::json!("Go")));
        assert!(quota.get("fetched_at").is_some());
    }

    /// The parse contract over planted negatives: a wrong percent value, a
    /// fractional percent, and a missing window each fail the parse instead
    /// of yielding a partial reading.
    #[test]
    fn planted_negatives_fail_the_parse() {
        let received_at = UtcTimestamp::from_unix_nanos(1_000_000_000);
        let parse_windows = |state: &str| {
            let root: serde_json::Value = serde_json::from_str(state).unwrap();
            parse_state(&root, received_at)
        };
        // Percent out of range: the provider's field is 0..=100.
        let out_of_range = r#"{"rolling":{"percent":101,"reset_in_sec":10},"weekly":{"percent":1,"reset_in_sec":10},"monthly":{"percent":1,"reset_in_sec":10}}"#;
        assert_eq!(
            parse_windows(out_of_range),
            Err(FailureClass::MalformedBody)
        );
        // Fractional percent: the contract's integer percent, not a float.
        let fractional = r#"{"rolling":{"percent":1.5,"reset_in_sec":10},"weekly":{"percent":1,"reset_in_sec":10},"monthly":{"percent":1,"reset_in_sec":10}}"#;
        assert_eq!(parse_windows(fractional), Err(FailureClass::MalformedBody));
        // A missing required window: refused, never defaulted to zero.
        let missing = r#"{"rolling":{"percent":1,"reset_in_sec":10},"weekly":{"percent":1,"reset_in_sec":10}}"#;
        assert_eq!(
            parse_windows(missing),
            Err(FailureClass::MissingRequiredField)
        );
        // Negative reset interval: nonsense, refused.
        let negative_reset = r#"{"rolling":{"percent":1,"reset_in_sec":-1},"weekly":{"percent":1,"reset_in_sec":10},"monthly":{"percent":1,"reset_in_sec":10}}"#;
        assert_eq!(
            parse_windows(negative_reset),
            Err(FailureClass::MalformedBody)
        );
    }

    /// A state string containing a brace inside a quoted value never ends
    /// the JSON object early: the brace matcher respects strings.
    #[test]
    fn the_brace_matcher_respects_quoted_braces() {
        let script = r#" prefix {"plan":"brace } inside","rolling":{"percent":1,"reset_in_sec":2},"weekly":{"percent":3,"reset_in_sec":4},"monthly":{"percent":5,"reset_in_sec":6}} trailing"#;
        let extracted = extract_json_object(script).expect("the object extracts whole");
        let parsed: serde_json::Value = serde_json::from_str(&extracted).expect("valid JSON");
        assert_eq!(
            parsed.get("plan"),
            Some(&serde_json::json!("brace } inside"))
        );
    }
}
