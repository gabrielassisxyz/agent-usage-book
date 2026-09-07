//! The Codex provider meter adapter (`aub-cg6k`).
//!
//! Reads the ChatGPT rate limits the Codex CLI already persists locally: the
//! newest `rollout-*.jsonl` under the account's Codex home carries the
//! provider's own `rate_limits` block, and the adapter turns it into two
//! account-wide windows, a 5-hour `primary` and a 7-day `secondary`. There
//! is no network call anywhere in this adapter; the bytes cross the same
//! [`HttpTransport`] port an HTTP request takes through the transport
//! request's local-file source, so evidence capture and the synthetic
//! transport seam work unchanged (the decision recorded on the bead: reading
//! the file directly inside the adapter was rejected because it bypasses
//! evidence capture and the synthetic transport at once).
//!
//! The account identity comes from the JWT payload in the same home's
//! `auth.json`, which is this adapter's credential material: `tokens.id_token`
//! carries the email and the plan. **The JWT is not verified.** Its signature
//! is never checked; it is used as a label on the observation, never as an
//! authentication fact, and the token bytes are registered as sensitive
//! material so they can never reach the evidence capsule.
//!
//! A home with no rollout is an observation of class `NoEvidence` - spelled
//! [`FailureClass::MalformedBody`], the class the Anthropic adapter reports
//! for an empty body - not an error, and never a fabricated window. The
//! adapter reads the newest rollout only: an older rollout with a rate-limit
//! record is never consulted when the newest one carries none, because
//! reporting an older window's numbers against the account's present quota
//! is the stale-reading-rendered-as-fresh defect this project exists to
//! prevent (this is a deliberate divergence from `bin/quota-bars`, which
//! falls back to older files because scanning every file takes minutes).
//!
//! # Boundary rules
//!
//! May not depend on:
//! - SQLite directly (rule `03`)
//! - credential or configuration modules (rule `07`): the credential arrives
//!   resolved, and the home directory arrives as request parameters
//! - the ureq transport driver (rule `12`)
//! - write-capable filesystem facilities (rule `17`): every byte this adapter
//!   reads crosses the transport port

use crate::domain::failure::FailureClass;
use crate::domain::ids::{MeterSemanticsId, ProviderContractId};
use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
use crate::domain::time::{
    Clock, MeasurementBasis, MonotonicDuration, ProviderObservedAt, UtcTimestamp,
};
use crate::domain::window::{
    MeterWindow, NominalWindowDuration, QuantizationSemantics, ReportedResolution,
    WindowResetState, WindowScope, WindowSemanticKey,
};
use crate::meter::adapter::{
    AdapterDeclarations, CredentialHandle, HttpTransport, MeterRequest, ProviderAdapter,
    ProviderObservation, RequiredWindowKinds,
};
use crate::meter::evidence::{
    CapturedProviderResponse, SensitiveResponseMaterial, capture_json_body,
    quota_response_from_capsule,
};
use crate::meter::transport::{
    CommandBudget, HttpRequest, LOCAL_FILE_MTIME_HEADER, LOCAL_FILE_PATH_HEADER,
    RequestTimeoutConfig,
};

/// The account identity the adapter decoded from the credential material's
/// JWT payload. Carried on the reading as the label the observation was made
/// under; the orchestrator persists only what [`crate::meter::sampler::
/// MeteredReading`] exposes, so neither field of this struct reaches the
/// ledger, and both are registered as sensitive material so neither can
/// reach the evidence capsule either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexIdentity {
    /// The ChatGPT plan label the JWT payload carries, the observed-plan
    /// shaped string (for example `plus`). The JWT is not verified; this is
    /// the provider's own claim, kept as a label and never an authority.
    pub plan: Option<String>,
    /// The account email, the identity the token labels. It never leaves
    /// this reading: the sampler persists windows and instants, and the
    /// sanitizer holds the email as known-sensitive material.
    pub email: Option<String>,
}

/// The typed observation reading produced by [`CodexAdapter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexReading {
    pub windows: Vec<MeterWindow>,
    /// The instant the provider wrote the chosen rollout record, taken from
    /// the file's modification time. This is the reading's measurement time:
    /// a local file's own mtime is the closest thing to a provider timestamp
    /// a local source carries, hence the `ProviderObserved` basis. An mtime
    /// days behind the read is honest idleness, not clock skew: the
    /// clock-skew envelope applies ahead of the receive timestamp only, so a
    /// stale rollout ages into `AgeExceeded` while a future mtime is still a
    /// `ClockAnomaly` (aub-3o0w).
    pub provider_observed_at: Option<ProviderObservedAt>,
    /// The identity decoded from the credential material's JWT payload, when
    /// the credential carries a decodable one. Its absence never blocks the
    /// measurement: the rate limits stand on the rollout record alone.
    pub identity: Option<CodexIdentity>,
    pub provider_contract_id: ProviderContractId,
}

/// The Codex provider adapter: the provider's own `rate_limits` records read
/// from the account's newest local rollout, through the transport port.
pub struct CodexAdapter {
    declarations: AdapterDeclarations,
}

impl CodexAdapter {
    /// The provider contract this adapter parses against: the rollout
    /// records' `rate_limits` shape.
    pub const CONTRACT_ID: &'static str = "openai-codex-rollout-rate-limits-v1";
    /// What a reading from this adapter physically means: a ChatGPT
    /// subscription's usage windows.
    pub const SEMANTICS_ID: &'static str = "openai-chatgpt-subscription-v1";
    /// The window kinds a measured reading must carry. The provider names
    /// them `primary` and `secondary` inside `rate_limits`.
    pub const REQUIRED_WINDOW_KINDS: &'static [&'static str] = &["primary", "secondary"];

    /// The primary window's length in minutes when the record omits
    /// `window_minutes`: five hours.
    pub const PRIMARY_DEFAULT_WINDOW_MINUTES: u64 = 300;
    /// The secondary window's length in minutes when the record omits
    /// `window_minutes`: seven days.
    pub const SECONDARY_DEFAULT_WINDOW_MINUTES: u64 = 10_080;

    /// The provider publishes whole percentage points, so every reading's
    /// resolution is one percent, that is 10 000 parts per million.
    const RESOLUTION_PPM: i32 = 10_000;

    /// Where the Codex CLI keeps its session rollouts, under the configured
    /// home.
    const SESSIONS_SUBDIR: &'static str = "sessions";
    /// The rollout file-name glob the transport's newest-match arm resolves.
    const ROLLOUT_GLOB: &'static str = "rollout-*.jsonl";

    pub fn new() -> Self {
        Self {
            declarations: AdapterDeclarations::new(
                MeasurementBasis::ProviderObserved,
                ProviderContractId::new(Self::CONTRACT_ID),
                MeterSemanticsId::new(Self::SEMANTICS_ID),
            )
            .with_required_window_kinds(RequiredWindowKinds::from_values(
                Self::REQUIRED_WINDOW_KINDS,
            )),
        }
    }
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

/// The JWT claim namespace the ChatGPT plan lives under.
const JWT_AUTH_CLAIM_NAMESPACE: &str = "https://api.openai.com/auth";

/// Decodes the account identity the credential material carries. The
/// credential for a Codex account is the home's `auth.json` content, and its
/// `tokens.id_token` is a JWT whose payload carries the email and the plan.
/// The JWT is not verified: its signature is never checked, because the
/// payload is used as a label on an observation the provider's own rollout
/// record already stands behind, never as an authentication decision.
fn decode_identity(credential: &CredentialHandle) -> Option<CodexIdentity> {
    let auth: serde_json::Value = serde_json::from_str(credential.expose().trim()).ok()?;
    let token = auth.get("tokens")?.get("id_token")?.as_str()?;
    let payload = decode_jwt_payload_segment(token)?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    let email = claims
        .get("email")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let plan = claims
        .get(JWT_AUTH_CLAIM_NAMESPACE)
        .and_then(|namespace| namespace.get("chatgpt_plan_type"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    if email.is_none() && plan.is_none() {
        return None;
    }
    Some(CodexIdentity { plan, email })
}

/// Decodes the payload segment of a JWT: exactly three dot-separated
/// segments, base64url, no signature verification of any kind.
fn decode_jwt_payload_segment(token: &str) -> Option<Vec<u8>> {
    let mut segments = token.split('.');
    let _header = segments.next()?;
    let payload = segments.next()?;
    let _signature = segments.next()?;
    if segments.next().is_some() {
        return None;
    }
    base64url_decode(payload)
}

/// Base64url decoding without padding and without a dependency: the only
/// base64 this crate needs is one JWT payload segment, and the alphabet is
/// fixed by RFC 4648 section 5.
fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    const INVALID: u8 = 64;
    // Index by ASCII value: A-Z 0-25, a-z 26-51, 0-9 52-61, `-` 62, `_` 63.
    fn value_of(byte: u8) -> u8 {
        match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => INVALID,
        }
    }

    let bytes: Vec<u8> = text
        .bytes()
        .filter(|byte| *byte != b'=')
        .map(value_of)
        .collect();
    if bytes.contains(&INVALID) {
        return None;
    }
    let mut decoded = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        match chunk.len() {
            2 => {
                decoded.push((chunk[0] << 2) | (chunk[1] >> 4));
            }
            3 => {
                decoded.push((chunk[0] << 2) | (chunk[1] >> 4));
                decoded.push((chunk[1] << 4) | (chunk[2] >> 2));
            }
            4 => {
                decoded.push((chunk[0] << 2) | (chunk[1] >> 4));
                decoded.push((chunk[1] << 4) | (chunk[2] >> 2));
                decoded.push((chunk[2] << 6) | chunk[3]);
            }
            _ => return None,
        }
    }
    Some(decoded)
}

/// The `rate_limits` block of the last JSONL line in the rollout that
/// carries one. A rollout is a log: an empty line is skipped, and any
/// non-empty line that does not parse as JSON makes the source untrustworthy
/// (a mid-write truncation is genuinely malformed evidence, and refusing the
/// observation records that instead of silently using the healthy prefix).
/// A file whose every line parses but none carries the block is the
/// missing-required-field class: the source parsed and the contract's field
/// was absent.
fn last_rate_limits_block(body: &[u8]) -> Result<serde_json::Value, FailureClass> {
    let text = std::str::from_utf8(body).map_err(|_| FailureClass::MalformedBody)?;
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            return Err(FailureClass::MalformedBody);
        };
        if let Some(block) = record
            .get("payload")
            .and_then(|payload| payload.get("rate_limits"))
        {
            found = Some(block.clone());
        }
    }
    found.ok_or(FailureClass::MissingRequiredField)
}

/// Parses the provider's own `rate_limits` block into the two required
/// windows. `primary` and `secondary` are required kinds: a window object
/// that is absent, null, or unusable refuses the observation with
/// `MissingRequiredField` rather than fabricating a window, and a window
/// present without `resets_at` is [`WindowResetState::NotStarted`] - the
/// provider's own way of saying the window has not started.
fn parse_rate_limits(rate_limits: &serde_json::Value) -> Result<Vec<MeterWindow>, FailureClass> {
    let primary = parse_rate_limits_window(
        rate_limits.get("primary"),
        "primary",
        CodexAdapter::PRIMARY_DEFAULT_WINDOW_MINUTES,
    )?;
    let secondary = parse_rate_limits_window(
        rate_limits.get("secondary"),
        "secondary",
        CodexAdapter::SECONDARY_DEFAULT_WINDOW_MINUTES,
    )?;
    Ok(vec![primary, secondary])
}

fn parse_rate_limits_window(
    window: Option<&serde_json::Value>,
    kind: &str,
    default_window_minutes: u64,
) -> Result<MeterWindow, FailureClass> {
    let object = window
        .and_then(serde_json::Value::as_object)
        .ok_or(FailureClass::MissingRequiredField)?;

    let used_percent = object
        .get("used_percent")
        .and_then(serde_json::Value::as_f64)
        .ok_or(FailureClass::MissingRequiredField)?;
    if !(0.0..=100.0).contains(&used_percent) || !used_percent.is_finite() {
        return Err(FailureClass::MissingRequiredField);
    }
    let ppm = (used_percent * 10_000.0).round() as i32;
    let quota_used =
        QuotaUsed::new(QuotaFractionPpm::new(ppm).ok_or(FailureClass::MissingRequiredField)?);

    let window_minutes = match object.get("window_minutes") {
        None | Some(serde_json::Value::Null) => default_window_minutes,
        Some(value) => {
            let minutes = value.as_u64().ok_or(FailureClass::MissingRequiredField)?;
            if minutes == 0 {
                return Err(FailureClass::MissingRequiredField);
            }
            minutes
        }
    };

    let reset_state = match object.get("resets_at") {
        None | Some(serde_json::Value::Null) => WindowResetState::NotStarted,
        // The epoch-seconds spelling every rollout record on the reference
        // machine carries.
        Some(serde_json::Value::Number(seconds)) => {
            let seconds = seconds.as_i64().ok_or(FailureClass::MissingRequiredField)?;
            let nanos = seconds
                .checked_mul(1_000_000_000)
                .filter(|nanos| *nanos >= 0)
                .ok_or(FailureClass::MissingRequiredField)?;
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(nanos))
        }
        // The RFC 3339 spelling the usage-page surface uses.
        Some(serde_json::Value::String(text)) => WindowResetState::Known(
            UtcTimestamp::parse_rfc3339(text).ok_or(FailureClass::MissingRequiredField)?,
        ),
        Some(_) => return Err(FailureClass::MissingRequiredField),
    };

    let resolution = QuotaFractionPpm::new(CodexAdapter::RESOLUTION_PPM)
        .expect("10 000 ppm is a valid non-zero quota fraction");
    Ok(MeterWindow::new(
        WindowSemanticKey::new(kind),
        WindowScope::AccountWide,
        quota_used,
        ReportedResolution::new(resolution).expect("10 000 ppm is a valid non-zero resolution"),
        // The provider publishes integer percents, so every ppm value is the
        // nearest reading of a fraction quantized to the reported resolution.
        QuantizationSemantics::RoundedToNearest,
        reset_state,
        NominalWindowDuration::from_nanos(window_minutes * 60 * 1_000_000_000),
    ))
}

/// Reinterprets retained evidence with the current Codex semantics. The
/// stored capsule remains unchanged, so callers can persist a corrected
/// observation beside the original interpretation.
fn replay_codex_capsule(capsule: &str) -> Result<CodexReading, FailureClass> {
    let quota_response = quota_response_from_capsule(capsule).map_err(|message| {
        if message == "capsule does not contain a quota response" {
            FailureClass::MalformedBody
        } else {
            FailureClass::MissingRequiredField
        }
    })?;
    let rate_limits = quota_response
        .get("rate_limits")
        .ok_or(FailureClass::MissingRequiredField)?;
    let windows = parse_rate_limits(rate_limits)?;
    let provider_observed_at = quota_response
        .get("source")
        .and_then(|source| source.get("mtime_nanos"))
        .and_then(serde_json::Value::as_i64)
        .filter(|nanos| *nanos >= 0)
        .map(|nanos| ProviderObservedAt::new(UtcTimestamp::from_unix_nanos(nanos)));
    Ok(CodexReading {
        windows,
        provider_observed_at,
        identity: None,
        provider_contract_id: ProviderContractId::new(CodexAdapter::CONTRACT_ID),
    })
}

impl ProviderAdapter for CodexAdapter {
    type Reading = CodexReading;

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
        let Some(home) = &request.local_home else {
            // The request parameters the caller owed: a file-backed meter has
            // no source without the account's home directory, and a caller
            // that dispatched the codex adapter without one violated the
            // dispatch contract, which is a missing-required-field outcome,
            // never a fabricated reading.
            return CapturedProviderResponse::without_response(ProviderObservation::Unreachable(
                FailureClass::MissingRequiredField,
            ));
        };

        let identity = decode_identity(credential);
        let sensitive = SensitiveResponseMaterial::new([
            credential.expose(),
            identity
                .as_ref()
                .and_then(|identity| identity.email.as_deref())
                .unwrap_or_default(),
        ]);

        let timeouts = RequestTimeoutConfig::new(
            MonotonicDuration::from_seconds(5),
            MonotonicDuration::from_seconds(10),
            Some(MonotonicDuration::from_seconds(15)),
        );
        let request = HttpRequest::newest_local_file(
            home.join(Self::SESSIONS_SUBDIR),
            Self::ROLLOUT_GLOB,
            timeouts,
        );
        let budget = CommandBudget::new(MonotonicDuration::from_seconds(30), clock);
        let response = match transport.send(&request, &budget, clock) {
            Ok(response) => response,
            Err(failure) => {
                return CapturedProviderResponse::without_response(
                    ProviderObservation::Unreachable(failure),
                );
            }
        };

        // Which file was read and when the provider wrote it are the two
        // source facts the capsule records. The transport reports them in its
        // response headers; the capsule excludes headers, so they enter the
        // body this adapter captures instead. The mtime crosses as epoch
        // nanos: the one spelling that carries no format ambiguity.
        let (source_path, source_mtime_nanos) = match (
            response.header(LOCAL_FILE_PATH_HEADER),
            response
                .header(LOCAL_FILE_MTIME_HEADER)
                .and_then(|value| value.parse::<i64>().ok()),
        ) {
            (Some(path), Some(mtime)) => (path.to_string(), mtime),
            _ => {
                // The transport answered without the source facts its
                // local-file arm owes; the evidence is untrustworthy as a
                // quota record, not merely incomplete.
                let evidence = capture_json_body(response.body(), &sensitive);
                return CapturedProviderResponse {
                    observation: ProviderObservation::Unreachable(
                        FailureClass::MissingRequiredField,
                    ),
                    evidence: Some(evidence),
                    failed_body: None,
                };
            }
        };

        let block = match last_rate_limits_block(response.body()) {
            Ok(block) => block,
            Err(failure) => {
                // The failure still carries a content hash over the raw file,
                // exactly as an unparseable HTTP body would.
                let evidence = capture_json_body(response.body(), &sensitive);
                return CapturedProviderResponse {
                    observation: ProviderObservation::Unreachable(failure),
                    evidence: Some(evidence),
                    failed_body: None,
                };
            }
        };

        // The composed capture body: the quota-relevant subtree plus the
        // provider-observed source the file read resolved to. Capturing this
        // (rather than the raw JSONL) keeps the evidence path identical to
        // every other adapter: one sanitized JSON capsule, its raw scalar
        // lexemes, and the content hash of the bytes interpreted.
        let capture_body = serde_json::json!({
            "rate_limits": block,
            "source": {
                "path": source_path,
                "mtime_nanos": source_mtime_nanos,
            },
        });
        let capture_bytes =
            serde_json::to_vec(&capture_body).expect("a JSON value always serializes");
        let evidence = capture_json_body(&capture_bytes, &sensitive);

        let observation = match replay_codex_capsule(evidence.serialized()) {
            Ok(mut reading) => {
                reading.identity = identity;
                ProviderObservation::Measured(reading)
            }
            Err(failure) => ProviderObservation::Unreachable(failure),
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
    use crate::meter::transport::{BlockingTransport, CommandBudget, HttpResponse};

    use crate::meter::evidence::JsonEvidenceCapsule;

    /// The synthetic transport (`aub-cg6k`): serves the programmed fixture
    /// bytes for any local-file request, reporting the resolved source path
    /// and modification time the real disk arm would. This is the seam the
    /// local-file source exists to keep: the adapter is exercised against
    /// fixtures with no filesystem behind it at all.
    struct FixtureTransport {
        body: Vec<u8>,
        source_path: String,
        mtime_nanos: i64,
    }

    impl FixtureTransport {
        fn serving(body: &'static [u8]) -> Self {
            Self {
                body: body.to_vec(),
                source_path: "/fixture/codex-home/sessions/2026/09/05/rollout-fixture.jsonl"
                    .to_string(),
                mtime_nanos: 1_788_646_100_000_000_000,
            }
        }
    }

    impl HttpTransport for FixtureTransport {
        fn send(
            &self,
            request: &HttpRequest,
            _budget: &CommandBudget,
            _clock: &impl Clock,
        ) -> Result<HttpResponse, FailureClass> {
            assert!(
                request.local_file.is_some(),
                "the codex adapter reads only through the local-file arm"
            );
            Ok(HttpResponse {
                status: 200,
                headers: vec![
                    (LOCAL_FILE_PATH_HEADER.to_string(), self.source_path.clone()),
                    (
                        LOCAL_FILE_MTIME_HEADER.to_string(),
                        self.mtime_nanos.to_string(),
                    ),
                ],
                body: self.body.clone(),
            })
        }
    }

    const FIXTURE_VALID_TWO_WINDOWS: &[u8] =
        include_bytes!("../../tests/fixtures/meter/codex/valid-two-windows.json");
    const FIXTURE_SECONDARY_WITHOUT_RESET: &[u8] =
        include_bytes!("../../tests/fixtures/meter/codex/secondary-without-reset.json");
    const FIXTURE_MISSING_WINDOW_MINUTES: &[u8] =
        include_bytes!("../../tests/fixtures/meter/codex/missing-window-minutes.json");
    const FIXTURE_NO_RATE_LIMITS_LINE: &[u8] =
        include_bytes!("../../tests/fixtures/meter/codex/no-rate-limits-line.json");
    const FIXTURE_MALFORMED: &[u8] =
        include_bytes!("../../tests/fixtures/meter/codex/malformed.json");
    const FIXTURE_AUTH: &[u8] =
        include_bytes!("../../tests/fixtures/meter/codex/auth-fixture.json");

    fn test_adapter() -> CodexAdapter {
        CodexAdapter::new()
    }

    fn test_credential() -> CredentialHandle {
        CredentialHandle::new(String::from_utf8(FIXTURE_AUTH.to_vec()).unwrap())
    }

    fn test_request() -> MeterRequest {
        MeterRequest {
            local_home: Some(std::path::PathBuf::from("/fixture/codex-home")),
            ..MeterRequest::default()
        }
    }

    fn test_clock() -> FakeClock {
        FakeClock::new(UtcTimestamp::from_unix_nanos(1_788_646_200_000_000_000))
    }

    fn observe_with(
        adapter: &CodexAdapter,
        transport: &impl HttpTransport,
    ) -> CapturedProviderResponse<CodexReading> {
        adapter.observe_with_evidence(
            &test_credential(),
            &test_request(),
            transport,
            &test_clock(),
        )
    }

    fn expect_measured(captured: CapturedProviderResponse<CodexReading>) -> CodexReading {
        match captured.observation {
            ProviderObservation::Measured(reading) => reading,
            ProviderObservation::AuthRequired(reason) => {
                panic!("expected Measured, got AuthRequired({reason:?})")
            }
            ProviderObservation::Unreachable(failure) => {
                panic!("expected Measured, got Unreachable({failure:?})")
            }
        }
    }

    fn expect_unreachable(
        captured: CapturedProviderResponse<CodexReading>,
    ) -> (FailureClass, Option<JsonEvidenceCapsule>) {
        let CapturedProviderResponse {
            observation,
            evidence,
            ..
        } = captured;
        match observation {
            ProviderObservation::Unreachable(failure) => (failure, evidence),
            ProviderObservation::Measured(_) => panic!("expected Unreachable, got Measured"),
            ProviderObservation::AuthRequired(reason) => {
                panic!("expected Unreachable, got AuthRequired({reason:?})")
            }
        }
    }

    /// Case 01: both windows carry a `resets_at`, so both come back `Known`,
    /// with the percent scaled to parts per million at the provider's
    /// integer-percent resolution, and the last rate-limit line in the file
    /// is the one used (the earlier line carries deliberately different
    /// values, so a naive first-line implementation fails here).
    #[test]
    fn case_01_valid_two_windows() {
        let adapter = test_adapter();
        let transport = FixtureTransport::serving(FIXTURE_VALID_TWO_WINDOWS);
        let captured = observe_with(&adapter, &transport);
        let reading = expect_measured(captured);

        assert_eq!(reading.windows.len(), 2);
        let primary = &reading.windows[0];
        assert_eq!(primary.semantic_key().as_str(), "primary");
        assert_eq!(*primary.scope(), WindowScope::AccountWide);
        assert_eq!(primary.quota_used().as_ppm().get(), 420_000);
        assert_eq!(primary.reported_resolution().as_ppm().get(), 10_000);
        assert_eq!(
            primary.nominal_duration().as_nanos(),
            300 * 60 * 1_000_000_000
        );
        match primary.reset_state() {
            WindowResetState::Known(resets_at) => {
                assert_eq!(resets_at.unix_nanos(), 1_788_650_033_000_000_000)
            }
            other @ (WindowResetState::NotStarted | WindowResetState::Scheduled { .. }) => {
                panic!("primary carries a known reset, got {other:?}")
            }
        }

        let secondary = &reading.windows[1];
        assert_eq!(secondary.semantic_key().as_str(), "secondary");
        assert_eq!(*secondary.scope(), WindowScope::AccountWide);
        assert_eq!(secondary.quota_used().as_ppm().get(), 70_000);
        assert_eq!(
            secondary.nominal_duration().as_nanos(),
            10_080 * 60 * 1_000_000_000
        );
        match secondary.reset_state() {
            WindowResetState::Known(resets_at) => {
                assert_eq!(resets_at.unix_nanos(), 1_789_174_263_000_000_000)
            }
            other @ (WindowResetState::NotStarted | WindowResetState::Scheduled { .. }) => {
                panic!("secondary carries a known reset, got {other:?}")
            }
        }

        // The reading's measurement time is the provider-written file's
        // modification time, and the identity comes from the JWT payload.
        let observed = reading
            .provider_observed_at
            .expect("the reading carries the file's mtime as its measurement time");
        assert_eq!(observed.as_utc().unix_nanos(), 1_788_646_100_000_000_000);
        let identity = reading.identity.expect("the fixture credential decodes");
        assert_eq!(
            identity.email.as_deref(),
            Some("fixture-account@example.test")
        );
        assert_eq!(identity.plan.as_deref(), Some("plus"));
        assert_eq!(
            reading.provider_contract_id.as_str(),
            CodexAdapter::CONTRACT_ID
        );
    }

    /// Case 02: the secondary window is present with its usage but carries no
    /// `resets_at`, so it is `NotStarted` while the primary stays `Known`.
    #[test]
    fn case_02_secondary_without_reset() {
        let adapter = test_adapter();
        let transport = FixtureTransport::serving(FIXTURE_SECONDARY_WITHOUT_RESET);
        let captured = observe_with(&adapter, &transport);
        let reading = expect_measured(captured);

        assert_eq!(reading.windows.len(), 2);
        assert!(matches!(
            reading.windows[0].reset_state(),
            WindowResetState::Known(_)
        ));
        assert_eq!(
            reading.windows[1].quota_used().as_ppm().get(),
            70_000,
            "the not-started window still carries the usage the provider reported"
        );
        assert_eq!(
            reading.windows[1].reset_state(),
            WindowResetState::NotStarted
        );
    }

    /// Case 03: `window_minutes` is absent on both windows, so the nominal
    /// durations come from the defaults, 300 and 10 080 minutes.
    #[test]
    fn case_03_missing_window_minutes() {
        let adapter = test_adapter();
        let transport = FixtureTransport::serving(FIXTURE_MISSING_WINDOW_MINUTES);
        let captured = observe_with(&adapter, &transport);
        let reading = expect_measured(captured);

        assert_eq!(reading.windows.len(), 2);
        assert_eq!(
            reading.windows[0].nominal_duration().as_nanos(),
            300 * 60 * 1_000_000_000,
            "the primary window defaults to five hours"
        );
        assert_eq!(
            reading.windows[1].nominal_duration().as_nanos(),
            10_080 * 60 * 1_000_000_000,
            "the secondary window defaults to seven days"
        );
        assert!(matches!(
            reading.windows[0].reset_state(),
            WindowResetState::Known(_)
        ));
        assert!(matches!(
            reading.windows[1].reset_state(),
            WindowResetState::Known(_)
        ));
    }

    /// Case 04: every line parses but no line carries `rate_limits`. The
    /// source parsed and the contract's field was absent, so the observation
    /// is refused with the missing-required-field class, and the raw file
    /// still leaves a content hash behind as evidence.
    #[test]
    fn case_04_no_rate_limits_line() {
        let adapter = test_adapter();
        let transport = FixtureTransport::serving(FIXTURE_NO_RATE_LIMITS_LINE);
        let captured = observe_with(&adapter, &transport);
        let (failure, evidence) = expect_unreachable(captured);

        assert_eq!(failure, FailureClass::MissingRequiredField);
        let evidence = evidence.expect("a refused observation still captures evidence");
        assert!(!evidence.body_hash().is_empty());
    }

    /// Case 05: the file is not a rollout at all. A non-empty line that does
    /// not parse makes the source untrustworthy: malformed body, not a
    /// silently empty reading.
    #[test]
    fn case_05_malformed() {
        let adapter = test_adapter();
        let transport = FixtureTransport::serving(FIXTURE_MALFORMED);
        let captured = observe_with(&adapter, &transport);
        let (failure, evidence) = expect_unreachable(captured);

        assert_eq!(failure, FailureClass::MalformedBody);
        let evidence = evidence.expect("a malformed source still captures its hash");
        assert!(!evidence.body_hash().is_empty());
    }

    /// Case 06: the newest rollout wins, over a temp tree with three dated
    /// subdirectories exactly as Codex nests them. Each file's mtime is
    /// pinned explicitly, so the answer cannot depend on creation order or
    /// filesystem timestamp granularity, and each file carries a distinct
    /// percent, so a naive first-file implementation fails on the assertion.
    #[test]
    fn case_06_newest_rollout_selection_over_dated_subdirectories() {
        let scratch = test_support::StateDir::new();
        let home = scratch.path().join("codex-home");
        let sessions = home.join("sessions");
        let pin_mtime = |path: &std::path::Path, seconds: u64| {
            test_support::scratch_files::pin_mtime(path, seconds);
        };
        let write_rollout = |subdir: &str, used_percent: f64, resets_at: i64| {
            let dir = sessions.join(subdir);
            test_support::scratch_files::create_dir_all(&dir);
            let line = serde_json::json!({
                "timestamp": "2026-09-05T22:08:20.572Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "rate_limits": {
                        "limit_id": "codex",
                        "primary": {
                            "used_percent": used_percent,
                            "window_minutes": 300,
                            "resets_at": resets_at,
                        },
                        "secondary": {
                            "used_percent": 7.0,
                            "window_minutes": 10_080,
                            "resets_at": 1_789_174_263i64,
                        },
                    },
                },
            });
            let path = dir.join("rollout-session.jsonl");
            test_support::scratch_files::write(&path, serde_json::to_string(&line).unwrap());
            path
        };
        write_rollout("2026/07/04", 5.0, 1_783_221_436);
        write_rollout("2026/08/20", 11.0, 1_785_000_000);
        write_rollout("2026/09/05", 42.0, 1_788_650_033);
        // The mtimes, pinned independently of file creation order: the July
        // file is the oldest and the September file the newest.
        pin_mtime(
            &sessions.join("2026/07/04/rollout-session.jsonl"),
            1_783_221_000,
        );
        pin_mtime(
            &sessions.join("2026/08/20/rollout-session.jsonl"),
            1_785_000_000,
        );
        pin_mtime(
            &sessions.join("2026/09/05/rollout-session.jsonl"),
            1_788_646_100,
        );

        let adapter = test_adapter();
        let clock = test_clock();
        let captured = adapter.observe_with_evidence(
            &test_credential(),
            &MeterRequest {
                local_home: Some(home.clone()),
                ..MeterRequest::default()
            },
            &BlockingTransport,
            &clock,
        );
        let reading = expect_measured(captured);

        assert_eq!(reading.windows.len(), 2);
        assert_eq!(reading.windows[0].quota_used().as_ppm().get(), 420_000);
        match reading.windows[0].reset_state() {
            WindowResetState::Known(resets_at) => {
                assert_eq!(resets_at.unix_nanos(), 1_788_650_033_000_000_000)
            }
            other @ (WindowResetState::NotStarted | WindowResetState::Scheduled { .. }) => {
                panic!("the newest window carries a known reset, got {other:?}")
            }
        }
    }

    /// Case 07: a home whose sessions tree exists but holds no rollout is
    /// the no-evidence class - the same outcome an empty provider body gets
    /// - not an error out of the adapter and never a fabricated window.
    #[test]
    fn case_07_home_with_no_rollout_reports_no_evidence() {
        let scratch = test_support::StateDir::new();
        let home = scratch.path().join("codex-home");
        test_support::scratch_files::create_dir_all(&home.join("sessions/2026/09/05"));

        let adapter = test_adapter();
        let clock = test_clock();
        let captured = adapter.observe_with_evidence(
            &test_credential(),
            &MeterRequest {
                local_home: Some(home),
                ..MeterRequest::default()
            },
            &BlockingTransport,
            &clock,
        );
        let (failure, evidence) = expect_unreachable(captured);
        assert_eq!(failure, FailureClass::MalformedBody);
        assert!(
            evidence.is_none(),
            "nothing was read, so nothing is captured"
        );
    }

    /// Case 08: the JWT payload decodes for identity, and the token bytes
    /// never reach the evidence capsule. The token and the email are both
    /// registered as known-sensitive material, so even a provider block that
    /// echoed them would come back redacted; the grep here is the belt that
    /// proves the redaction held on the serialized capsule and the failure
    /// body alike. A credential carrying no JWT at all leaves the identity
    /// absent while the measurement stands on the rollout record alone.
    #[test]
    fn case_08_identity_token_never_reaches_the_capsule() {
        let adapter = test_adapter();
        let transport = FixtureTransport::serving(FIXTURE_VALID_TWO_WINDOWS);
        let captured = observe_with(&adapter, &transport);

        let auth: serde_json::Value =
            serde_json::from_slice(FIXTURE_AUTH).expect("the fixture auth parses");
        let token = auth["tokens"]["id_token"]
            .as_str()
            .expect("the fixture carries an id_token");
        let email = "fixture-account@example.test";

        // The token and the email are both registered as known-sensitive
        // material, so even a provider block that echoed them would come back
        // redacted; the grep here is the belt that proves the redaction held
        // on the serialized capsule and the failure body alike.
        let mut retained = String::new();
        if let Some(evidence) = &captured.evidence {
            retained.push_str(evidence.serialized());
        }
        if let Some(failed) = &captured.failed_body {
            retained.push_str(&String::from_utf8_lossy(failed));
        }
        assert!(
            !retained.contains(token),
            "no byte of the JWT may reach the evidence capsule"
        );
        assert!(
            !retained.contains(email),
            "the account email may not reach the evidence capsule"
        );

        // The identity itself decoded for the reading label, off a token
        // whose signature is a fake.
        let reading = expect_measured(captured);
        let identity = reading.identity.expect("the fixture JWT decodes");
        assert_eq!(identity.email.as_deref(), Some(email));
        assert_eq!(identity.plan.as_deref(), Some("plus"));

        // A credential with no decodable JWT leaves identity None and the
        // measurement standing.
        let no_token_credential =
            CredentialHandle::new("{\"tokens\":{},\"last_refresh\":\"2026-09-05T22:00:00Z\"}");
        let captured = test_adapter().observe_with_evidence(
            &no_token_credential,
            &test_request(),
            &FixtureTransport::serving(FIXTURE_VALID_TWO_WINDOWS),
            &test_clock(),
        );
        let reading = expect_measured(captured);
        assert!(reading.identity.is_none());
        assert_eq!(reading.windows.len(), 2);
    }

    /// The negative that a naive wrong implementation fails: a dispatch that
    /// hands the codex adapter no home directory must refuse the observation
    /// rather than fabricate one, and the missing request parameter is the
    /// missing-required-field class.
    #[test]
    fn observe_without_a_local_home_refuses_the_observation() {
        let captured = test_adapter().observe_with_evidence(
            &test_credential(),
            &MeterRequest::default(),
            &FixtureTransport::serving(FIXTURE_VALID_TWO_WINDOWS),
            &test_clock(),
        );
        let (failure, evidence) = expect_unreachable(captured);
        assert_eq!(failure, FailureClass::MissingRequiredField);
        assert!(evidence.is_none());
    }

    /// The evidence capsule records the file that was read and the instant
    /// the provider wrote it, alongside the quota subtree itself: the
    /// provider-observed source is in the capsule, not in a header it drops.
    #[test]
    fn the_capsule_records_the_source_path_and_mtime() {
        let adapter = test_adapter();
        let transport = FixtureTransport::serving(FIXTURE_VALID_TWO_WINDOWS);
        let captured = observe_with(&adapter, &transport);
        let evidence = captured
            .evidence
            .expect("a measured reading carries evidence");
        let capsule: serde_json::Value =
            serde_json::from_str(evidence.serialized()).expect("the capsule is JSON");

        let source = &capsule["quota_response"]["source"];
        assert_eq!(
            source["path"],
            "/fixture/codex-home/sessions/2026/09/05/rollout-fixture.jsonl"
        );
        assert_eq!(source["mtime_nanos"], 1_788_646_100_000_000_000i64);
    }
}
