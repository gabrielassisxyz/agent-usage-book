//! The response shapes the synthetic provider server is programmed to produce.
//!
//! The script is a sequence of [`ScriptedOutcome`] values, one per inbound
//! request the server accepts. The server advances through the script
//! deterministically: the same script in two servers produces the same sequence
//! of network-visible outcomes on every run. Calling past the end of the script
//! is a test-authoring error and panics, mirroring [`crate::meter::synthetic`].
//!
//! May not depend on:
//! - SQLite (rule `03`)
//! - the credential or configuration modules (rule `07`)
//! - the ureq transport driver (rule `12`)
//!
//! The shapes here are deliberately the wire-level outcomes a real provider
//! could produce: a status code, headers, a body. They are not the typed
//! [`crate::meter::synthetic::ScriptedResponse`] the in-process adapter works
//! in; that adapter skips the wire entirely. Anything an HTTP test needs to
//! produce over a real socket goes here.

use std::fmt;

/// One scripted outcome the synthetic server will produce for the next inbound
/// request it accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptedOutcome {
    /// Send a complete, valid HTTP response. The body is sent in one write; the
    /// server then closes the response half of the connection cleanly.
    Success(ScriptedResponseBody),

    /// Send a 401 status (the credential was rejected), with no body. Real
    /// providers differ on whether 401 carries a body; this is the minimal
    /// form a test can rely on.
    Unauthorized401,

    /// Send a 403 status. The synthetic adapter's "ambiguous 403" contract
    /// case proves the adapter classifies this as `ClientError` rather than
    /// `AuthRequired`.
    Forbidden403,

    /// Send a 429 with an optional `Retry-After` header in seconds.
    TooManyRequests429 { retry_after_seconds: Option<u32> },

    /// Send a 500 status. Used to exercise `HttpStatusClass::ServerError` and
    /// the corresponding [`crate::domain::failure::ProblemCode`] mapping.
    InternalServerError500,

    /// Send a body that is not valid JSON. The real adapter's parser must map
    /// this to [`crate::domain::failure::FailureClass::MalformedBody`].
    MalformedJson {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },

    /// Send a JSON body that is missing a field the contract requires. The
    /// adapter's typed reader is expected to map this to
    /// [`crate::domain::failure::FailureClass::MissingRequiredField`].
    MissingRequiredField {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },

    /// Send a JSON body that carries an extra, unknown field. The adapter
    /// must tolerate it: an unknown additional field is not a failure.
    UnknownAdditionalField {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },

    /// Send a stale `provider_observed_at` timestamp in a JSON success body.
    StaleServerTimestamp {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },

    /// Send headers, then stop responding without ever sending the body. The
    /// client's read timeout fires. Maps to
    /// [`crate::domain::failure::FailureClass::ReadTimeout`] (or
    /// `TotalBudgetExpired` if the command budget is shorter).
    HeadersThenStall {
        status: u16,
        headers: Vec<(String, String)>,
    },

    /// Accept the connection, then sit without sending anything at all. The
    /// client's connect timeout is satisfied (the kernel completed the TCP
    /// handshake) but the read timeout fires. Distinguishes from
    /// [`ScriptedOutcome::HeadersThenStall`] by producing no application-level
    /// bytes; a real provider can do this when its event loop is wedged
    /// between accept and first write.
    AcceptThenNeverRespond,

    /// Send the response status line and headers, then part of the body, then
    /// close the socket. The transport receives a partial body and maps the
    /// truncated read to [`crate::domain::failure::FailureClass::MalformedBody`]
    /// (or a read timeout, depending on which side gives up first).
    CloseMidBody {
        status: u16,
        headers: Vec<(String, String)>,
        partial_body: Vec<u8>,
    },
}

impl ScriptedOutcome {
    /// Returns `true` if this outcome is a clean success: a complete, parseable
    /// response that a working adapter should turn into a measured reading.
    pub fn is_clean_success(&self) -> bool {
        matches!(self, ScriptedOutcome::Success(_))
    }

    /// Returns `true` if this outcome is meant to map to
    /// [`crate::domain::failure::FailureClass`] (i.e. the test asserts on the
    /// typed failure vocabulary downstream).
    pub fn is_failure_outcome(&self) -> bool {
        !self.is_clean_success()
    }
}

/// A successful response body the synthetic server will send. The server does
/// not parse JSON; it forwards bytes verbatim. Tests that need a typed reading
/// compose the JSON body using the production [`serde_json`] (in the main
/// package's dev-dependency set) and pass the resulting `Vec<u8>` here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedResponseBody {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl ScriptedResponseBody {
    /// Builds a 200 OK with the given body and `Content-Length: <n>`.
    pub fn json_ok(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body,
        }
    }

    /// Builds a response with an explicit status and body. The caller supplies
    /// the headers it wants; the server adds `Content-Length` if absent.
    pub fn with_status(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body,
        }
    }
}

impl fmt::Display for ScriptedOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScriptedOutcome::Success(_) => f.write_str("Success(200)"),
            ScriptedOutcome::Unauthorized401 => f.write_str("Unauthorized401"),
            ScriptedOutcome::Forbidden403 => f.write_str("Forbidden403"),
            ScriptedOutcome::TooManyRequests429 { .. } => f.write_str("TooManyRequests429"),
            ScriptedOutcome::InternalServerError500 => f.write_str("InternalServerError500"),
            ScriptedOutcome::MalformedJson { status, .. } => {
                write!(f, "MalformedJson(status={status})")
            }
            ScriptedOutcome::MissingRequiredField { status, .. } => {
                write!(f, "MissingRequiredField(status={status})")
            }
            ScriptedOutcome::UnknownAdditionalField { status, .. } => {
                write!(f, "UnknownAdditionalField(status={status})")
            }
            ScriptedOutcome::StaleServerTimestamp { status, .. } => {
                write!(f, "StaleServerTimestamp(status={status})")
            }
            ScriptedOutcome::HeadersThenStall { status, .. } => {
                write!(f, "HeadersThenStall(status={status})")
            }
            ScriptedOutcome::AcceptThenNeverRespond => f.write_str("AcceptThenNeverRespond"),
            ScriptedOutcome::CloseMidBody { status, .. } => {
                write!(f, "CloseMidBody(status={status})")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_ok_sets_content_type_and_length_is_implicit() {
        let body = ScriptedResponseBody::json_ok(b"{}".to_vec());
        assert_eq!(body.status, 200);
        assert_eq!(body.body, b"{}");
        let content_type = body
            .headers
            .iter()
            .find(|(k, _)| k == "Content-Type")
            .map(|(_, v)| v.as_str());
        assert_eq!(content_type, Some("application/json"));
    }

    #[test]
    fn clean_success_is_a_clean_success() {
        assert!(ScriptedOutcome::Success(ScriptedResponseBody::json_ok(b"{}".to_vec()))
            .is_clean_success());
    }

    #[test]
    fn non_success_outcomes_are_failure_outcomes() {
        let outcomes = [
            ScriptedOutcome::Unauthorized401,
            ScriptedOutcome::Forbidden403,
            ScriptedOutcome::TooManyRequests429 { retry_after_seconds: None },
            ScriptedOutcome::InternalServerError500,
            ScriptedOutcome::AcceptThenNeverRespond,
        ];
        for o in outcomes {
            assert!(o.is_failure_outcome(), "{o} should be a failure outcome");
        }
    }

    #[test]
    fn display_includes_status_for_status_bearing_variants() {
        let s = ScriptedOutcome::MalformedJson {
            status: 200,
            headers: Vec::new(),
            body: b"not json".to_vec(),
        };
        assert_eq!(format!("{s}"), "MalformedJson(status=200)");
    }
}
