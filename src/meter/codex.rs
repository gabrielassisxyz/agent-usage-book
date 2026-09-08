//! The Codex provider meter adapter (`aub-cg6k`, `aub-er47`).
//!
//! Two sources, one reading shape. A home that owns its sessions tree keeps
//! the original local source: the newest `rollout-*.jsonl` under the
//! account's Codex home carries the provider's own `rate_limits` block, and
//! the adapter turns it into two account-wide windows, a 5-hour `primary`
//! and a 7-day `secondary`. A home that does not own its sessions tree - a
//! `caam` shallow profile whose `sessions` is a symlink to the shared
//! `~/.codex/sessions` - never opens a rollout: every profile would read the
//! same file and report another account's spend under its own name. It reads
//! the provider's usage endpoint instead
//! (`GET https://chatgpt.com/backend-api/wham/usage`) with the account's own
//! credential, the way the Anthropic adapter reads its usage endpoint, and
//! parses `rate_limit.primary_window` and `rate_limit.secondary_window`
//! into the same two windows.
//!
//! Which source is taken is decided by whether `<codex_home>/sessions` is a
//! real directory, resolved by the caller into
//! [`crate::meter::adapter::MeterRequest::codex_sessions_owned`] - the fact
//! that makes a rollout reading attributable. The rollout bytes cross the
//! [`HttpTransport`] port through the transport request's local-file source
//! and the endpoint bytes through an ordinary GET, so evidence capture and
//! the synthetic transport seam work unchanged on both paths (the decision
//! recorded on the bead: reading the file directly inside the adapter was
//! rejected because it bypasses evidence capture and the synthetic transport
//! at once).
//!
//! The account identity comes from the JWT payload in the same home's
//! `auth.json`, which is this adapter's credential material: `tokens.id_token`
//! carries the email and the plan. **The JWT is not verified.** Its signature
//! is never checked; it is used as a label on the observation, never as an
//! authentication fact, and the token bytes are registered as sensitive
//! material so they can never reach the evidence capsule.
//!
//! The endpoint authenticates with `tokens.access_token` as a bearer token
//! and `tokens.account_id` in the `chatgpt-account-id` header, both read
//! from the same `auth.json`. A 401 or 403 from the endpoint classifies as
//! `AuthRequired`: the endpoint's error shape is unestablished, so no
//! provider-declared-expiry signal is parsed and every auth refusal is a
//! rejection. A 200 without a `rate_limit` object classifies as
//! `SchemaDrift`, never a silent zero.
//!
//! The endpoint capsule carries only the quota-relevant subtree, the same
//! construction the rollout path uses: the response also carries `user_id`,
//! `account_id` and `email` at the top level and a `model_usage` map, and
//! none of those may enter a fixture or a capsule
//! (`docs/forbidden-patterns.txt`). The sanitizer cannot strip values it was
//! never told, so the adapter strips those keys before capture rather than
//! trusting redaction after it.
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
//!   reads crosses the transport port, and the sessions-tree ownership the
//!   source selection turns on arrives resolved in the request for the same
//!   reason

use crate::domain::failure::{AuthReason, FailureClass, HttpStatusClass};
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
    CapturedProviderResponse, JsonEvidenceCapsule, SensitiveResponseMaterial, capture_json_body,
    error_report_for_observation, quota_response_from_capsule,
};
use crate::meter::transport::{
    CommandBudget, HttpRequest, HttpResponse, LOCAL_FILE_MTIME_HEADER, LOCAL_FILE_PATH_HEADER,
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
/// from the account's newest local rollout when the home owns its sessions
/// tree, and from the provider's usage endpoint otherwise, both through the
/// transport port.
pub struct CodexAdapter {
    endpoint_url: String,
    declarations: AdapterDeclarations,
}

impl CodexAdapter {
    /// The provider contract the rollout records' `rate_limits` shape parses
    /// against. Kept as the adapter's declared contract: endpoint readings
    /// carry their own contract id on the reading, and the sampler persists
    /// the reading's id when one is present.
    pub const CONTRACT_ID: &'static str = "openai-codex-rollout-rate-limits-v1";
    /// The provider contract the usage endpoint's `rate_limit` shape parses
    /// against, beside the rollout one. The meter semantics and the window
    /// rows do not change between the two; only this id and
    /// `provider_observed_at` (the response instant on the endpoint path)
    /// tell them apart.
    pub const ENDPOINT_CONTRACT_ID: &'static str = "openai-codex-wham-usage-v1";
    /// The usage endpoint a home without its own sessions tree reads
    /// (aub-er47).
    pub const DEFAULT_ENDPOINT: &'static str = "https://chatgpt.com/backend-api/wham/usage";
    /// The account header the usage endpoint is sent beside the bearer
    /// token, carrying `tokens.account_id` from the account's `auth.json`.
    /// Both headers were sent in the capture the bead records; whether the
    /// endpoint requires the second one is unestablished, so both are sent.
    pub const ACCOUNT_ID_HEADER: &'static str = "chatgpt-account-id";
    /// What a reading from this adapter physically means: a ChatGPT
    /// subscription's usage windows.
    pub const SEMANTICS_ID: &'static str = "openai-chatgpt-subscription-v1";
    /// The window kinds a measured reading must carry. The provider names
    /// them `primary` and `secondary` inside `rate_limits` on the rollout
    /// path and `primary_window` and `secondary_window` inside `rate_limit`
    /// on the endpoint path.
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
        Self::with_endpoint(Self::DEFAULT_ENDPOINT)
    }

    pub fn with_endpoint(endpoint_url: impl Into<String>) -> Self {
        Self {
            endpoint_url: endpoint_url.into(),
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

    pub fn endpoint_url(&self) -> &str {
        &self.endpoint_url
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

/// The endpoint authentication pair from the account's `auth.json`: the
/// bearer token and the account id the usage endpoint wants beside it. Both
/// live under `tokens` (`tokens.access_token`, `tokens.account_id`); a
/// top-level `account_id` is accepted as a fallback spelling, because the
/// file's exact shape is the CLI's, not this adapter's, to fix. Either value
/// missing or empty means the credential cannot authenticate this contract,
/// which is an expired-credential outcome, never a fabricated request.
fn extract_endpoint_credential(
    credential: &CredentialHandle,
) -> Result<(String, String), AuthReason> {
    let auth: serde_json::Value = serde_json::from_str(credential.expose().trim())
        .map_err(|_| AuthReason::CredentialExpired)?;
    let tokens = auth.get("tokens");
    let access_token = tokens
        .and_then(|tokens| tokens.get("access_token"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .ok_or(AuthReason::CredentialExpired)?;
    let account_id = tokens
        .and_then(|tokens| tokens.get("account_id"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| auth.get("account_id").and_then(serde_json::Value::as_str))
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .ok_or(AuthReason::CredentialExpired)?;
    Ok((access_token, account_id))
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
            epoch_seconds_reset_state(seconds).ok_or(FailureClass::MissingRequiredField)?
        }
        // The RFC 3339 spelling the usage-page surface uses.
        Some(serde_json::Value::String(text)) => WindowResetState::Known(
            UtcTimestamp::parse_rfc3339(text).ok_or(FailureClass::MissingRequiredField)?,
        ),
        Some(_) => return Err(FailureClass::MissingRequiredField),
    };

    build_codex_window(kind, used_percent, reset_state, window_minutes * 60)
}

/// One Codex window from its three honest parts: the percent the provider
/// published, the reset the provider reported, and the window length in
/// seconds. Both sources build through here, so the percent step (whole
/// points to parts per million), the resolution and the scope cannot drift
/// apart between the rollout path and the endpoint path.
fn build_codex_window(
    kind: &str,
    used_percent: f64,
    reset_state: WindowResetState,
    window_seconds: u64,
) -> Result<MeterWindow, FailureClass> {
    if !(0.0..=100.0).contains(&used_percent) || !used_percent.is_finite() {
        return Err(FailureClass::MissingRequiredField);
    }
    let ppm = (used_percent * 10_000.0).round() as i32;
    let quota_used =
        QuotaUsed::new(QuotaFractionPpm::new(ppm).ok_or(FailureClass::MissingRequiredField)?);
    if window_seconds == 0 {
        return Err(FailureClass::MissingRequiredField);
    }

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
        NominalWindowDuration::from_nanos(window_seconds * 1_000_000_000),
    ))
}

/// The reset state for an epoch-seconds number, or `None` when the number
/// cannot name an instant: negative, or overflowing nanos.
fn epoch_seconds_reset_state(seconds: &serde_json::Number) -> Option<WindowResetState> {
    let seconds = seconds.as_i64()?;
    let nanos = seconds
        .checked_mul(1_000_000_000)
        .filter(|nanos| *nanos >= 0)?;
    Some(WindowResetState::Known(UtcTimestamp::from_unix_nanos(
        nanos,
    )))
}

/// Parses the usage endpoint's `rate_limit` object into the same two windows
/// the rollout path produces. A 200 whose body carries no `rate_limit`
/// object is [`FailureClass::SchemaDrift`]: the bytes parsed and the
/// contract's object was absent, which is a drifted schema wearing a 200,
/// never a silent zero. A `rate_limit` object whose window is absent, null
/// or unusable is [`FailureClass::MissingRequiredField`], the same refusal
/// the rollout path reports for a missing window.
fn parse_wham_usage(
    rate_limit_owner: &serde_json::Value,
) -> Result<Vec<MeterWindow>, FailureClass> {
    let rate_limit = rate_limit_owner
        .get("rate_limit")
        .and_then(serde_json::Value::as_object)
        .ok_or(FailureClass::SchemaDrift)?;
    let primary = parse_wham_window(
        rate_limit.get("primary_window"),
        "primary",
        CodexAdapter::PRIMARY_DEFAULT_WINDOW_MINUTES,
    )?;
    let secondary = parse_wham_window(
        rate_limit.get("secondary_window"),
        "secondary",
        CodexAdapter::SECONDARY_DEFAULT_WINDOW_MINUTES,
    )?;
    Ok(vec![primary, secondary])
}

fn parse_wham_window(
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

    let window_seconds = match object.get("limit_window_seconds") {
        None | Some(serde_json::Value::Null) => default_window_minutes * 60,
        Some(value) => {
            let seconds = value.as_u64().ok_or(FailureClass::MissingRequiredField)?;
            if seconds == 0 {
                return Err(FailureClass::MissingRequiredField);
            }
            seconds
        }
    };

    let reset_state = match object.get("reset_at") {
        None | Some(serde_json::Value::Null) => WindowResetState::NotStarted,
        // Epoch seconds, the only spelling the endpoint documents.
        Some(serde_json::Value::Number(seconds)) => {
            epoch_seconds_reset_state(seconds).ok_or(FailureClass::MissingRequiredField)?
        }
        Some(_) => return Err(FailureClass::MissingRequiredField),
    };

    build_codex_window(kind, used_percent, reset_state, window_seconds)
}

/// Strips the identity-shaped top-level keys the usage endpoint carries
/// beside the quota object: `user_id`, `account_id` and `email`, and the
/// `model_usage` map. None of those may enter a fixture or a capsule, and
/// the sanitizer cannot strip values it was never told, so the adapter
/// removes the keys from the parsed body before capture rather than trusting
/// redaction after it.
fn strip_wham_identity(body: &mut serde_json::Value) {
    if let Some(object) = body.as_object_mut() {
        for key in ["user_id", "account_id", "email", "model_usage"] {
            object.remove(key);
        }
    }
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

/// Reinterprets a retained endpoint capsule with the current Codex endpoint
/// semantics: the `rate_limit` subtree the capture kept, nothing else.
fn replay_codex_endpoint_capsule(capsule: &str) -> Result<Vec<MeterWindow>, FailureClass> {
    let quota_response = quota_response_from_capsule(capsule).map_err(|message| {
        if message == "capsule does not contain a quota response" {
            FailureClass::MalformedBody
        } else {
            FailureClass::MissingRequiredField
        }
    })?;
    parse_wham_usage(&quota_response)
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
        // The rollout path is taken only for a home that owns its sessions
        // tree. A shared tree's newest rollout carries no account identity,
        // so reading it under any one name attributes another account's
        // spend; those homes read the live endpoint with their own
        // credential instead, and never open a rollout.
        if request.codex_sessions_owned {
            let Some(home) = &request.local_home else {
                // The caller promised an owned tree but named no home: the
                // request parameters it owed are missing, which is a
                // missing-required-field outcome, never a fabricated reading.
                return CapturedProviderResponse::without_response(
                    ProviderObservation::Unreachable(FailureClass::MissingRequiredField),
                );
            };
            return self.observe_rollout(credential, home, transport, clock);
        }
        self.observe_endpoint(credential, transport, clock)
    }
}

impl CodexAdapter {
    /// The rollout path: the newest `rollout-*.jsonl` under the owning
    /// home's sessions directory, through the transport's local-file arm.
    fn observe_rollout(
        &self,
        credential: &CredentialHandle,
        home: &std::path::Path,
        transport: &impl HttpTransport,
        clock: &impl Clock,
    ) -> CapturedProviderResponse<CodexReading> {
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
                    failed_error: None,
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
                    failed_error: None,
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
            // A rollout is a local file, not a provider error response: the
            // parse-shaped failures here have no provider word to store, and
            // the sampler derives their classification from the failure
            // class.
            failed_error: None,
        }
    }

    /// The endpoint path: the provider's usage endpoint read with the
    /// account's own bearer token and account id, for a home whose sessions
    /// tree is shared and whose rollouts are therefore unattributable. When
    /// the endpoint is unreachable the observation is the unreachable class,
    /// never another account's block: there is no rollout fallback here.
    fn observe_endpoint(
        &self,
        credential: &CredentialHandle,
        transport: &impl HttpTransport,
        clock: &impl Clock,
    ) -> CapturedProviderResponse<CodexReading> {
        let (access_token, account_id) = match extract_endpoint_credential(credential) {
            Ok(pair) => pair,
            Err(reason) => {
                return CapturedProviderResponse::without_response(
                    ProviderObservation::AuthRequired(reason),
                );
            }
        };
        let identity = decode_identity(credential);
        // The bearer token, the account id and the whole credential file are
        // registered as known-sensitive material, so even a response that
        // echoed one of them under an innocuous field name comes back
        // redacted; the subtree capture below is the second belt, keeping
        // the identity-shaped keys out of the capsule by construction.
        let sensitive = SensitiveResponseMaterial::new([
            credential.expose(),
            access_token.as_str(),
            account_id.as_str(),
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
        let request = HttpRequest::get(&self.endpoint_url, timeouts)
            .with_header("Authorization", format!("Bearer {access_token}"))
            .with_header(Self::ACCOUNT_ID_HEADER, &account_id)
            .with_header("Accept", "application/json")
            .with_header("User-Agent", "agent-usage-book/0.1.0");
        let budget = CommandBudget::new(MonotonicDuration::from_seconds(30), clock);
        let response = match transport.send(&request, &budget, clock) {
            Ok(response) => response,
            Err(failure) => {
                return CapturedProviderResponse::without_response(
                    ProviderObservation::Unreachable(failure),
                );
            }
        };

        // The response instant is this reading's measurement time: the
        // endpoint names no provider instant of its own, so the instant the
        // bytes arrived is the closest thing to one. It is captured beside
        // the endpoint that answered, the provider-observed source facts the
        // way the rollout path captures its file and mtime.
        let received_at = clock.now();
        let observation = match response.status() {
            200 => self.reading_from_wham_response(&response, &sensitive, received_at, identity),
            // The endpoint's error shape is unestablished, so no
            // provider-declared-expiry signal is parsed: every auth refusal
            // is a rejection. A 403 is an auth conclusion here by the
            // provider's own contract (unlike the Anthropic one, where a 403
            // is an ambiguous client error), which is why the two statuses
            // share this arm deliberately.
            401 | 403 => (
                ProviderObservation::AuthRequired(AuthReason::CredentialRejected),
                Some(capture_json_body(response.body(), &sensitive)),
            ),
            429 => {
                let retry_after = response
                    .header("retry-after")
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .map(MonotonicDuration::from_seconds);
                (
                    ProviderObservation::Unreachable(FailureClass::RateLimited { retry_after }),
                    Some(capture_json_body(response.body(), &sensitive)),
                )
            }
            400..=499 => (
                ProviderObservation::Unreachable(FailureClass::HttpStatus(
                    HttpStatusClass::ClientError,
                )),
                Some(capture_json_body(response.body(), &sensitive)),
            ),
            500..=599 => (
                ProviderObservation::Unreachable(FailureClass::HttpStatus(
                    HttpStatusClass::ServerError,
                )),
                Some(capture_json_body(response.body(), &sensitive)),
            ),
            _ => (
                ProviderObservation::Unreachable(FailureClass::HttpStatus(
                    HttpStatusClass::ClientError,
                )),
                Some(capture_json_body(response.body(), &sensitive)),
            ),
        };
        let (observation, evidence) = observation;
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
            &sensitive,
        );
        CapturedProviderResponse {
            observation,
            evidence,
            failed_body,
            failed_error,
        }
    }

    /// Reads one successful endpoint response: the identity-shaped keys are
    /// stripped first, then the quota-relevant subtree is captured and the
    /// windows are parsed from the capsule's own subtree, so the retained
    /// evidence is exactly what the reading was derived from. A body without
    /// a `rate_limit` object is schema drift with the stripped body retained
    /// for debugging, never a silent zero.
    fn reading_from_wham_response(
        &self,
        response: &HttpResponse,
        sensitive: &SensitiveResponseMaterial,
        received_at: UtcTimestamp,
        identity: Option<CodexIdentity>,
    ) -> (
        ProviderObservation<CodexReading>,
        Option<JsonEvidenceCapsule>,
    ) {
        let Ok(mut raw) = serde_json::from_slice::<serde_json::Value>(response.body()) else {
            // Not JSON at all: the minimal capsule keeps the content hash
            // and nothing else, exactly as an unparseable HTTP body would.
            let evidence = capture_json_body(response.body(), sensitive);
            return (
                ProviderObservation::Unreachable(FailureClass::MalformedBody),
                Some(evidence),
            );
        };
        strip_wham_identity(&mut raw);
        if raw
            .get("rate_limit")
            .and_then(serde_json::Value::as_object)
            .is_none()
        {
            let stripped = serde_json::to_vec(&raw).expect("a JSON value always serializes");
            let evidence = capture_json_body(&stripped, sensitive);
            return (
                ProviderObservation::Unreachable(FailureClass::SchemaDrift),
                Some(evidence),
            );
        }
        // The composed capture body: the quota-relevant subtree plus the
        // endpoint that answered and the instant it did. Capturing this
        // (rather than the raw body) keeps the identity-shaped keys the
        // response carries beside the quota object out of the capsule by
        // construction.
        let capture_body = serde_json::json!({
            "rate_limit": raw.get("rate_limit"),
            "source": {
                "endpoint": self.endpoint_url,
                "received_at_nanos": received_at.unix_nanos(),
            },
        });
        let capture_bytes =
            serde_json::to_vec(&capture_body).expect("a JSON value always serializes");
        let evidence = capture_json_body(&capture_bytes, sensitive);
        match replay_codex_endpoint_capsule(evidence.serialized()) {
            Ok(windows) => (
                ProviderObservation::Measured(CodexReading {
                    windows,
                    provider_observed_at: Some(ProviderObservedAt::new(received_at)),
                    identity,
                    provider_contract_id: ProviderContractId::new(Self::ENDPOINT_CONTRACT_ID),
                }),
                Some(evidence),
            ),
            Err(failure) => (ProviderObservation::Unreachable(failure), Some(evidence)),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::FakeClock;
    use crate::meter::transport::{BlockingTransport, CommandBudget, HttpResponse};
    use test_support::sanitization::matched_patterns;

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
    const FIXTURE_ENDPOINT_VALID: &[u8] =
        include_bytes!("../../tests/fixtures/meter/codex/wham-usage-valid.json");

    /// The synthetic endpoint transport (aub-er47): serves one programmed
    /// HTTP response for the usage-endpoint GET and asserts the request
    /// contract itself: the endpoint URL, the bearer token carried in the
    /// `Authorization` header and in no header beside it, and the account id
    /// in the account header. A request for the local-file arm fails the
    /// test outright, which is how the source-selection tests prove a
    /// shared home never opens a rollout.
    struct EndpointTransport {
        status: u16,
        body: Vec<u8>,
        expected_url: String,
        expected_token: String,
        expected_account_id: String,
    }

    impl EndpointTransport {
        fn serving(status: u16, body: &[u8]) -> Self {
            let auth: serde_json::Value =
                serde_json::from_slice(FIXTURE_AUTH).expect("the fixture auth parses");
            Self {
                status,
                body: body.to_vec(),
                expected_url: CodexAdapter::DEFAULT_ENDPOINT.to_string(),
                expected_token: auth["tokens"]["access_token"]
                    .as_str()
                    .expect("the fixture carries an access_token")
                    .to_string(),
                expected_account_id: auth["tokens"]["account_id"]
                    .as_str()
                    .expect("the fixture carries an account_id")
                    .to_string(),
            }
        }
    }

    impl HttpTransport for EndpointTransport {
        fn send(
            &self,
            request: &HttpRequest,
            _budget: &CommandBudget,
            _clock: &impl Clock,
        ) -> Result<HttpResponse, FailureClass> {
            assert!(
                request.local_file.is_none(),
                "the endpoint path never opens a rollout"
            );
            assert_eq!(request.url, self.expected_url);
            let authorization = request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("Authorization"))
                .map(|(_, value)| value.as_str());
            assert_eq!(
                authorization,
                Some(format!("Bearer {}", self.expected_token).as_str()),
                "the bearer token travels in the Authorization header"
            );
            for (name, value) in &request.headers {
                if !name.eq_ignore_ascii_case("Authorization") {
                    assert!(
                        !value.contains(&self.expected_token),
                        "the bearer token appears in no header other than Authorization, \
                         yet {name} carries it"
                    );
                }
            }
            let account_header = request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(CodexAdapter::ACCOUNT_ID_HEADER))
                .map(|(_, value)| value.as_str());
            assert_eq!(
                account_header,
                Some(self.expected_account_id.as_str()),
                "the account id travels in the account header"
            );
            Ok(HttpResponse {
                status: self.status,
                headers: Vec::new(),
                body: self.body.clone(),
            })
        }
    }

    fn test_adapter() -> CodexAdapter {
        CodexAdapter::new()
    }

    fn test_credential() -> CredentialHandle {
        CredentialHandle::new(String::from_utf8(FIXTURE_AUTH.to_vec()).unwrap())
    }

    fn test_request() -> MeterRequest {
        MeterRequest {
            local_home: Some(std::path::PathBuf::from("/fixture/codex-home")),
            codex_sessions_owned: true,
            ..MeterRequest::default()
        }
    }

    /// The same request for a home that does not own its sessions tree: the
    /// adapter must take the endpoint path and never open a rollout.
    fn test_shared_request() -> MeterRequest {
        MeterRequest {
            local_home: Some(std::path::PathBuf::from("/fixture/shared-codex-home")),
            codex_sessions_owned: false,
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
                codex_sessions_owned: true,
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
                codex_sessions_owned: true,
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
    /// hands the codex adapter no home directory takes the endpoint path,
    /// because a missing home is not an owned tree, and without a usable
    /// endpoint credential there is nothing to authenticate with. The
    /// missing credential material is the expired-credential class, never a
    /// fabricated reading.
    #[test]
    fn observe_without_a_local_home_and_without_a_token_is_an_auth_outcome() {
        let tokenless =
            CredentialHandle::new("{\"tokens\":{},\"last_refresh\":\"2026-09-05T22:00:00Z\"}");
        let captured = test_adapter().observe_with_evidence(
            &tokenless,
            &MeterRequest::default(),
            &FixtureTransport::serving(FIXTURE_VALID_TWO_WINDOWS),
            &test_clock(),
        );
        match captured.observation {
            ProviderObservation::AuthRequired(AuthReason::CredentialExpired) => {}
            ProviderObservation::Measured(_) => {
                panic!("expected AuthRequired(CredentialExpired), got Measured")
            }
            ProviderObservation::AuthRequired(other) => {
                panic!("expected AuthRequired(CredentialExpired), got AuthRequired({other:?})")
            }
            ProviderObservation::Unreachable(failure) => {
                panic!("expected AuthRequired(CredentialExpired), got Unreachable({failure:?})")
            }
        }
        assert!(captured.evidence.is_none());
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

    /// Case 09: the endpoint happy path over the sanitized capture. The two
    /// windows carry the fixture's percents at the provider's integer-percent
    /// resolution with provider-reported resets, the endpoint contract id,
    /// and the response instant as the measurement time. The raw response
    /// carries the identity-shaped keys and they must not survive into the
    /// capsule: no `user_id`, `account_id`, `email` or `model_usage` key, no
    /// bearer token, no account id.
    #[test]
    fn case_09_endpoint_valid_two_windows() {
        let adapter = test_adapter();
        let transport = EndpointTransport::serving(200, FIXTURE_ENDPOINT_VALID);
        let clock = test_clock();
        let captured = adapter.observe_with_evidence(
            &test_credential(),
            &test_shared_request(),
            &transport,
            &clock,
        );
        let evidence = captured
            .evidence
            .as_ref()
            .expect("a measured reading carries evidence")
            .serialized()
            .to_string();
        let reading = expect_measured(captured);

        assert_eq!(reading.windows.len(), 2);
        let primary = &reading.windows[0];
        assert_eq!(primary.semantic_key().as_str(), "primary");
        assert_eq!(*primary.scope(), WindowScope::AccountWide);
        assert_eq!(primary.quota_used().as_ppm().get(), 0);
        assert_eq!(primary.reported_resolution().as_ppm().get(), 10_000);
        assert_eq!(
            primary.nominal_duration().as_nanos(),
            18_000 * 1_000_000_000
        );
        match primary.reset_state() {
            WindowResetState::Known(resets_at) => {
                assert_eq!(resets_at.unix_nanos(), 1_788_814_546_000_000_000)
            }
            other @ (WindowResetState::NotStarted | WindowResetState::Scheduled { .. }) => {
                panic!("primary carries a known reset, got {other:?}")
            }
        }

        let secondary = &reading.windows[1];
        assert_eq!(secondary.semantic_key().as_str(), "secondary");
        assert_eq!(*secondary.scope(), WindowScope::AccountWide);
        assert_eq!(secondary.quota_used().as_ppm().get(), 270_000);
        assert_eq!(
            secondary.nominal_duration().as_nanos(),
            604_800 * 1_000_000_000
        );
        match secondary.reset_state() {
            WindowResetState::Known(resets_at) => {
                assert_eq!(resets_at.unix_nanos(), 1_789_174_263_000_000_000)
            }
            other @ (WindowResetState::NotStarted | WindowResetState::Scheduled { .. }) => {
                panic!("secondary carries a known reset, got {other:?}")
            }
        }

        // The reading's measurement time is the response instant, and the
        // contract is the endpoint one beside the rollout one.
        let observed = reading
            .provider_observed_at
            .expect("the endpoint reading carries the response instant");
        assert_eq!(observed.as_utc().unix_nanos(), 1_788_646_200_000_000_000);
        assert_eq!(
            reading.provider_contract_id.as_str(),
            CodexAdapter::ENDPOINT_CONTRACT_ID
        );
        let identity = reading.identity.expect("the fixture credential decodes");
        assert_eq!(identity.plan.as_deref(), Some("plus"));

        // The capsule keeps the quota subtree and the endpoint source, and
        // none of the identity-shaped keys the raw response carried.
        let capsule: serde_json::Value =
            serde_json::from_str(&evidence).expect("the capsule is JSON");
        let quota = &capsule["quota_response"];
        assert_eq!(
            quota["rate_limit"]["primary_window"]["used_percent"],
            serde_json::json!(0)
        );
        assert_eq!(quota["source"]["endpoint"], CodexAdapter::DEFAULT_ENDPOINT);
        assert_eq!(
            quota["source"]["received_at_nanos"],
            1_788_646_200_000_000_000i64
        );
        for key in ["user_id", "account_id", "email", "model_usage"] {
            assert!(
                quota.get(key).is_none(),
                "the endpoint capsule must not carry {key}"
            );
        }

        let auth: serde_json::Value =
            serde_json::from_slice(FIXTURE_AUTH).expect("the fixture auth parses");
        let token = auth["tokens"]["access_token"].as_str().unwrap();
        let account_id = auth["tokens"]["account_id"].as_str().unwrap();
        assert!(
            !evidence.contains(token),
            "no byte of the bearer token may reach the evidence capsule"
        );
        assert!(
            !evidence.contains(account_id),
            "the account id may not reach the evidence capsule"
        );
        assert!(
            !evidence.contains("fixture-account@example.test"),
            "the account email may not reach the evidence capsule"
        );
    }

    /// Case 10: a 401 and a 403 from the endpoint are both authentication
    /// outcomes, never an unreachable source and never a silent zero. The
    /// 403 is an auth conclusion here by the endpoint's own contract, which
    /// is the deliberate divergence from the Anthropic adapter documented on
    /// the endpoint path.
    #[test]
    fn case_10_endpoint_401_and_403_are_auth_required() {
        for status in [401u16, 403u16] {
            let transport =
                EndpointTransport::serving(status, b"{\"error\":{\"message\":\"invalid token\"}}");
            let captured = test_adapter().observe_with_evidence(
                &test_credential(),
                &test_shared_request(),
                &transport,
                &test_clock(),
            );
            match captured.observation {
                ProviderObservation::AuthRequired(AuthReason::CredentialRejected) => {}
                ProviderObservation::Measured(_) => panic!(
                    "status {status}: expected AuthRequired(CredentialRejected), got Measured"
                ),
                ProviderObservation::AuthRequired(other) => panic!(
                    "status {status}: expected AuthRequired(CredentialRejected), got AuthRequired({other:?})"
                ),
                ProviderObservation::Unreachable(failure) => panic!(
                    "status {status}: expected AuthRequired(CredentialRejected), got Unreachable({failure:?})"
                ),
            }
            assert!(
                captured.evidence.is_some(),
                "status {status}: the refused response is still captured"
            );
        }
    }

    /// Case 11: a 200 without a `rate_limit` object is schema drift, never a
    /// silent zero. The stripped body is still captured for debugging, with
    /// the identity-shaped keys removed before capture.
    #[test]
    fn case_11_endpoint_without_rate_limit_is_schema_drift() {
        for body in [
            b"{\"plan_type\":\"plus\",\"credits\":{\"has_credits\":false}}".as_slice(),
            b"{\"plan_type\":\"plus\",\"rate_limit\":null,\"email\":\"someone@example.test\"}"
                .as_slice(),
        ] {
            let transport = EndpointTransport::serving(200, body);
            let captured = test_adapter().observe_with_evidence(
                &test_credential(),
                &test_shared_request(),
                &transport,
                &test_clock(),
            );
            let (failure, evidence) = expect_unreachable(captured);
            assert_eq!(failure, FailureClass::SchemaDrift);
            let evidence = evidence.expect("a drifted response still captures evidence");
            assert!(
                !evidence.serialized().contains("someone@example.test"),
                "the stripped drift body must not carry the identity value"
            );
        }
    }

    /// The committed endpoint fixture parses clean against the shared
    /// forbidden-pattern list: no credential shape, no home path, no
    /// account identifier survives the sanitization the bead demands.
    #[test]
    fn the_endpoint_fixture_parses_clean_against_the_forbidden_pattern_list() {
        let text = std::str::from_utf8(FIXTURE_ENDPOINT_VALID).expect("the fixture is UTF-8");
        let hits = test_support::sanitization::matched_patterns(text);
        assert!(
            hits.is_empty(),
            "forbidden patterns in the fixture: {hits:?}"
        );
        // The fixture still parses and still carries the quota object the
        // parser needs: clean must not mean gutted.
        let body: serde_json::Value =
            serde_json::from_slice(FIXTURE_ENDPOINT_VALID).expect("the fixture is JSON");
        assert!(body.get("rate_limit").is_some());
    }

    /// The source selection rule over a fixture tree with one real and one
    /// symlinked sessions directory. The shared tree holds a rollout
    /// carrying 99%: if the linked home opened it, the assertion fails. The
    /// ownership answers come from the production helper, so this proves the
    /// decision the binary takes, not a flag the test set by hand.
    #[test]
    fn a_symlinked_sessions_tree_never_opens_a_rollout() {
        let scratch = test_support::StateDir::new();
        let rollout_line = serde_json::json!({
            "timestamp": "2026-09-05T22:08:20.572Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "rate_limits": {
                    "limit_id": "codex",
                    "primary": {"used_percent": 99.0, "window_minutes": 300, "resets_at": 1_788_650_033i64},
                    "secondary": {"used_percent": 99.0, "window_minutes": 10_080, "resets_at": 1_789_174_263i64},
                },
            },
        });
        let shared = scratch.path().join("shared-sessions");
        test_support::scratch_files::create_dir_all(&shared.join("2026/09/05"));
        test_support::scratch_files::write(
            &shared.join("2026/09/05/rollout-shared.jsonl"),
            serde_json::to_string(&rollout_line).unwrap(),
        );

        let linked_home = scratch.path().join("linked-home");
        test_support::scratch_files::create_dir_all(&linked_home);
        test_support::scratch_files::symlink(&shared, &linked_home.join("sessions"));

        let owned_home = scratch.path().join("owned-home");
        test_support::scratch_files::create_dir_all(&owned_home.join("sessions/2026/09/05"));
        test_support::scratch_files::write(
            &owned_home.join("sessions/2026/09/05/rollout-owned.jsonl"),
            serde_json::to_string(&rollout_line).unwrap(),
        );

        assert!(!crate::local_source::codex_home_owns_sessions_tree(
            &linked_home
        ));
        assert!(crate::local_source::codex_home_owns_sessions_tree(
            &owned_home
        ));

        // The linked home reads the endpoint (0% and 27%), never the shared
        // tree's 99% rollout; the endpoint transport fails the test on any
        // local-file request, so passing proves no rollout was opened.
        let adapter = test_adapter();
        let clock = test_clock();
        let linked_owned = crate::local_source::codex_home_owns_sessions_tree(&linked_home);
        let captured = adapter.observe_with_evidence(
            &test_credential(),
            &MeterRequest {
                local_home: Some(linked_home),
                codex_sessions_owned: linked_owned,
                ..MeterRequest::default()
            },
            &EndpointTransport::serving(200, FIXTURE_ENDPOINT_VALID),
            &clock,
        );
        let reading = expect_measured(captured);
        assert_eq!(reading.windows[0].quota_used().as_ppm().get(), 0);
        assert_eq!(reading.windows[1].quota_used().as_ppm().get(), 270_000);

        // The owned home reads its own rollout (99%) through the real disk
        // arm, never the network.
        let owned_owned = crate::local_source::codex_home_owns_sessions_tree(&owned_home);
        let captured = adapter.observe_with_evidence(
            &test_credential(),
            &MeterRequest {
                local_home: Some(owned_home),
                codex_sessions_owned: owned_owned,
                ..MeterRequest::default()
            },
            &BlockingTransport,
            &clock,
        );
        let reading = expect_measured(captured);
        assert_eq!(reading.windows[0].quota_used().as_ppm().get(), 990_000);
        assert_eq!(
            reading.provider_contract_id.as_str(),
            CodexAdapter::CONTRACT_ID
        );
    }

    /// A transport that never answers: the endpoint is unreachable.
    struct UnreachableTransport;

    impl HttpTransport for UnreachableTransport {
        fn send(
            &self,
            _request: &HttpRequest,
            _budget: &CommandBudget,
            _clock: &impl Clock,
        ) -> Result<HttpResponse, FailureClass> {
            Err(FailureClass::ConnectTimeout)
        }
    }

    /// When the endpoint is unreachable for a shared home, the observation
    /// is the unreachable class, not another account's block: there is no
    /// rollout fallback on this path, even with a rollout sitting in the
    /// shared tree.
    #[test]
    fn an_unreachable_endpoint_on_a_shared_tree_is_unreachable_not_another_accounts_block() {
        let scratch = test_support::StateDir::new();
        let shared = scratch.path().join("shared-sessions");
        test_support::scratch_files::create_dir_all(&shared.join("2026/09/05"));
        test_support::scratch_files::write(
            &shared.join("2026/09/05/rollout-shared.jsonl"),
            "{\"payload\":{\"rate_limits\":{\"primary\":{\"used_percent\":99.0},\"secondary\":{\"used_percent\":99.0}}}}",
        );
        let linked_home = scratch.path().join("linked-home");
        test_support::scratch_files::create_dir_all(&linked_home);
        test_support::scratch_files::symlink(&shared, &linked_home.join("sessions"));

        let captured = test_adapter().observe_with_evidence(
            &test_credential(),
            &MeterRequest {
                local_home: Some(linked_home),
                codex_sessions_owned: false,
                ..MeterRequest::default()
            },
            &UnreachableTransport,
            &test_clock(),
        );
        let (failure, evidence) = expect_unreachable(captured);
        assert_eq!(failure, FailureClass::ConnectTimeout);
        assert!(evidence.is_none());
    }

    /// An owned tree with no home named is a missing request parameter, the
    /// same refusal the adapter always reported for a missing home on the
    /// rollout path.
    #[test]
    fn observe_with_an_owned_tree_but_no_home_refuses_the_observation() {
        let captured = test_adapter().observe_with_evidence(
            &test_credential(),
            &MeterRequest {
                local_home: None,
                codex_sessions_owned: true,
                ..MeterRequest::default()
            },
            &FixtureTransport::serving(FIXTURE_VALID_TWO_WINDOWS),
            &test_clock(),
        );
        let (failure, evidence) = expect_unreachable(captured);
        assert_eq!(failure, FailureClass::MissingRequiredField);
        assert!(evidence.is_none());
    }

    /// Case 12 (aub-rfot): a 429 endpoint response whose body names the
    /// provider's own error type stores it as the classification with the
    /// sanitized message beside it, and a 401 stores the authentication
    /// spelling the same way. Neither field of either report matches a
    /// forbidden pattern, and the rollout path attaches no report at all:
    /// a local file supplies no provider words to store.
    #[test]
    fn case_12_endpoint_failures_store_the_provider_s_classification() {
        let rate_limited = EndpointTransport::serving(
            429,
            br#"{"error":{"type":"rate_limit_error","message":"Rate limit exceeded. Please retry later."}}"#.as_slice(),
        );
        let captured = test_adapter().observe_with_evidence(
            &test_credential(),
            &test_shared_request(),
            &rate_limited,
            &test_clock(),
        );
        let report = captured
            .failed_error
            .as_ref()
            .expect("a 429 response stores the provider's error report");
        assert_eq!(report.classification, "rate_limit_error");
        assert_eq!(report.message, "Rate limit exceeded. Please retry later.");
        assert!(matched_patterns(&report.classification).is_empty());
        assert!(matched_patterns(&report.message).is_empty());

        let rejected = EndpointTransport::serving(
            401,
            br#"{"error":{"type":"authentication_error","message":"Invalid authentication token provided."}}"#.as_slice(),
        );
        let captured = test_adapter().observe_with_evidence(
            &test_credential(),
            &test_shared_request(),
            &rejected,
            &test_clock(),
        );
        let report = captured
            .failed_error
            .as_ref()
            .expect("a 401 response stores the provider's error report");
        assert_eq!(report.classification, "authentication_error");
        assert_eq!(report.message, "Invalid authentication token provided.");
        assert!(matched_patterns(&report.classification).is_empty());
        assert!(matched_patterns(&report.message).is_empty());
    }
}
