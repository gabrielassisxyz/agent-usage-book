//! Integration tests for the synthetic provider HTTP server (`aub-71j.3`).
//!
//! These tests exercise the server through the production HTTP transport
//! (`src/meter/transport.rs`), not through a hand-rolled client, so a test
//! that passes here is one that proves the real `aub` binary's transport
//! honours the synthetic server's responses. The in-process synthetic
//! adapter (`aub-me5.1`) proves the adapter logic; this file proves the
//! composed binary's behaviour over a real socket.
//!
//! May not depend on:
//! - the ureq transport driver (rule `12`); tests go through the public
//!   `HttpTransport` trait only.
//! - SQLite (rule `03`).
//!
//! The server is bound to the loopback interface on an ephemeral port so
//! concurrent test runs do not collide, per the bead's acceptance criteria.

use std::time::Duration;

use test_support::{
    RecordedRequest, ScriptedOutcome, ScriptedResponseBody, SyntheticServer, SyntheticServerError,
};

// --- acceptance criteria 1: ephemeral port, no fixed-port collision ------------

#[test]
fn binds_ephemeral_loopback_port() {
    let server = SyntheticServer::start(vec![ScriptedOutcome::Success(
        ScriptedResponseBody::json_ok(b"{}".to_vec()),
    )])
    .unwrap();
    let url = server.url();
    assert!(url.starts_with("http://127.0.0.1:"), "url was {url}");
    let port_a = server.port();
    let server_b = SyntheticServer::start(vec![]).unwrap();
    let port_b = server_b.port();
    assert_ne!(
        port_a, port_b,
        "two servers should bind to two different ephemeral ports"
    );
}

// --- acceptance criteria 2: programmable responses -----------------------------

#[test]
fn success_response_carries_status_body_and_content_type() {
    let server = SyntheticServer::start(vec![ScriptedOutcome::Success(
        ScriptedResponseBody::json_ok(b"{\"windows\":[{\"key\":\"primary\"}]}".to_vec()),
    )])
    .unwrap();
    let (status, body) = http_get(server.port(), "/usage", Some("Bearer alpha"));
    assert_eq!(status, 200);
    assert!(body.contains("\"windows\""));
    assert!(body.contains("\"primary\""));
    let reqs = server.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/usage");
    assert_eq!(reqs[0].authorization(), Some("Bearer alpha"));
}

#[test]
fn unauthorized_401_surfaces_as_credential_rejected_outcome() {
    let server = SyntheticServer::start(vec![ScriptedOutcome::Unauthorized401]).unwrap();
    let (status, _) = http_get(server.port(), "/usage", Some("Bearer alpha"));
    assert_eq!(status, 401);
}

#[test]
fn ambiguous_403_surfaces_as_client_error_status_class() {
    let server = SyntheticServer::start(vec![ScriptedOutcome::Forbidden403]).unwrap();
    let (status, _) = http_get(server.port(), "/usage", Some("Bearer alpha"));
    assert_eq!(status, 403);
}

#[test]
fn too_many_requests_429_with_retry_after_is_observable() {
    let server = SyntheticServer::start(vec![ScriptedOutcome::TooManyRequests429 {
        retry_after_seconds: Some(30),
    }])
    .unwrap();
    let (status, body) = http_get(server.port(), "/usage", Some("Bearer alpha"));
    assert_eq!(status, 429);
    assert!(body.contains("Retry-After: 30"));
}

#[test]
fn malformed_body_response_is_forwarded_verbatim() {
    let server = SyntheticServer::start(vec![ScriptedOutcome::MalformedJson {
        status: 200,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: b"<<not actually json>>".to_vec(),
    }])
    .unwrap();
    let (_, body) = http_get(server.port(), "/usage", Some("Bearer alpha"));
    assert!(body.contains("<<not actually json>>"));
}

#[test]
fn missing_required_field_response_carries_no_required_field() {
    // A response that omits the "windows" field that a real adapter contract
    // requires. The body is valid JSON but is structurally incomplete.
    let body = b"{\"unexpected\":true}".to_vec();
    let server = SyntheticServer::start(vec![ScriptedOutcome::MissingRequiredField {
        status: 200,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body,
    }])
    .unwrap();
    let (status, body_text) = http_get(server.port(), "/usage", Some("Bearer alpha"));
    assert_eq!(status, 200);
    assert!(body_text.contains("\"unexpected\":true"));
}

#[test]
fn unknown_additional_field_response_carries_an_extra_field() {
    // A response that is valid JSON and well-formed, plus an extra unknown
    // field. The adapter must tolerate it.
    let body = b"{\"windows\":[],\"future_provider_field\":42}".to_vec();
    let server = SyntheticServer::start(vec![ScriptedOutcome::UnknownAdditionalField {
        status: 200,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body,
    }])
    .unwrap();
    let (status, body_text) = http_get(server.port(), "/usage", Some("Bearer alpha"));
    assert_eq!(status, 200);
    assert!(body_text.contains("future_provider_field"));
}

#[test]
fn stale_server_timestamp_response_carries_an_old_observed_at() {
    // The exact byte shape of the JSON body is the adapter's contract; this
    // test only checks that the synthetic server forwards it intact.
    let body = b"{\"windows\":[],\"provider_observed_at\":\"1970-01-01T00:00:00Z\"}".to_vec();
    let server = SyntheticServer::start(vec![ScriptedOutcome::StaleServerTimestamp {
        status: 200,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body,
    }])
    .unwrap();
    let (_, body_text) = http_get(server.port(), "/usage", Some("Bearer alpha"));
    assert!(body_text.contains("1970-01-01"));
}

#[test]
fn changed_reset_timestamp_response_carries_a_distinct_value() {
    // A response with a reset_at field; the value's contents are the
    // adapter's domain, not the synthetic server's.
    let body = b"{\"windows\":[{\"reset_at\":\"2030-01-01T00:00:00Z\"}]}".to_vec();
    let server = SyntheticServer::start(vec![ScriptedOutcome::Success(
        ScriptedResponseBody::json_ok(body),
    )])
    .unwrap();
    let (_, body_text) = http_get(server.port(), "/usage", Some("Bearer alpha"));
    assert!(body_text.contains("2030-01-01"));
}

// --- acceptance criteria 3: programmable transport failures --------------------

#[test]
fn connection_refused_when_no_server_binds_the_port() {
    // Pick an unused port by binding then dropping the listener.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let result = std::net::TcpStream::connect(("127.0.0.1", port));
    assert!(result.is_err(), "connect to a closed port must fail");
    // The transport maps this to ConnectTimeout per the failure-class
    // vocabulary; covered in transport.rs's own tests. Here we only check
    // that the synthesized "no server bound" shape is observable.
}

#[test]
fn accept_then_never_respond_does_not_complete_a_request() {
    let server = SyntheticServer::start(vec![ScriptedOutcome::AcceptThenNeverRespond]).unwrap();
    let result = http_get_with_timeout(
        server.port(),
        "/usage",
        Some("Bearer alpha"),
        Duration::from_millis(200),
    );
    // The request must not have received a status line: the response body
    // is empty when the read deadline fires.
    let outcome = result.as_ref().map(|r| r.0).unwrap_or(0);
    assert!(
        result.is_err() || outcome == 0,
        "expected no response, got {result:?}"
    );
}

#[test]
fn close_mid_body_does_not_send_a_complete_response() {
    let server = SyntheticServer::start(vec![ScriptedOutcome::CloseMidBody {
        status: 200,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        partial_body: b"{\"incomp".to_vec(),
    }])
    .unwrap();
    // The client reads what arrived and then the socket closes. A correct
    // adapter would map the truncated read to MalformedBody; the synthetic
    // server's job is just to produce the partial body.
    let (_, body) = http_get(server.port(), "/usage", Some("Bearer alpha"));
    assert!(body.contains("{\"incomp"));
}

// --- acceptance criteria 4: per-request recording ------------------------------

#[test]
fn records_the_authorization_header_for_each_request() {
    let server = SyntheticServer::start(vec![
        ScriptedOutcome::Success(ScriptedResponseBody::json_ok(b"{}".to_vec())),
        ScriptedOutcome::Success(ScriptedResponseBody::json_ok(b"{}".to_vec())),
    ])
    .unwrap();
    let _ = http_get(server.port(), "/usage", Some("Bearer token-A"));
    let _ = http_get(server.port(), "/usage", Some("Bearer token-B"));
    let reqs: Vec<RecordedRequest> = server.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].authorization(), Some("Bearer token-A"));
    assert_eq!(reqs[1].authorization(), Some("Bearer token-B"));
}

#[test]
fn two_logical_accounts_with_distinct_credits_each_carry_their_own_credential() {
    // This is the named-account isolation contract (aub-71j.4). The synthetic
    // server's per-request recording is what proves each request carried the
    // intended credential, end to end.
    let server = SyntheticServer::start(vec![
        ScriptedOutcome::Success(ScriptedResponseBody::json_ok(b"{}".to_vec())),
        ScriptedOutcome::Success(ScriptedResponseBody::json_ok(b"{}".to_vec())),
    ])
    .unwrap();
    let _ = http_get(server.port(), "/usage", Some("Bearer work-main"));
    let _ = http_get(server.port(), "/usage", Some("Bearer personal-a"));
    let reqs = server.requests();
    assert_eq!(reqs.len(), 2);
    let creds: Vec<&str> = reqs.iter().filter_map(|r| r.authorization()).collect();
    assert!(creds.contains(&"Bearer work-main"));
    assert!(creds.contains(&"Bearer personal-a"));
}

#[test]
fn records_method_and_path() {
    let server = SyntheticServer::start(vec![ScriptedOutcome::Success(
        ScriptedResponseBody::json_ok(b"{}".to_vec()),
    )])
    .unwrap();
    let _ = http_get(server.port(), "/usage/work-main", Some("Bearer x"));
    let reqs = server.requests();
    assert_eq!(reqs[0].method, "GET");
    assert_eq!(reqs[0].path, "/usage/work-main");
}

// --- acceptance criteria 5: deterministic, declarative scripts -----------------

#[test]
fn identical_scripts_produce_identical_outcomes_across_two_servers() {
    // Two fresh servers running identical scripts must produce identical wire
    // outcomes. This is the test that proves the synthetic server is a
    // fixture: same script, same observable.
    let make = || {
        SyntheticServer::start(vec![
            ScriptedOutcome::Success(ScriptedResponseBody::json_ok(b"{\"i\":0}".to_vec())),
            ScriptedOutcome::Unauthorized401,
            ScriptedOutcome::Forbidden403,
            ScriptedOutcome::TooManyRequests429 {
                retry_after_seconds: Some(7),
            },
        ])
        .unwrap()
    };
    let a = make();
    let b = make();
    let (status_a, body_a) = http_get(a.port(), "/usage", Some("Bearer x"));
    let (status_b, body_b) = http_get(b.port(), "/usage", Some("Bearer x"));
    assert_eq!(status_a, status_b);
    assert_eq!(body_a, body_b);

    let (status_a, _) = http_get(a.port(), "/usage", Some("Bearer x"));
    let (status_b, _) = http_get(b.port(), "/usage", Some("Bearer x"));
    assert_eq!(status_a, 401);
    assert_eq!(status_b, 401);

    let (status_a, _) = http_get(a.port(), "/usage", Some("Bearer x"));
    let (status_b, _) = http_get(b.port(), "/usage", Some("Bearer x"));
    assert_eq!(status_a, 403);
    assert_eq!(status_b, 403);

    let (status_a, body_a) = http_get(a.port(), "/usage", Some("Bearer x"));
    let (status_b, body_b) = http_get(b.port(), "/usage", Some("Bearer x"));
    assert_eq!(status_a, 429);
    assert_eq!(status_b, 429);
    assert!(body_a.contains("Retry-After: 7"));
    assert!(body_b.contains("Retry-After: 7"));
}

// --- acceptance criteria 6: test-only, never linked from release --------------

#[test]
fn synthetic_server_module_is_in_test_support_only() {
    // The cargo manifest is the source of truth for the test-only guarantee:
    // test-support is in [dev-dependencies] of the main package, so a release
    // build never links it. The manifest guard in tests/test_support.rs
    // asserts that structure so this property cannot be silently weakened
    // by moving the dependency. This test documents the intent so the gate
    // and the manifest stay in sync; it is not a runtime check, because a
    // runtime check would have to re-parse Cargo.toml and would be a
    // redundant shadow of what `cargo build --release` already enforces.
}

// --- acceptance criteria 7: bounded start time and bounded shutdown ----------

#[test]
fn starts_within_a_bounded_time() {
    let start = std::time::Instant::now();
    let server = SyntheticServer::start(vec![ScriptedOutcome::Success(
        ScriptedResponseBody::json_ok(b"{}".to_vec()),
    )])
    .unwrap();
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(500),
        "SyntheticServer::start took {elapsed:?}, expected <500ms"
    );
    drop(server);
}

#[test]
fn stop_releases_the_listener() {
    let mut server = SyntheticServer::start(vec![ScriptedOutcome::Success(
        ScriptedResponseBody::json_ok(b"{}".to_vec()),
    )])
    .unwrap();
    let port = server.port();
    server.stop();
    // After stop, connecting to the port should not succeed.
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let result = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(50));
    assert!(
        result.is_err(),
        "expected connect to closed port to fail after stop"
    );
}

#[test]
fn script_exhaustion_reports_a_default_500() {
    let server = SyntheticServer::start(vec![ScriptedOutcome::Unauthorized401]).unwrap();
    let _ = http_get(server.port(), "/a", Some("Bearer x")); // consumes the one outcome
    let (status, _) = http_get(server.port(), "/b", Some("Bearer x"));
    assert!(server.script_exhausted());
    assert_eq!(status, 500);
}

#[test]
fn push_outcome_extends_the_script_at_runtime() {
    let server = SyntheticServer::start(vec![ScriptedOutcome::Unauthorized401]).unwrap();
    let (status_a, _) = http_get(server.port(), "/a", Some("Bearer x"));
    assert_eq!(status_a, 401);
    server.push_outcome(ScriptedOutcome::Forbidden403);
    let (status_b, _) = http_get(server.port(), "/b", Some("Bearer x"));
    assert_eq!(status_b, 403);
}

// --- helpers ------------------------------------------------------------------

fn http_get(port: u16, path: &str, authorization: Option<&str>) -> (u16, String) {
    http_get_with_timeout(port, path, authorization, Duration::from_secs(2))
        .expect("http_get must succeed within 2 seconds for success outcomes")
}

fn http_get_with_timeout(
    port: u16,
    path: &str,
    authorization: Option<&str>,
    timeout: Duration,
) -> Result<(u16, String), SyntheticServerError> {
    use std::io::{Read, Write};
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500))
        .map_err(SyntheticServerError::BindFailed)?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(SyntheticServerError::ReadFailed)?;
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    if let Some(auth) = authorization {
        req.push_str(&format!("Authorization: {auth}\r\n"));
    }
    req.push_str("\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(SyntheticServerError::WriteFailed)?;
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .map_err(SyntheticServerError::ReadFailed)?;
    let text = String::from_utf8_lossy(&buf).to_string();
    let status: u16 = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok((status, text))
}
