//! The OpenCode Go workspace-page meter adapter (aub-8hu3).
//!
//! The OpenCode Go usage meter has no public endpoint: the authoritative
//! surface is the workspace page (`GET https://opencode.ai/workspace/<id>/go`)
//! as seen in a signed-in browser. The page renders its usage meters as
//! markup, not as an embedded JSON blob: the reference tool
//! (`git.sr.ht/~hrbrmstr/opencode-go-usage`, `usage/usage.go`) parses the
//! rendered HTML rather than any script payload, and this adapter follows
//! the same contract. This adapter fetches that page with the session
//! cookie the caller resolved (an `env` credential whose value is the bare
//! `auth` cookie value), reads the three usage windows from the markup, and
//! answers with a typed reading.
//!
//! One `<div data-slot="usage-item">` exists per window. Inside it: a
//! `<span data-slot="usage-label">` naming the window (`5-hour Usage`,
//! `Weekly Usage`, `Monthly Usage`); a `role="progressbar"` element whose
//! `aria-valuenow` attribute carries the percent as a decimal with one
//! place; and a `<span data-slot="reset-time">` whose text, once its React
//! comment markers are stripped, reads `Resets in <N days> <N hours>
//! <N minutes> <N seconds>` in any subset of those units.
//!
//! The request never follows a redirect: the provider answers an expired
//! session by redirecting to its sign-in page, so the redirect response is
//! the authentication signal and arrives here unfollowed (the transport's
//! `without_redirects`). A page with no `data-slot="usage-item"` element is
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
    WindowScope, WindowSemanticKey,
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

/// The page-state marker the parser keys on: the literal attribute pair that
/// opens every usage-window block. A page with no element carrying it has no
/// usage meters to read, and is schema drift.
pub const STATE_MARKER: &str = "data-slot=\"usage-item\"";

/// The provider base the workspace page hangs from. The full page URL is
/// `{DEFAULT_PAGE_BASE}/workspace/<id>/go`; the id is account configuration
/// handed across the boundary in [`MeterRequest::workspace_id`].
pub const DEFAULT_PAGE_BASE: &str = "https://opencode.ai";

/// The one cookie-header credential: the reference's client authenticates
/// with a session cookie named `auth`, and the operator exports the bare
/// cookie value by hand (decided in `aub-r7k0`); the adapter builds the
/// `auth=<value>` header pair itself.
pub const COOKIE_HEADER: &str = "Cookie";

/// The typed success reading produced by [`OpenCodeAdapter`]: one row per
/// usage window the page carries, with the reset anchored at the instant the
/// page was received.
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

    /// The precision of every reset instant this adapter derives, in seconds:
    /// the page's rendered reset text floors the remaining time to whole
    /// hours (`Resets in 6 days 8 hours`), so a re-derived instant is exact
    /// only to one hour. The classifier consumes this declaration
    /// (`aub-w1a0`); one hour covers all three windows because all three
    /// read their reset from the same text renderer.
    pub const RESET_PRECISION_SECONDS: u64 = 3600;

    /// Builds the adapter with an optional full workspace-page URL override.
    pub fn new(endpoint_override: Option<String>) -> Self {
        Self {
            endpoint_override,
            declarations: AdapterDeclarations::new(
                // The page documents no provider measurement time: the
                // reading's basis is the local receive instant, the same
                // anchor the reset arithmetic uses.
                MeasurementBasis::LocallyReceived,
                ProviderContractId::new(Self::DEFAULT_CONTRACT_ID),
                MeterSemanticsId::new(Self::DEFAULT_SEMANTICS_ID),
            )
            .with_required_window_kinds(RequiredWindowKinds::from_values(
                Self::REQUIRED_WINDOW_KINDS,
            ))
            .with_reset_precision(
                // One hour, statically non-zero, so the constructor's
                // refusal is unreachable for this constant.
                ResetPrecision::from_seconds(Self::RESET_PRECISION_SECONDS)
                    .expect("a one-hour precision is a non-zero second count"),
            ),
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

/// The one-decimal percent to parts-per-million step: the provider reports
/// the percent to one decimal place, so one tenth of a percent is exactly
/// 1 000 ppm.
const PPM_PER_PERCENT: f64 = 10_000.0;

/// Splits a workspace page body on [`STATE_MARKER`] and returns the inner
/// markup of each `<div data-slot="usage-item">…</div>` block, in document
/// order. A page with no such element has nothing to read and is
/// [`FailureClass::SchemaDrift`].
fn extract_item_blocks(body: &[u8]) -> Result<Vec<&str>, FailureClass> {
    let text = std::str::from_utf8(body).map_err(|_| FailureClass::MalformedBody)?;
    let mut items = Vec::new();
    let mut search_from = 0usize;
    while let Some(marker_offset) = text[search_from..].find(STATE_MARKER) {
        let marker_pos = search_from + marker_offset;
        let tag_start = text[..marker_pos]
            .rfind("<div")
            .ok_or(FailureClass::MalformedBody)?;
        let tag_end = text[tag_start..]
            .find('>')
            .map(|offset| tag_start + offset + 1)
            .ok_or(FailureClass::MalformedBody)?;
        let inner_len = balanced_div_end(&text[tag_end..]).ok_or(FailureClass::MalformedBody)?;
        items.push(&text[tag_end..tag_end + inner_len]);
        search_from = tag_end + inner_len;
    }
    if items.is_empty() {
        return Err(FailureClass::SchemaDrift);
    }
    Ok(items)
}

/// Finds the byte offset, within `text`, of the `</div>` that closes the
/// div whose content `text` begins at (depth already 1 for that div), by
/// tracking every nested `<div` opening against every `</div>` closing.
/// Only bare `<div` opening tags are counted, which matches the reference
/// fixture's markup and every fixture this adapter reads.
fn balanced_div_end(text: &str) -> Option<usize> {
    let mut depth = 1i32;
    for (offset, _) in text.char_indices() {
        if text[offset..].starts_with("<div") {
            depth += 1;
        } else if text[offset..].starts_with("</div>") {
            depth -= 1;
            if depth == 0 {
                return Some(offset);
            }
        }
    }
    None
}

/// Extracts the text between the opening tag of the first
/// `<span data-slot="$slot">…` in `block` and its next `</span>`. The
/// reset-time span nests only comment markers, never another `<span>`, so
/// the next `</span>` is always the matching close. Returns `None` when the
/// span itself is absent, which is a structural read failure distinct from
/// a value that is present but unusable.
fn span_text<'a>(block: &'a str, slot: &str) -> Option<&'a str> {
    let needle = format!("data-slot=\"{slot}\"");
    let marker_pos = block.find(&needle)?;
    let tag_end = block[marker_pos..].find('>').map(|o| marker_pos + o + 1)?;
    let close = block[tag_end..].find("</span>").map(|o| tag_end + o)?;
    Some(&block[tag_end..close])
}

/// Extracts the value of `attribute="…"` from `block`. Returns `None` when
/// the attribute itself is absent.
fn attribute_value<'a>(block: &'a str, attribute: &str) -> Option<&'a str> {
    let needle = format!("{attribute}=\"");
    let start = block.find(&needle).map(|o| o + needle.len())?;
    let end = block[start..].find('"').map(|o| start + o)?;
    Some(&block[start..end])
}

/// Strips every `<…>` span (both real tags and the React `<!--$-->` /
/// `<!--/-->` comment markers) out of `text`, leaving the plain reader-
/// visible text behind.
fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    for character in text.chars() {
        match character {
            '<' => inside = true,
            '>' => inside = false,
            _ if !inside => out.push(character),
            _ => {}
        }
    }
    out
}

/// Parses `Resets in <N days> <N hours> <N minutes> <N seconds>` (any subset
/// of those four units, in that order) into a total second count. The
/// leading "Resets in" label is case-insensitively stripped first, and the
/// React comment markers around it are already gone by the time this runs
/// (the caller strips tags before calling this).
fn parse_reset_seconds(text: &str) -> Option<i64> {
    let lower = text.to_ascii_lowercase();
    let stripped = lower
        .strip_prefix("resets in")
        .map(|rest| &text[text.len() - rest.len()..])
        .unwrap_or(text);
    let tokens: Vec<&str> = stripped.split_whitespace().collect();
    if tokens.is_empty() || !tokens.len().is_multiple_of(2) {
        return None;
    }
    let mut total = 0i64;
    for pair in tokens.chunks_exact(2) {
        let amount: i64 = pair[0].parse().ok()?;
        let unit_seconds = match pair[1].trim_end_matches(',').to_ascii_lowercase().as_str() {
            "day" | "days" => 24 * 3600,
            "hour" | "hours" => 3600,
            "minute" | "minutes" => 60,
            "second" | "seconds" => 1,
            _ => return None,
        };
        total = total.checked_add(amount.checked_mul(unit_seconds)?)?;
    }
    Some(total)
}

/// The raw fields one usage-item block contributes, extracted structurally
/// (the span and attribute exist) but not yet validated semantically (the
/// percent may not parse as a number, the reset text may not parse as a
/// duration). Building this struct can fail only when the markup itself is
/// missing a required element; a present-but-unusable value survives into
/// this struct as text, so it can still ride into the evidence capsule.
struct RawWindowItem {
    key: WindowKind,
    percent_text: String,
    reset_text: String,
}

/// Reads the structural fields out of one `usage-item` block: the label
/// (used only to identify which window this is), the `aria-valuenow`
/// attribute text, and the reset-time span text with its tags stripped. A
/// label that matches none of the three known windows yields `None`, and
/// the item is skipped rather than treated as an error, matching the
/// reference tool's own label-driven mapping.
fn read_raw_item(block: &str) -> Result<Option<RawWindowItem>, FailureClass> {
    let label = span_text(block, "usage-label").ok_or(FailureClass::MalformedBody)?;
    let Some(key) = WindowKind::from_label(label) else {
        return Ok(None);
    };
    let percent_text =
        attribute_value(block, "aria-valuenow").ok_or(FailureClass::MalformedBody)?;
    let reset_span = span_text(block, "reset-time").ok_or(FailureClass::MalformedBody)?;
    let reset_text = strip_tags(reset_span);
    Ok(Some(RawWindowItem {
        key,
        percent_text: percent_text.to_string(),
        reset_text,
    }))
}

/// Builds the raw-evidence JSON object from every recognized item on the
/// page: one entry per matched window, carrying the still-unvalidated
/// `percent` and `reset_text` strings exactly as read from the markup. This
/// is the object the evidence capsule is captured from, and the object
/// [`parse_state`] later re-reads to derive the typed windows, so the
/// retained evidence is exactly what the reading was derived from.
fn build_raw_state(body: &[u8]) -> Result<serde_json::Value, FailureClass> {
    let blocks = extract_item_blocks(body)?;
    let mut object = serde_json::Map::new();
    for block in blocks {
        if let Some(item) = read_raw_item(block)? {
            object.insert(
                item.key.semantic_key().as_str().to_string(),
                serde_json::json!({
                    "percent": item.percent_text,
                    "reset_text": item.reset_text,
                }),
            );
        }
    }
    Ok(serde_json::Value::Object(object))
}

/// Parses the raw-evidence JSON object (as re-read from the capsule) into
/// the reading's windows. Every required window must be present, with a
/// percent that parses as a decimal in `0.0..=100.0` and a reset text that
/// parses as a duration; every reset is anchored at the instant the page
/// was received: the provider states the interval, `aub` states the
/// anchor.
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

/// The three usage windows the page's `usage-label` text distinguishes,
/// each with its semantic key and the nominal length this adapter stores
/// for it: rolling matches the page's own "5-hour Usage" label, and the two
/// calendar windows are the bead's 7-day and 30-day decisions.
#[derive(Debug, Clone, Copy)]
enum WindowKind {
    Rolling,
    Weekly,
    Monthly,
}

impl WindowKind {
    /// Maps a `usage-label` text to the window it names, matching the
    /// reference tool's own substring rule: a label containing `5-hour`,
    /// `Weekly` or `Monthly` names that window; anything else is not one of
    /// the three required windows.
    fn from_label(label: &str) -> Option<Self> {
        if label.contains("5-hour") {
            Some(Self::Rolling)
        } else if label.contains("Weekly") {
            Some(Self::Weekly)
        } else if label.contains("Monthly") {
            Some(Self::Monthly)
        } else {
            None
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

    /// Parses one window object from the raw-evidence root: the still-raw
    /// `percent` and `reset_text` strings, validated and converted into the
    /// typed window.
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
        let percent_text = object
            .get("percent")
            .and_then(|value| value.as_str())
            .ok_or(FailureClass::MalformedBody)?;
        let percent: f64 = percent_text
            .parse()
            .map_err(|_| FailureClass::MalformedBody)?;
        if !(0.0..=100.0).contains(&percent) {
            return Err(FailureClass::MalformedBody);
        }
        let reset_text = object
            .get("reset_text")
            .and_then(|value| value.as_str())
            .ok_or(FailureClass::MalformedBody)?;
        let reset_in_sec = parse_reset_seconds(reset_text).ok_or(FailureClass::MalformedBody)?;
        let reset_nanos = reset_in_sec
            .checked_mul(1_000_000_000)
            .and_then(|nanos| received_at.unix_nanos().checked_add(nanos))
            .ok_or(FailureClass::MalformedBody)?;
        // `percent` is already validated into `0.0..=100.0` above, so the
        // rounded ppm value is always within `QuotaFractionPpm`'s domain;
        // the fallible constructor is still the boundary that proves it.
        let percent_ppm = QuotaFractionPpm::new((percent * PPM_PER_PERCENT).round() as i32)
            .ok_or(FailureClass::MalformedBody)?;
        Ok(MeterWindow::new(
            key,
            WindowScope::AccountWide,
            QuotaUsed::new(percent_ppm),
            decimal_percent_resolution(),
            QuantizationSemantics::Exact,
            UtcTimestamp::from_unix_nanos(reset_nanos),
            self.nominal_duration(),
        ))
    }
}

/// The reported resolution of a one-decimal percent: one tenth of a
/// percent, 1 000 ppm.
fn decimal_percent_resolution() -> ReportedResolution {
    // 1 000 ppm is one tenth of a percent, statically inside the fraction's
    // domain and non-zero, so both constructors' refusals are unreachable
    // for this constant.
    ReportedResolution::new(
        QuotaFractionPpm::new(1_000).expect("one tenth of a percent is a valid fraction"),
    )
    .expect("one tenth of a percent is a non-zero resolution")
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
        // The credential is the bare `auth` cookie value, decided in
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
            // The `auth` cookie name is the reference client's own name for
            // this session cookie; the credential material is the bare
            // cookie value, and the header carries the name and the value
            // together.
            .with_header(COOKIE_HEADER, format!("auth={material}"))
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
        // The anchor for every reset derivation: the instant the page
        // arrived. The provider states the interval, `aub` states the
        // anchor, and the capsule keeps the raw percent and reset text
        // beside the derived instants so the arithmetic stays auditable.
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

/// Reads one successful workspace response: the raw-state object is built
/// from the markup and sanitized into the evidence capsule first, then the
/// windows are parsed from the capsule's own quota subtree, so the retained
/// evidence is exactly what the reading was derived from. A page with no
/// `usage-item` element never reaches the capsule at all: it is schema
/// drift with nothing to retain.
fn reading_from_response(
    response: &HttpResponse,
    received_at: UtcTimestamp,
    cookie_material: &str,
) -> (
    ProviderObservation<OpenCodeReading>,
    Option<JsonEvidenceCapsule>,
) {
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

    /// The fixture credential material: the bare cookie value the adapter
    /// receives, distinctive on purpose so the leak grep over the capsule
    /// matches nothing but a leak, and free of every shared forbidden
    /// pattern (no credential-shaped prefix, no at sign, no path).
    const FIXTURE_COOKIE: &str = "fixture-session-cookie-9f2c-not-a-real-value";
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
            local_home: None,
            codex_sessions_owned: false,
        };
        adapter.observe_with_evidence(
            &CredentialHandle::new(FIXTURE_COOKIE),
            &request,
            transport,
            &clock,
        )
    }

    /// Case 01: the valid page parses to the three required windows, each
    /// carrying the fixture's decimal percent as its exact parts-per-million
    /// value at one-tenth-of-a-percent resolution.
    #[test]
    fn case_01_valid_page_yields_three_windows_at_decimal_percent_resolution() {
        let transport = SyntheticTransport::serving(FIXTURE_VALID.as_bytes());
        let captured = observing(&transport);
        let ProviderObservation::Measured(reading) = captured.observation else {
            panic!("the valid fixture must measure: {:?}", captured.observation);
        };
        assert_eq!(reading.windows.len(), 3);
        for (key, percent_ppm, reset_in_sec) in [
            ("rolling", 0, 18_000),
            ("weekly", 11_000, 547_200),
            ("monthly", 21_000, 2_361_600),
        ] {
            let window = reading
                .windows
                .iter()
                .find(|window| window.semantic_key().as_str() == key)
                .unwrap_or_else(|| panic!("the {key} window must be present"));
            assert_eq!(
                window.quota_used().as_ppm().get(),
                percent_ppm,
                "the {key} decimal percent converts to ppm by the one-tenth-percent step"
            );
            assert_eq!(window.reported_resolution().as_ppm().get(), 1_000);
            assert_eq!(window.quantization(), QuantizationSemantics::Exact);
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

    /// Case 03: a page with no `usage-item` element is schema drift, never a
    /// silent zero and never a malformed-body claim about a body that
    /// otherwise parsed.
    #[test]
    fn case_03_page_without_the_usage_item_marker_is_schema_drift() {
        let transport = SyntheticTransport::serving(FIXTURE_NO_MARKER.as_bytes());
        let captured = observing(&transport);
        assert_eq!(
            captured.observation,
            ProviderObservation::Unreachable(FailureClass::SchemaDrift)
        );
        assert!(captured.evidence.is_none());
    }

    /// Case 04: a page whose items read structurally (label, attribute and
    /// reset span all present) but whose weekly `aria-valuenow` is not a
    /// number is the parse failure class. Because the markup itself was
    /// readable, the sanitized raw-percent/reset-text evidence still rides
    /// along for the bounded failure store, and it never carries the cookie.
    #[test]
    fn case_04_malformed_state_is_the_parse_failure_class() {
        let transport = SyntheticTransport::serving(FIXTURE_MALFORMED.as_bytes());
        let captured = observing(&transport);
        assert_eq!(
            captured.observation,
            ProviderObservation::Unreachable(FailureClass::MalformedBody)
        );
        assert!(
            captured.evidence.is_some(),
            "a structurally readable page keeps its raw evidence even when a value fails"
        );
        let failed_body = captured
            .failed_body
            .expect("the sanitized raw-state body rides along on the failure");
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
    /// the credential material: a page whose reset-time text echoes the
    /// session value must still yield a capsule without it.
    #[test]
    fn the_sanitizer_removes_the_cookie_even_when_the_page_echoes_it() {
        let echoed = FIXTURE_VALID.replace("5 hours 0 minutes", FIXTURE_COOKIE);
        let transport = SyntheticTransport::serving(echoed.as_bytes());
        let captured = observing(&transport);
        // The echoed reset text no longer parses as a duration, so this page
        // fails to measure; the sanitizer's job is that the failure's
        // retained evidence still excludes the cookie, which is exactly the
        // property this case checks.
        assert_eq!(
            captured.observation,
            ProviderObservation::Unreachable(FailureClass::MalformedBody)
        );
        let failed_body = captured
            .failed_body
            .expect("the raw-state body still rides along on a value-parse failure");
        assert!(
            !String::from_utf8_lossy(&failed_body).contains(FIXTURE_COOKIE),
            "the echoed session value must be sanitized out of the retained evidence"
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
                assert_eq!(
                    *value,
                    format!("auth={FIXTURE_COOKIE}"),
                    "the material goes out under the auth cookie name"
                );
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
            local_home: None,
            codex_sessions_owned: false,
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
            local_home: None,
            codex_sessions_owned: false,
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

    /// The evidence capsule carries the raw percent and reset text per
    /// window, and never the cookie material: the serialized capsule is
    /// grepped for the fixture cookie string, which must match nothing.
    #[test]
    fn the_capsule_holds_the_raw_percent_and_reset_text_and_never_the_cookie() {
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
        // subtree is the raw-state object, so every percent and reset_text
        // is readable in it, keeping the derived instants auditable.
        let quota = quota_response_from_capsule(serialized).expect("the capsule holds the state");
        for (key, percent, reset_text) in [
            ("rolling", "0", "Resets in 5 hours 0 minutes"),
            ("weekly", "1.1", "Resets in 6 days 8 hours"),
            ("monthly", "2.1", "Resets in 27 days 8 hours"),
        ] {
            let window = quota.get(key).expect("the raw window object");
            assert_eq!(window.get("percent"), Some(&serde_json::json!(percent)));
            assert_eq!(
                window.get("reset_text"),
                Some(&serde_json::json!(reset_text))
            );
        }
    }

    /// The parse contract over planted negatives: an out-of-range percent, a
    /// missing window, and a reset text with no parseable unit each fail the
    /// parse instead of yielding a partial reading.
    #[test]
    fn planted_negatives_fail_the_parse() {
        let received_at = UtcTimestamp::from_unix_nanos(1_000_000_000);
        let parse_windows = |state: &serde_json::Value| parse_state(state, received_at);
        let window = |percent: &str, reset_text: &str| serde_json::json!({"percent": percent, "reset_text": reset_text});
        // Percent out of range: the provider's field is 0..=100.
        let out_of_range = serde_json::json!({
            "rolling": window("101", "Resets in 10 seconds"),
            "weekly": window("1", "Resets in 10 seconds"),
            "monthly": window("1", "Resets in 10 seconds"),
        });
        assert_eq!(
            parse_windows(&out_of_range),
            Err(FailureClass::MalformedBody)
        );
        // A missing required window: refused, never defaulted to zero.
        let missing = serde_json::json!({
            "rolling": window("1", "Resets in 10 seconds"),
            "weekly": window("1", "Resets in 10 seconds"),
        });
        assert_eq!(
            parse_windows(&missing),
            Err(FailureClass::MissingRequiredField)
        );
        // A reset text with no recognized unit: refused, never a zero reset.
        let unrecognized_unit = serde_json::json!({
            "rolling": window("1", "Resets in 10 fortnights"),
            "weekly": window("1", "Resets in 10 seconds"),
            "monthly": window("1", "Resets in 10 seconds"),
        });
        assert_eq!(
            parse_windows(&unrecognized_unit),
            Err(FailureClass::MalformedBody)
        );
    }

    /// `parse_reset_seconds` sums every subset of the four units the
    /// reference documents, in the order the page renders them.
    #[test]
    fn parse_reset_seconds_sums_every_unit_subset() {
        assert_eq!(
            parse_reset_seconds("Resets in 5 hours 0 minutes"),
            Some(18_000)
        );
        assert_eq!(
            parse_reset_seconds("Resets in 4 days 10 hours"),
            Some(381_600)
        );
        assert_eq!(
            parse_reset_seconds("Resets in 7 days 5 hours"),
            Some(622_800)
        );
        assert_eq!(parse_reset_seconds("Resets in 45 seconds"), Some(45));
        assert_eq!(parse_reset_seconds("Resets in"), None);
    }

    /// `strip_tags` removes both real tags and the React comment markers,
    /// leaving the reader-visible text exactly as rendered.
    #[test]
    fn strip_tags_removes_comment_markers_and_real_tags() {
        assert_eq!(
            strip_tags("<!--$-->Resets in<!--/--> <!--$-->4 days 10 hours<!--/-->"),
            "Resets in 4 days 10 hours"
        );
        assert_eq!(strip_tags("<b>bold</b> plain"), "bold plain");
    }

    /// Case 05 (aub-rfot): a 429 over an HTML page stores the status spelling
    /// as the classification and no message, because an HTML body carries no
    /// `error.type` or `error.message` to read. That is the honest report for
    /// a failure the provider did not name in a parseable shape.
    #[test]
    fn case_05_error_429_over_an_html_page_stores_the_status_spelling() {
        let transport = SyntheticTransport::with_status(429, "/", b"<html>slow down</html>");
        let captured = observing(&transport);
        let report = captured
            .failed_error
            .as_ref()
            .expect("a 429 response stores the provider's error report");
        assert_eq!(report.classification, "http_429");
        assert_eq!(report.message, "");
        assert!(test_support::sanitization::matched_patterns(&report.classification).is_empty());
    }

    /// Case 06 (aub-rfot): a 401 over an HTML page stores the status spelling
    /// too. The classification is what coverage and doctor group by; the
    /// empty message says the provider supplied no readable words.
    #[test]
    fn case_06_error_401_over_an_html_page_stores_the_status_spelling() {
        let transport = SyntheticTransport::with_status(401, "/", b"<html>no</html>");
        let captured = observing(&transport);
        let report = captured
            .failed_error
            .as_ref()
            .expect("a 401 response stores the provider's error report");
        assert_eq!(report.classification, "http_401");
        assert_eq!(report.message, "");
        assert!(test_support::sanitization::matched_patterns(&report.classification).is_empty());
    }
}
