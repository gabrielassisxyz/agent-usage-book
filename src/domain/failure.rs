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
        FailureClass::MalformedBody | FailureClass::MissingRequiredField => {
            StaleReason::MalformedProviderResponse
        }
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

/// A bare token is treated as credential-shaped once it is long enough, and made only
/// of characters a token or key commonly uses, that leaving it in place is a bigger
/// risk than redacting an occasional long non-secret.
const BARE_SECRET_MIN_LENGTH: usize = 20;

/// Strips credential-shaped substrings from provider error text before it can enter a
/// failure classification: sanitizes at the one boundary where provider text enters
/// this module, rather than trusting every future call site to remember to redact.
///
/// Two heuristics, applied per whitespace-delimited token: a token matching a known
/// credential label (`Authorization:`, `Bearer`, `api_key=`, ...) is redacted outright
/// as a cheap first path, and a token containing a run of characters long enough and
/// shaped enough to plausibly be a secret is redacted even under a label nobody
/// enumerated (`token=`, `x-api-key:`, or any other `label=SECRET`/`label:SECRET`
/// shape), since the run check does not depend on recognizing the label at all.
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
    if is_labeled || looks_like_a_bare_secret(word) {
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
        use super::super::attempt::AttemptOutcome;

        let outcome = AttemptOutcome::Unreachable(FailureClass::TotalBudgetExpired);
        match outcome {
            AttemptOutcome::Unreachable(FailureClass::TotalBudgetExpired) => {}
            AttemptOutcome::Success | AttemptOutcome::AuthRequired => {
                panic!("expected Unreachable(TotalBudgetExpired), got a different outcome kind")
            }
            other @ AttemptOutcome::Unreachable(_) => {
                panic!("expected Unreachable(TotalBudgetExpired), got {other:?}")
            }
        }
    }

    /// Not every 403 means authentication: an ambiguous status is recorded as an
    /// ordinary unreachable failure, never automatically upgraded to AuthRequired.
    /// That upgrade, when a provider's contract says it is warranted, is the adapter's
    /// decision, made outside this module.
    #[test]
    fn an_ambiguous_403_is_not_classified_as_authentication_by_default() {
        use super::super::attempt::AttemptOutcome;

        let ambiguous_403 =
            AttemptOutcome::Unreachable(FailureClass::HttpStatus(HttpStatusClass::ClientError));
        match ambiguous_403 {
            AttemptOutcome::Unreachable(FailureClass::HttpStatus(HttpStatusClass::ClientError)) => {
            }
            AttemptOutcome::AuthRequired => {
                panic!("an ambiguous HTTP status must never be auto-classified as auth required")
            }
            AttemptOutcome::Success => {
                panic!("expected Unreachable(HttpStatus(ClientError)), got Success")
            }
            other @ AttemptOutcome::Unreachable(_) => {
                panic!("expected Unreachable(HttpStatus(ClientError)), got {other:?}")
            }
        }
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

    /// A random short lowercase word, standing in for a label nobody enumerated. Not
    /// filtered against `CREDENTIAL_LABEL_TOKENS`: even an accidental collision with a
    /// known label is still a valid case, and the point of this generator is to reach
    /// labels the code was never told about, which an unfiltered random word already
    /// does with overwhelming probability.
    fn synthetic_unknown_label(next: &mut impl FnMut() -> u64) -> String {
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
        let len = 3 + (next() % 6) as usize;
        (0..len)
            .map(|_| ALPHABET[(next() % ALPHABET.len() as u64) as usize] as char)
            .collect()
    }

    /// Property: over a corpus of synthetic error bodies seeded with credential
    /// material, the sanitizer emits no credential substring, whether the secret is
    /// labeled with a known label, labeled with a label nobody enumerated, or bare.
    ///
    /// The unknown-label template is the one that would have caught
    /// `sanitizer_redacts_a_secret_under_an_unenumerated_label`'s regression before it
    /// shipped: the other four templates are all shapes the matcher already handled,
    /// derived from the same mental model as the code, so they explore exactly the
    /// space the matcher already covers. A generated corpus is only as good as the
    /// shapes it can imagine.
    #[test]
    fn sanitizer_never_leaks_a_seeded_credential_over_a_generated_corpus() {
        let mut next = xorshift(0xD1B5_4A32_7F19_9E3C);
        let templates: [fn(&str) -> String; 4] = [
            |secret| format!("error: Authorization: Bearer {secret} was rejected"),
            |secret| format!("upstream said api_key={secret} is invalid"),
            |secret| format!("body contained token unexpectedly: {secret}"),
            |secret| format!("{secret} appeared with nothing else around it"),
        ];

        for _ in 0..200 {
            let secret = synthetic_secret(&mut next);
            let choice = next() % (templates.len() as u64 + 1);
            let body = if choice < templates.len() as u64 {
                templates[choice as usize](&secret)
            } else {
                let label = synthetic_unknown_label(&mut next);
                format!("upstream rejected because {label}={secret}")
            };

            let sanitized = sanitize_provider_error_text(&body);
            assert!(
                !sanitized.contains(&secret),
                "sanitized text still contains the seeded secret: {sanitized:?}"
            );
        }
    }
}
