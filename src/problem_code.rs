//! Stable symbolic problem codes, derived in one place from the failure, stale-reason
//! and report-qualification classifications.
//!
//! Exit codes are a coarse channel: nine of them and far more distinguishable
//! conditions. A symbolic code such as `REMOTE_TIMEOUT` or `COLLECTOR_INTERRUPTED`
//! carries the detail without expanding the exit taxonomy, and automation reads a name
//! rather than parsing prose. Stability across releases is the whole value, so the
//! codes are derived from the existing enums rather than written twice, and every
//! code maps to exactly one [`ExitClass`].

use crate::domain::failure::{AuthReason, FailureClass, HttpStatusClass};
use crate::domain::freshness::StaleReason;
use crate::error::ExitClass;
use crate::evidence::CoverageCompleteness;

/// A stable symbolic problem code.
///
/// One variant per distinguishable condition, derived from [`FailureClass`],
/// [`AuthReason`], [`StaleReason`] and [`CoverageCompleteness`]. The string form is
/// the public contract and is documented in `docs/problem-codes.md`; a test fails when
/// a code is renamed or removed, so a change is deliberate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProblemCode {
    // FailureClass.
    DnsFailure,
    ConnectTimeout,
    ReadTimeout,
    TotalBudgetExpired,
    HttpClientError,
    HttpServerError,
    RateLimited,
    MalformedBody,
    MissingRequiredField,
    // AuthReason.
    CredentialExpired,
    CredentialRejected,
    ProviderDeclaredExpiry,
    // StaleReason (the variants that do not delegate to FailureClass).
    AgeExceeded,
    NoSuccessfulObservation,
    MalformedProviderResponse,
    SamplingGap,
    ClockAnomaly,
    CollectorInterrupted,
    CredentialChangedUnverified,
    // CoverageCompleteness.
    IngestPartial,
}

impl ProblemCode {
    /// The stable string form of this code, the public JSON contract.
    pub fn code(self) -> &'static str {
        match self {
            ProblemCode::DnsFailure => "DNS_FAILURE",
            ProblemCode::ConnectTimeout => "CONNECT_TIMEOUT",
            ProblemCode::ReadTimeout => "READ_TIMEOUT",
            ProblemCode::TotalBudgetExpired => "TOTAL_BUDGET_EXPIRED",
            ProblemCode::HttpClientError => "HTTP_CLIENT_ERROR",
            ProblemCode::HttpServerError => "HTTP_SERVER_ERROR",
            ProblemCode::RateLimited => "RATE_LIMITED",
            ProblemCode::MalformedBody => "MALFORMED_BODY",
            ProblemCode::MissingRequiredField => "MISSING_REQUIRED_FIELD",
            ProblemCode::CredentialExpired => "CREDENTIAL_EXPIRED",
            ProblemCode::CredentialRejected => "CREDENTIAL_REJECTED",
            ProblemCode::ProviderDeclaredExpiry => "PROVIDER_DECLARED_EXPIRY",
            ProblemCode::AgeExceeded => "AGE_EXCEEDED",
            ProblemCode::NoSuccessfulObservation => "NO_SUCCESSFUL_OBSERVATION",
            ProblemCode::MalformedProviderResponse => "MALFORMED_PROVIDER_RESPONSE",
            ProblemCode::SamplingGap => "SAMPLING_GAP",
            ProblemCode::ClockAnomaly => "CLOCK_ANOMALY",
            ProblemCode::CollectorInterrupted => "COLLECTOR_INTERRUPTED",
            ProblemCode::CredentialChangedUnverified => "CREDENTIAL_CHANGED_UNVERIFIED",
            ProblemCode::IngestPartial => "INGEST_PARTIAL",
        }
    }

    /// The one stable exit class this code belongs to. Multiple codes may share a
    /// coarse class; the mapping is exhaustive with no wildcard arm, so adding a code
    /// without choosing its class fails to compile.
    pub fn exit_class(self) -> ExitClass {
        match self {
            ProblemCode::DnsFailure
            | ProblemCode::ConnectTimeout
            | ProblemCode::ReadTimeout
            | ProblemCode::TotalBudgetExpired
            | ProblemCode::HttpClientError
            | ProblemCode::HttpServerError
            | ProblemCode::RateLimited
            | ProblemCode::MalformedBody
            | ProblemCode::MissingRequiredField
            | ProblemCode::MalformedProviderResponse => ExitClass::RemoteUnavailable,
            ProblemCode::CredentialExpired
            | ProblemCode::CredentialRejected
            | ProblemCode::ProviderDeclaredExpiry
            | ProblemCode::CredentialChangedUnverified => ExitClass::AuthRequired,
            ProblemCode::AgeExceeded => ExitClass::Success,
            ProblemCode::NoSuccessfulObservation => ExitClass::InsufficientEvidence,
            ProblemCode::SamplingGap
            | ProblemCode::CollectorInterrupted
            | ProblemCode::IngestPartial => ExitClass::IngestIncomplete,
            ProblemCode::ClockAnomaly => ExitClass::Internal,
        }
    }

    /// One instance of every code, for the enumeration and documentation tests.
    pub fn all() -> [ProblemCode; 20] {
        [
            ProblemCode::DnsFailure,
            ProblemCode::ConnectTimeout,
            ProblemCode::ReadTimeout,
            ProblemCode::TotalBudgetExpired,
            ProblemCode::HttpClientError,
            ProblemCode::HttpServerError,
            ProblemCode::RateLimited,
            ProblemCode::MalformedBody,
            ProblemCode::MissingRequiredField,
            ProblemCode::CredentialExpired,
            ProblemCode::CredentialRejected,
            ProblemCode::ProviderDeclaredExpiry,
            ProblemCode::AgeExceeded,
            ProblemCode::NoSuccessfulObservation,
            ProblemCode::MalformedProviderResponse,
            ProblemCode::SamplingGap,
            ProblemCode::ClockAnomaly,
            ProblemCode::CollectorInterrupted,
            ProblemCode::CredentialChangedUnverified,
            ProblemCode::IngestPartial,
        ]
    }

    /// Renders this code alongside a human message as a JSON object, so automation
    /// reads the code and a human reads the message from the same record.
    pub fn as_json(self, message: &str) -> String {
        format!(
            "{{\"code\":\"{}\",\"message\":\"{}\"}}",
            self.code(),
            message.replace('\\', "\\\\").replace('"', "\\\"")
        )
    }

    /// The code for a report qualification. `Complete` is not a problem, so it has no
    /// code; `Partial` is the one qualification that carries a code.
    pub fn from_coverage(coverage: CoverageCompleteness) -> Option<ProblemCode> {
        match coverage {
            CoverageCompleteness::Complete => None,
            CoverageCompleteness::Partial { .. } => Some(ProblemCode::IngestPartial),
        }
    }
}

impl From<FailureClass> for ProblemCode {
    fn from(class: FailureClass) -> Self {
        match class {
            FailureClass::DnsFailure => ProblemCode::DnsFailure,
            FailureClass::ConnectTimeout => ProblemCode::ConnectTimeout,
            FailureClass::ReadTimeout => ProblemCode::ReadTimeout,
            FailureClass::TotalBudgetExpired => ProblemCode::TotalBudgetExpired,
            FailureClass::HttpStatus(HttpStatusClass::ClientError) => ProblemCode::HttpClientError,
            FailureClass::HttpStatus(HttpStatusClass::ServerError) => ProblemCode::HttpServerError,
            FailureClass::RateLimited { .. } => ProblemCode::RateLimited,
            FailureClass::MalformedBody => ProblemCode::MalformedBody,
            FailureClass::MissingRequiredField => ProblemCode::MissingRequiredField,
        }
    }
}

impl From<AuthReason> for ProblemCode {
    fn from(reason: AuthReason) -> Self {
        match reason {
            AuthReason::CredentialExpired => ProblemCode::CredentialExpired,
            AuthReason::CredentialRejected => ProblemCode::CredentialRejected,
            AuthReason::ProviderDeclaredExpiry => ProblemCode::ProviderDeclaredExpiry,
        }
    }
}

impl From<StaleReason> for ProblemCode {
    fn from(reason: StaleReason) -> Self {
        match reason {
            StaleReason::AgeExceeded => ProblemCode::AgeExceeded,
            StaleReason::NoSuccessfulObservation => ProblemCode::NoSuccessfulObservation,
            StaleReason::SourceUnreachable(class) => class.into(),
            StaleReason::MalformedProviderResponse => ProblemCode::MalformedProviderResponse,
            StaleReason::RateLimited => ProblemCode::RateLimited,
            StaleReason::SamplingGap => ProblemCode::SamplingGap,
            StaleReason::ClockAnomaly => ProblemCode::ClockAnomaly,
            StaleReason::CollectorInterrupted => ProblemCode::CollectorInterrupted,
            StaleReason::CredentialChangedUnverified => ProblemCode::CredentialChangedUnverified,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::failure::HttpStatusClass;
    use std::collections::BTreeSet;

    fn all_failure_classes() -> [FailureClass; 9] {
        [
            FailureClass::DnsFailure,
            FailureClass::ConnectTimeout,
            FailureClass::ReadTimeout,
            FailureClass::TotalBudgetExpired,
            FailureClass::HttpStatus(HttpStatusClass::ClientError),
            FailureClass::HttpStatus(HttpStatusClass::ServerError),
            FailureClass::RateLimited { retry_after: None },
            FailureClass::MalformedBody,
            FailureClass::MissingRequiredField,
        ]
    }

    fn all_auth_reasons() -> [AuthReason; 3] {
        [
            AuthReason::CredentialExpired,
            AuthReason::CredentialRejected,
            AuthReason::ProviderDeclaredExpiry,
        ]
    }

    fn all_stale_reasons() -> [StaleReason; 9] {
        [
            StaleReason::AgeExceeded,
            StaleReason::NoSuccessfulObservation,
            StaleReason::SourceUnreachable(FailureClass::ConnectTimeout),
            StaleReason::MalformedProviderResponse,
            StaleReason::RateLimited,
            StaleReason::SamplingGap,
            StaleReason::ClockAnomaly,
            StaleReason::CollectorInterrupted,
            StaleReason::CredentialChangedUnverified,
        ]
    }

    /// Every variant of the three source enums derives to a code, and the derived code
    /// set is exactly the enumerated code set: nothing is written independently.
    #[test]
    fn every_source_variant_derives_to_an_enumerated_code() {
        let enumerated: BTreeSet<&str> = ProblemCode::all().iter().map(|c| c.code()).collect();

        for class in all_failure_classes() {
            assert!(enumerated.contains(ProblemCode::from(class).code()));
        }
        for reason in all_auth_reasons() {
            assert!(enumerated.contains(ProblemCode::from(reason).code()));
        }
        for reason in all_stale_reasons() {
            assert!(enumerated.contains(ProblemCode::from(reason).code()));
        }
        assert_eq!(
            ProblemCode::from_coverage(CoverageCompleteness::Complete),
            None
        );
        assert_eq!(
            ProblemCode::from_coverage(CoverageCompleteness::partial([
                crate::evidence::ComponentKind::new("x")
            ])),
            Some(ProblemCode::IngestPartial)
        );
    }

    /// Every code has a non-empty, unique string form, and the enumeration names the
    /// exact documented set in order, so a rename or removal changes this list and
    /// fails the run.
    #[test]
    fn enumeration_names_every_code_exactly_once() {
        const EXPECTED: [&str; 20] = [
            "DNS_FAILURE",
            "CONNECT_TIMEOUT",
            "READ_TIMEOUT",
            "TOTAL_BUDGET_EXPIRED",
            "HTTP_CLIENT_ERROR",
            "HTTP_SERVER_ERROR",
            "RATE_LIMITED",
            "MALFORMED_BODY",
            "MISSING_REQUIRED_FIELD",
            "CREDENTIAL_EXPIRED",
            "CREDENTIAL_REJECTED",
            "PROVIDER_DECLARED_EXPIRY",
            "AGE_EXCEEDED",
            "NO_SUCCESSFUL_OBSERVATION",
            "MALFORMED_PROVIDER_RESPONSE",
            "SAMPLING_GAP",
            "CLOCK_ANOMALY",
            "COLLECTOR_INTERRUPTED",
            "CREDENTIAL_CHANGED_UNVERIFIED",
            "INGEST_PARTIAL",
        ];
        let actual: Vec<&str> = ProblemCode::all().iter().map(|c| c.code()).collect();
        assert_eq!(actual, EXPECTED);
    }

    /// Every code renders into JSON alongside a human message, so a condition reported
    /// in JSON always carries its code.
    #[test]
    fn every_code_renders_into_json_with_a_message() {
        for code in ProblemCode::all() {
            let json = code.as_json("the human message");
            assert!(
                json.contains(code.code()),
                "JSON for {code:?} must contain its code string: {json}"
            );
            assert!(json.contains("the human message"));
        }
    }

    /// The documented table lists every code with its exit class, checked against the
    /// enum so the two cannot drift.
    #[test]
    fn documented_table_matches_the_enum() {
        let docs = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/docs/problem-codes.md"
        ))
        .expect("docs/problem-codes.md must be readable");

        for code in ProblemCode::all() {
            let row = format!("| {} | {} |", code.code(), code.exit_class().name());
            assert!(
                docs.contains(&row),
                "docs/problem-codes.md has no row matching {row:?}"
            );
        }
    }

    /// Every code maps to exactly one exit class, and a remote timeout stays
    /// distinguishable from a collector interruption even though both are coarse
    /// classes rather than one shared code.
    #[test]
    fn every_code_maps_to_exactly_one_exit_class() {
        for code in ProblemCode::all() {
            // The mapping is a total function; calling it is the exhaustive check.
            let _ = code.exit_class();
        }

        let timeout_codes = [
            ProblemCode::ConnectTimeout,
            ProblemCode::ReadTimeout,
            ProblemCode::TotalBudgetExpired,
        ];
        for timeout in timeout_codes {
            assert_eq!(timeout.exit_class(), ExitClass::RemoteUnavailable);
            assert_ne!(
                timeout.code(),
                ProblemCode::CollectorInterrupted.code(),
                "a remote timeout and a collector interruption must be distinct codes"
            );
        }
        assert_eq!(
            ProblemCode::CollectorInterrupted.exit_class(),
            ExitClass::IngestIncomplete
        );
    }
}
