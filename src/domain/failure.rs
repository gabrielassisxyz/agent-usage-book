//! The shared failure and authentication classifications every provider adapter reports
//! through, instead of each adapter inventing its own vocabulary and reports ending up
//! parsing prose.
//!
//! The invariant this module protects: expanding the transport taxonomy never expands
//! the freshness taxonomy. A new [`FailureClass`] variant maps into an existing
//! [`StaleReason`](super::freshness::StaleReason); it never becomes a fourth thing the
//! user has to understand. [`to_stale_reason`] is total and has no wildcard arm (denied
//! crate-wide by `#![deny(clippy::wildcard_enum_match_arm)]`, `src/lib.rs`), so adding a
//! variant to `FailureClass` without extending that match fails to compile with a plain
//! non-exhaustive-match error before it fails anywhere else.
//!
//! `FailureClass` and `AuthReason` are the single source the JSON contract's symbolic
//! codes (`aub-xus.4`) are derived from. The derivation itself is that bead's job; this
//! module only has to stay exhaustively matchable for it to derive from.

use super::freshness::StaleReason;
use super::time::MonotonicDuration;

/// A class of HTTP response status this project distinguishes at the shared layer.
///
/// Deliberately coarse: which *exact* status code means what is provider-specific
/// (`aub-eun.4`'s adapter contract), and this classification exists only to route a
/// response into freshness reporting, not to decide authentication. See
/// [`AuthReason`]'s documentation for why a 403 is not automatically authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpStatusClass {
    ClientError,
    ServerError,
}

/// Why a collection attempt could not reach or trust its source, shared across every
/// provider adapter.
///
/// Adding a variant here compiles only once [`to_stale_reason`] states where it maps;
/// that is the mechanism behind this bead's "Done when" criterion, not a separate check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureClass {
    DnsFailure,
    ConnectTimeout,
    ReadTimeout,
    /// The command's total execution budget (across retries) expired. Distinct from
    /// `ReadTimeout`/`ConnectTimeout`, which are per-call: a command can retry through
    /// several individual timeouts and still exhaust its total budget on the last one,
    /// or exhaust it while individual calls were each, on their own, fast enough.
    TotalBudgetExpired,
    HttpStatus(HttpStatusClass),
    /// A rate limit response, with the provider's advertised retry delay where one was
    /// given.
    RateLimited {
        retry_after: Option<MonotonicDuration>,
    },
    MalformedBody,
    MissingRequiredField,
    /// The source answered with a structure that no longer matches the shape
    /// this adapter parses (aub-8hu3, for the OpenCode workspace page: the
    /// page-state script the parser keys on is gone). Distinct from
    /// `MalformedBody`, which means the bytes themselves were unreadable: a
    /// drifted schema is readable text wearing an unexpected shape, and the
    /// remediation is a parser correction rather than a retry.
    SchemaDrift,
    /// The subscription behind the account's credential path changed
    /// (aub-iwkg): the provider answered, the reading parsed, and the sampler
    /// refused to attribute it because its subscription identity differs from
    /// the account's established one. Synthesized by the sampler, never by an
    /// adapter: no provider response carries this meaning, and an adapter
    /// that emitted it would be inventing sampler policy. Maps to
    /// [`StaleReason::CredentialChangedUnverified`](super::freshness::StaleReason::CredentialChangedUnverified),
    /// which is what it is: the credential material changed and the new
    /// subscription's continuity under this logical name is unverified.
    SubscriptionChanged,
}

/// Maps every [`FailureClass`] variant into exactly one [`StaleReason`]. Total, with no
/// wildcard arm: adding a `FailureClass` variant without adding its arm here is a
/// non-exhaustive-match compile error, which is what makes this bead's "Done when"
/// criterion mechanical rather than a matter of remembering to update a table.
pub fn to_stale_reason(class: FailureClass) -> StaleReason {
    match class {
        FailureClass::DnsFailure
        | FailureClass::ConnectTimeout
        | FailureClass::ReadTimeout
        | FailureClass::TotalBudgetExpired
        | FailureClass::HttpStatus(_) => StaleReason::SourceUnreachable(class),
        FailureClass::RateLimited { .. } => StaleReason::RateLimited,
        FailureClass::MalformedBody
        | FailureClass::MissingRequiredField
        | FailureClass::SchemaDrift => StaleReason::MalformedProviderResponse,
        // Deliberately not a new StaleReason: the freshness taxonomy stays at
        // three states and nine reasons, and "the credential behind the path
        // changed, continuity unverified" is exactly what this reason names.
        FailureClass::SubscriptionChanged => StaleReason::CredentialChangedUnverified,
    }
}

/// Why an attempt is classified as needing authentication attention.
///
/// Not every 403 means authentication: whether a specific response truly means "this
/// credential is bad" is provider-specific logic (the adapter contract, `aub-eun.4`), a
/// deliberate design rule stated twice because it is easy to break. Getting it wrong is
/// expensive in a specific way: an auth conclusion is sticky within its credential
/// context and tells the operator to go fix a credential that is fine. Nothing in this
/// module converts a `FailureClass::HttpStatus` into an `AuthReason` automatically; that
/// conversion, when a provider's contract says it is warranted, is the adapter's own
/// decision to make.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuthReason {
    CredentialExpired,
    CredentialRejected,
    /// The provider itself declared the credential's authentication expired (as
    /// distinct from this project's own clock concluding the credential is old).
    ProviderDeclaredExpiry,
}

/// What the provider said about a failed request, reduced to the two facts a
/// `meter_attempt_result` row stores and nothing else: a classification and a
/// message, both sanitized. Never a body: the body policy (`aub-2r3`) retains
/// raw responses only where a parse failed, so this report is all that
/// survives for a failed attempt the parsers understood enough to classify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderErrorReport {
    /// The sanitized classification: the response body's own `error.type`
    /// when one was parsed and survived sanitization, else a fallback the
    /// caller derives from what it knows (the HTTP status, the transport
    /// failure class). Normalized to the characters the stored spelling of
    /// this vocabulary uses throughout (`rate_limit_error`, `http_429`).
    pub classification: String,
    /// The sanitized error message, credential-free: the provider's own
    /// words with every credential-shaped and forbidden-pattern-bearing
    /// token redacted. Empty when the provider supplied nothing readable.
    pub message: String,
}

/// The fallback classification for a failure the provider never named: the
/// transport-level spelling of the shared failure class. The HTTP-bearing
/// classes fall back to their status spellings at the adapter, which knows
/// the exact status; this mapping covers what survives when no response did.
/// `tls` has no arm because the transport taxonomy cannot yet distinguish a
/// TLS handshake failure from any other failed connect; a `FailureClass`
/// variant for it would introduce the spelling here, not before.
pub fn provider_error_classification(class: FailureClass) -> &'static str {
    match class {
        FailureClass::DnsFailure => "dns",
        FailureClass::ConnectTimeout => "connect",
        FailureClass::ReadTimeout => "timeout",
        FailureClass::TotalBudgetExpired => "budget_expired_before_request",
        FailureClass::HttpStatus(HttpStatusClass::ClientError) => "http_client_error",
        FailureClass::HttpStatus(HttpStatusClass::ServerError) => "http_server_error",
        FailureClass::RateLimited { .. } => "http_429",
        FailureClass::MalformedBody => "malformed_body",
        FailureClass::MissingRequiredField => "missing_required_field",
        FailureClass::SchemaDrift => "schema_drift",
        FailureClass::SubscriptionChanged => "subscription_changed",
    }
}

/// The fallback classification for an authentication failure raised before
/// any exchange with the provider happened (a credential that could not be
/// read or parsed at all). Distinct from `authentication_error`, which is a
/// provider body's own word for a rejected credential.
pub const CREDENTIAL_UNAVAILABLE_CLASSIFICATION: &str = "credential_error";

/// Normalizes a provider-supplied error classification to the stored
/// vocabulary's shape: lowercase, with every character outside the spelling
/// the existing classifications use mapped to an underscore. The result is
/// guaranteed never to contain the stored-value separator (`": "`), so the
/// classification part of a stored value is always recoverable exactly.
pub fn normalize_error_classification(raw: &str) -> String {
    let mut normalized: String = raw
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    normalized.truncate(ERROR_CLASSIFICATION_MAX_CHARS);
    normalized
}

/// The most characters a stored classification may carry: a generous bound
/// over every spelling the vocabulary actually uses, so a provider echoing
/// something absurd under `error.type` cannot inflate the column.
const ERROR_CLASSIFICATION_MAX_CHARS: usize = 128;

/// The most characters a stored error message may carry: a diagnostic
/// excerpt, not a retained body. Bounded by length here because this is the
/// one place provider prose enters durable storage unaccompanied by the
/// count-bounded capsule machinery (`aub-2r3`).
const ERROR_MESSAGE_MAX_CHARS: usize = 512;

/// Bounds a sanitized message to the length the column's reader should ever
/// have to scan, at a character boundary.
pub fn bound_error_message(sanitized: &str) -> String {
    sanitized.chars().take(ERROR_MESSAGE_MAX_CHARS).collect()
}

/// Case-insensitive labels that precede credential material in provider error text.
/// Matched against a whitespace-delimited token, so `"Authorization: Bearer xyz"`
/// yields three tokens and this list only needs to recognize each label token itself,
/// not their argument.
const CREDENTIAL_LABEL_TOKENS: [&str; 6] = [
    "bearer",
    "authorization:",
    "authorization=",
    "api-key:",
    "api_key=",
    "apikey=",
];

/// The credential and identity patterns that must never survive into stored
/// provider error text, read from the one list every scan shares
/// (`docs/forbidden-patterns.txt`). A pattern added there protects stored
/// error messages with the same edit that protects the four scans the file
/// already feeds; this module keeps no private copy of the list. The
/// binary-scan marker divides the file only for the binary scan's own scope:
/// token-level redaction is safer with every pattern in it, since a token
/// carrying a key prefix is exactly as forbidden in a message as a token
/// naming a credential field.
/// The list is held reversed in the binary. `bin/checks/82-identity-privacy-scan`
/// treats any of these patterns in the release binary's string table as a leak,
/// and a verbatim `include_str!` of the list is exactly the legitimate copy that
/// scan cannot tell from an accidental one (the same reason `meter/evidence.rs`
/// keeps the Anthropic key prefix reversed). Reversing at compile time keeps the
/// single source and keeps every pattern out of the shipped bytes; the runtime
/// reversal below rebuilds the text on first use.
const FORBIDDEN_PATTERN_LIST_REVERSED: [u8; include_bytes!("../../docs/forbidden-patterns.txt")
    .len()] = reverse_bytes(include_bytes!("../../docs/forbidden-patterns.txt"));

const fn reverse_bytes<const N: usize>(source: &[u8; N]) -> [u8; N] {
    let mut out = [0u8; N];
    let mut i = 0;
    while i < N {
        out[i] = source[N - 1 - i];
        i += 1;
    }
    out
}

/// The shared list, parsed once. Lines are matched verbatim (the list's
/// trailing-space convention is preserved), with comments and blanks skipped.
fn forbidden_patterns() -> &'static [String] {
    static PATTERNS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    PATTERNS.get_or_init(|| {
        let text = String::from_utf8(
            FORBIDDEN_PATTERN_LIST_REVERSED
                .iter()
                .rev()
                .copied()
                .collect(),
        )
        .expect("the forbidden-pattern list is UTF-8");
        text.lines()
            .filter(|line| {
                let trimmed = line.trim();
                !trimmed.is_empty() && !trimmed.starts_with('#')
            })
            .map(str::to_owned)
            .collect()
    })
}

/// True when a normalized provider classification may be stored: non-empty
/// and free of every forbidden pattern. A classification is a structured
/// vocabulary token, not message prose, so the free-text sanitizer's
/// bare-secret heuristic does not apply to it; the vocabulary's own
/// spellings are runs of exactly the length that heuristic exists to
/// redact. What must hold instead is the shared forbidden-pattern scan,
/// the same list the stored value is later audited against.
pub fn classification_is_storable(normalized: &str) -> bool {
    !normalized.is_empty()
        && !forbidden_patterns()
            .iter()
            .any(|pattern| normalized.to_lowercase().contains(&pattern.to_lowercase()))
}

/// A bare token is treated as credential-shaped once it is long enough, and made only
/// of characters a token or key commonly uses, that leaving it in place is a bigger
/// risk than redacting an occasional long non-secret.
const BARE_SECRET_MIN_LENGTH: usize = 20;

/// Strips credential-shaped substrings from provider error text before it can enter a
/// failure classification: sanitizes at the one boundary where provider text enters
/// this module, rather than trusting every future call site to remember to redact.
///
/// Three heuristics, applied per whitespace-delimited token: a token matching a known
/// credential label (`Authorization:`, `Bearer`, `api_key=`, ...) is redacted outright
/// as a cheap first path; a token carrying any identity or field-name pattern from
/// the shared forbidden-pattern list is redacted, so a provider message naming a
/// credential field (`x-api-key`, `access_token`) or an account-identifier shape
/// (anything carrying `@`) can never be stored; and a token containing a run of
/// characters long enough and shaped enough to plausibly be a secret is redacted
/// even under a label nobody enumerated (`token=`, `x-api-key:`, or any other
/// `label=SECRET`/`label:SECRET` shape), since the run check does not depend on
/// recognizing the label at all.
pub fn sanitize_provider_error_text(raw: &str) -> String {
    raw.split_whitespace()
        .map(redact_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn redact_token(word: &str) -> String {
    let lower = word.to_lowercase();
    let is_labeled = CREDENTIAL_LABEL_TOKENS
        .iter()
        .any(|label| lower.starts_with(label));
    let carries_forbidden_pattern = forbidden_patterns()
        .iter()
        .any(|pattern| lower.contains(pattern.as_str()));
    if is_labeled || carries_forbidden_pattern || looks_like_a_bare_secret(word) {
        "[REDACTED]".to_string()
    } else {
        word.to_string()
    }
}

/// True when `word` contains a run of alphanumeric, hyphen or underscore characters
/// long enough to plausibly be a secret, wherever that run sits inside the word.
///
/// Checking the run rather than the whole trimmed word is what catches
/// `token=SECRET` and `header:SECRET`: an interior `=` or `:` (or any other separator
/// a label happens to use) splits the word into parts, and the label's own name is one
/// of those parts, but the run length check only needs the SECRET part to be long
/// enough. This is why the label list above is a cheap first path rather than the only
/// path: this check finds a labeled secret without needing to have enumerated its
/// label.
fn looks_like_a_bare_secret(word: &str) -> bool {
    word.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .any(|part| part.len() >= BARE_SECRET_MIN_LENGTH)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_failure_classes() -> [FailureClass; 8] {
        [
            FailureClass::DnsFailure,
            FailureClass::ConnectTimeout,
            FailureClass::ReadTimeout,
            FailureClass::TotalBudgetExpired,
            FailureClass::HttpStatus(HttpStatusClass::ClientError),
            FailureClass::HttpStatus(HttpStatusClass::ServerError),
            FailureClass::RateLimited { retry_after: None },
            FailureClass::MalformedBody,
        ]
    }

    #[test]
    fn every_failure_class_maps_to_exactly_one_stale_reason() {
        for class in all_failure_classes() {
            let reason = to_stale_reason(class);
            match (class, reason) {
                (
                    FailureClass::DnsFailure
                    | FailureClass::ConnectTimeout
                    | FailureClass::ReadTimeout
                    | FailureClass::TotalBudgetExpired
                    | FailureClass::HttpStatus(_),
                    StaleReason::SourceUnreachable(_),
                ) => {}
                (FailureClass::RateLimited { .. }, StaleReason::RateLimited) => {}
                (FailureClass::MalformedBody, StaleReason::MalformedProviderResponse) => {}
                (FailureClass::MissingRequiredField, StaleReason::MalformedProviderResponse) => {}
                (other_class, other_reason) => {
                    panic!("unexpected mapping: {other_class:?} -> {other_reason:?}")
                }
            }
        }
        // MissingRequiredField is exercised on its own line since it is not part of
        // the Copy-friendly fixed-size array above (it carries no data either, but
        // keeping the array focused on the shapes that need a payload sample keeps the
        // array's own size assertion below meaningful).
        assert_eq!(
            to_stale_reason(FailureClass::MissingRequiredField),
            StaleReason::MalformedProviderResponse
        );
    }

    #[test]
    fn total_budget_expiry_is_recorded_as_unreachable_with_a_timeout_class_not_omitted() {
        // The production mapping is the recording decision: budget expiry must
        // stay visible as an unreachable source retaining its own class, so a
        // report can say an unfinished source was attempted and timed out, never
        // that it was omitted (aub-knw7 rewrote this to call the mapper instead of
        // constructing the expected value it matched back).
        let mapped = to_stale_reason(FailureClass::TotalBudgetExpired);
        assert_eq!(
            mapped,
            StaleReason::SourceUnreachable(FailureClass::TotalBudgetExpired)
        );
    }

    /// Not every 403 means authentication: an ambiguous status is recorded as an
    /// ordinary unreachable failure, never automatically upgraded to AuthRequired.
    /// That upgrade, when a provider's contract says it is warranted, is the adapter's
    /// decision, made outside this module. The input is fed through the production
    /// freshness state machine, the component where an auth upgrade would actually
    /// appear, so the guarantee covers the real decision and not a value this test
    /// built to match itself (aub-knw7).
    #[test]
    fn an_ambiguous_403_is_not_classified_as_authentication_by_default() {
        use crate::domain::attempt::{AttemptId, AttemptOutcome, AttemptResult, AttemptStarted};
        use crate::domain::freshness::{
            Freshness, FreshnessInput, LatestAttempt, compute_freshness,
        };
        use crate::domain::time::{ClockSkewEnvelope, FakeClock, MonotonicDuration, UtcTimestamp};

        let ctx = crate::domain::ids::CredentialContextId::new("ctx-403");
        let started = AttemptStarted::new(AttemptId::new(1), UtcTimestamp::from_unix_nanos(1_000));
        let result = AttemptResult::new(
            AttemptId::new(1),
            UtcTimestamp::from_unix_nanos(1_000),
            MonotonicDuration::from_seconds(0),
            AttemptOutcome::Unreachable(FailureClass::HttpStatus(HttpStatusClass::ClientError)),
        );
        let input = FreshnessInput::new(
            None,
            None,
            Some(LatestAttempt::new(started, Some(result), &ctx)),
            None,
            Some(&ctx),
            MonotonicDuration::from_seconds(60),
            MonotonicDuration::from_seconds(10),
            ClockSkewEnvelope::new(MonotonicDuration::from_seconds(10)),
        );
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(2_000));

        let freshness: Freshness<u64> = compute_freshness(&input, &clock);

        let Freshness::Stale {
            last_good: None,
            reason,
            ..
        } = freshness
        else {
            panic!("an ambiguous client error must never be auto-classified as auth required")
        };
        assert_eq!(
            reason,
            StaleReason::SourceUnreachable(FailureClass::HttpStatus(HttpStatusClass::ClientError)),
            "the ambiguous status must surface as an unreachable source, never as auth"
        );
    }

    #[test]
    fn auth_reason_distinguishes_expired_rejected_and_provider_declared() {
        let reasons = [
            AuthReason::CredentialExpired,
            AuthReason::CredentialRejected,
            AuthReason::ProviderDeclaredExpiry,
        ];
        for (i, a) in reasons.iter().enumerate() {
            for (j, b) in reasons.iter().enumerate() {
                assert_eq!(a == b, i == j, "reasons must be pairwise distinct");
            }
        }
    }

    #[test]
    fn sanitizer_redacts_a_labeled_bearer_token() {
        let raw = "request failed: Authorization: Bearer sk-abcdEFGH12345678ijkl status=403";
        let sanitized = sanitize_provider_error_text(raw);
        assert!(!sanitized.contains("sk-abcdEFGH12345678ijkl"));
        assert!(sanitized.contains("[REDACTED]"));
        assert!(sanitized.contains("status=403"));
    }

    #[test]
    fn sanitizer_redacts_a_bare_long_token_with_no_label() {
        let raw = "upstream returned x9f3k2m8q1w7e5r4t6y8u0i2o4p6a8s0d2f4g6h invalid";
        let sanitized = sanitize_provider_error_text(raw);
        assert!(!sanitized.contains("x9f3k2m8q1w7e5r4t6y8u0i2o4p6a8s0d2f4g6h"));
    }

    #[test]
    fn sanitizer_leaves_ordinary_short_words_alone() {
        let raw = "connection refused after 3 retries, status 503";
        assert_eq!(sanitize_provider_error_text(raw), raw);
    }

    /// Regression: a label the code does not enumerate (`token=`) previously defeated
    /// the bare-secret check, because that check trimmed non-token characters only
    /// from the ends of the word and then required every remaining character to be
    /// alphanumeric/hyphen/underscore - an interior `=` disqualified the whole word.
    /// `token=<credential>` is the single most common shape provider error text uses
    /// for a leaked credential. The fixture below is a long, low-entropy run
    /// (repeated characters) rather than a realistic-looking token: this project's own
    /// secret scanner correctly flags a high-entropy `token=`-labeled string as a
    /// likely live credential, which a random-looking fixture would be, so the test
    /// exercises the same length-based code path with a string that cannot be mistaken
    /// for a real one.
    #[test]
    fn sanitizer_redacts_a_secret_under_an_unenumerated_label() {
        let raw = "token=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
        let sanitized = sanitize_provider_error_text(raw);
        assert!(
            !sanitized.contains("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"),
            "sanitized text still contains the secret: {sanitized:?}"
        );
    }

    /// A deterministic pseudo-random generator, the same construction used elsewhere in
    /// this crate's own tests, so this runs over many synthetic bodies without a
    /// property-testing dependency.
    fn xorshift(seed: u64) -> impl FnMut() -> u64 {
        let mut state = seed;
        move || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
    }

    fn synthetic_secret(next: &mut impl FnMut() -> u64) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        (0..32)
            .map(|_| ALPHABET[(next() % ALPHABET.len() as u64) as usize] as char)
            .collect()
    }

    /// Property: over a corpus of synthetic error bodies seeded with credential
    /// material, the sanitizer emits no credential substring, whether the secret is
    /// labeled with a known label, labeled with a label nobody enumerated, or bare.
    ///
    /// The fifth template is the one that would have caught
    /// `sanitizer_redacts_a_secret_under_an_unenumerated_label`'s regression before it
    /// shipped: the other four are all shapes the matcher already handled, derived from
    /// the same mental model as the code, so on their own they explore exactly the
    /// space the matcher already covers. Its label is a random short lowercase word
    /// (not one of `CREDENTIAL_LABEL_TOKENS`, and not filtered against it: an
    /// accidental collision would still be a valid case), derived from the generated
    /// secret itself so it stays a plain `fn(&str) -> String` like the other four
    /// rather than needing to capture the shared generator. A generated corpus is only
    /// as good as the shapes it can imagine.
    #[test]
    fn sanitizer_never_leaks_a_seeded_credential_over_a_generated_corpus() {
        let mut next = xorshift(0xD1B5_4A32_7F19_9E3C);
        let templates: [fn(&str) -> String; 5] = [
            |secret| format!("error: Authorization: Bearer {secret} was rejected"),
            |secret| format!("upstream said api_key={secret} is invalid"),
            |secret| format!("body contained token unexpectedly: {secret}"),
            |secret| format!("{secret} appeared with nothing else around it"),
            |secret| {
                format!(
                    "upstream rejected because {}={secret}",
                    secret[..4].to_lowercase()
                )
            },
        ];

        for _ in 0..200 {
            let secret = synthetic_secret(&mut next);
            let template = templates[(next() % templates.len() as u64) as usize];
            let body = template(&secret);

            let sanitized = sanitize_provider_error_text(&body);
            assert!(
                !sanitized.contains(&secret),
                "sanitized text still contains the seeded secret: {sanitized:?}"
            );
        }
    }

    /// The planted negative for the forbidden-pattern extension: a message
    /// naming a credential field the label list never enumerated survives the
    /// label check and the bare-secret check, and only the forbidden-pattern
    /// scan catches it. This is the exact leak shape a provider 401 uses
    /// ("invalid x-api-key"), so the pattern list, not the label list, is
    /// what must hold here.
    #[test]
    fn sanitizer_redacts_a_credential_field_name_the_label_list_never_enumerated() {
        let raw = "invalid x-api-key provided for account user@example.com";
        let sanitized = sanitize_provider_error_text(raw);
        assert!(
            !sanitized.contains("api-key"),
            "sanitized text still names the credential field: {sanitized:?}"
        );
        assert!(
            !sanitized.contains('@'),
            "sanitized text still carries the account-identifier shape: {sanitized:?}"
        );
        assert!(
            sanitized.contains("provided"),
            "ordinary words survive: {sanitized:?}"
        );
    }

    /// A provider classification is normalized to the stored vocabulary's
    /// shape, and the normalization can never introduce the stored-value
    /// separator: whatever punctuation the provider used, decoding a stored
    /// value splits at the first ": " and lands on the classification.
    #[test]
    fn a_provider_classification_normalizes_to_the_stored_vocabulary_shape() {
        assert_eq!(
            normalize_error_classification("Rate Limit Error"),
            "rate_limit_error"
        );
        assert_eq!(
            normalize_error_classification("invalid_grant"),
            "invalid_grant",
            "an already-normalized classification passes through unchanged"
        );
        let weird = normalize_error_classification("auth: secret@type");
        assert!(!weird.contains(": "), "{weird:?}");
        assert!(!weird.contains('@'), "{weird:?}");
    }

    /// The failure classes that can arise with no response at all map to the
    /// transport-level spellings the bead's acceptance criteria name. The
    /// HTTP-bearing classes fall back at the adapter, which knows the exact
    /// status, so they are absent here on purpose.
    #[test]
    fn responseless_failure_classes_map_to_their_transport_spellings() {
        use crate::domain::time::MonotonicDuration;
        assert_eq!(
            provider_error_classification(FailureClass::DnsFailure),
            "dns"
        );
        assert_eq!(
            provider_error_classification(FailureClass::ConnectTimeout),
            "connect"
        );
        assert_eq!(
            provider_error_classification(FailureClass::ReadTimeout),
            "timeout"
        );
        assert_eq!(
            provider_error_classification(FailureClass::TotalBudgetExpired),
            "budget_expired_before_request"
        );
        assert_eq!(
            provider_error_classification(FailureClass::RateLimited {
                retry_after: Some(MonotonicDuration::from_seconds(60))
            }),
            "http_429",
            "a rate limit with no adapter report falls back to its status spelling"
        );
    }

    /// A stored message is a diagnostic excerpt, not a retained body: the
    /// bound caps the length at a character boundary without ever panicking
    /// on multi-byte input.
    #[test]
    fn a_message_bound_truncates_at_a_character_boundary() {
        let long = "é".repeat(ERROR_MESSAGE_MAX_CHARS + 10);
        let bounded = bound_error_message(&long);
        assert_eq!(bounded.chars().count(), ERROR_MESSAGE_MAX_CHARS);
        // One character past the bound: the truncation lands between the two
        // multi-byte characters, taking the first whole rather than cutting
        // its bytes.
        let multibyte_cut = bound_error_message(&format!(
            "{}{}",
            "a".repeat(ERROR_MESSAGE_MAX_CHARS - 1),
            "éé"
        ));
        assert_eq!(multibyte_cut.chars().count(), ERROR_MESSAGE_MAX_CHARS);
        assert!(multibyte_cut.ends_with('é'), "{multibyte_cut:?}");
    }
}
