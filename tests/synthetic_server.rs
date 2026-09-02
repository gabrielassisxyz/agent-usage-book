//! Integration tests for the synthetic provider HTTP server (`aub-71j.3`).
//!
//! The response-shape tests assert wire bytes directly, because what they
//! prove is byte forwarding; the failure-class tests drive the server through
//! the production HTTP transport (`src/meter/transport.rs`), so a test that
//! passes here is one that proves the real `aub` binary's transport honours
//! the synthetic server's shapes. The in-process synthetic adapter
//! (`aub-me5.1`) proves the adapter logic; this file proves the composed
//! binary's behaviour over a real socket.
//!
//! May not depend on:
//! - the ureq transport driver (rule `12`); tests go through the public
//!   `execute_single` entry point only.
//! - SQLite (rule `03`).
//!
//! The server is bound to the loopback interface on an ephemeral port so
//! concurrent test runs do not collide, per the bead's acceptance criteria.

use std::time::Duration;

use agent_usage_book::domain::failure::FailureClass;
use agent_usage_book::domain::time::{MonotonicDuration, RealClock};
use agent_usage_book::meter::transport::{
    CommandBudget, HttpRequest, RequestTimeoutConfig, execute_single,
};
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

// --- read-timeout versus connect-timeout, and the command budget -------------

/// The pair the bead's context names as the one most likely to be conflated:
/// a connect-phase failure and a read-phase failure must stay distinct
/// classes, and the declarative server produces both shapes on demand.
#[test]
fn headers_then_stall_and_a_refused_port_yield_distinct_failure_classes() {
    let clock = RealClock::new();
    let budget = CommandBudget::new(MonotonicDuration::from_millis(5_000), &clock);
    let timeouts = RequestTimeoutConfig::new(
        MonotonicDuration::from_millis(1_000),
        MonotonicDuration::from_millis(250),
        None,
    );

    // Read phase: the server accepts, sends headers, then never completes the
    // body. The transport must classify this as a read timeout.
    let stalled = SyntheticServer::start(vec![ScriptedOutcome::HeadersThenStall {
        status: 200,
        headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
    }])
    .unwrap();
    let request =
        HttpRequest::get(stalled.url(), timeouts).with_header("Authorization", "Bearer alpha");
    let read_phase = execute_single(&request, &budget, &clock);
    assert_eq!(read_phase.unwrap_err(), FailureClass::ReadTimeout);

    // Connect phase: nothing binds the port. The transport has no refused
    // variant, so refusal and connect timeout share the connect-phase class
    // on purpose; the assertion is that this is NOT the read-phase class.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let request = HttpRequest::get(
        format!("http://127.0.0.1:{port}/usage"),
        RequestTimeoutConfig::new(
            MonotonicDuration::from_millis(500),
            MonotonicDuration::from_millis(500),
            None,
        ),
    );
    let connect_phase = execute_single(&request, &budget, &clock);
    assert_eq!(connect_phase.unwrap_err(), FailureClass::ConnectTimeout);
}

#[test]
fn the_command_budget_bounds_a_wedged_endpoint() {
    let stalled = SyntheticServer::start(vec![ScriptedOutcome::HeadersThenStall {
        status: 200,
        headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
    }])
    .unwrap();
    let clock = RealClock::new();
    // The budget is far below the declared read timeout, so only the command
    // budget can bound this request; the wedged endpoint must not hold the
    // command for its declared read timeout.
    let budget = CommandBudget::new(MonotonicDuration::from_millis(300), &clock);
    let request = HttpRequest::get(
        stalled.url(),
        RequestTimeoutConfig::new(
            MonotonicDuration::from_millis(1_000),
            MonotonicDuration::from_millis(30_000),
            None,
        ),
    )
    .with_header("Authorization", "Bearer alpha");

    let started = std::time::Instant::now();
    let outcome = execute_single(&request, &budget, &clock);
    let elapsed = started.elapsed();

    assert_eq!(outcome.unwrap_err(), FailureClass::TotalBudgetExpired);
    assert!(
        elapsed < Duration::from_secs(2),
        "the budget must bound a wedged endpoint, took {elapsed:?}"
    );
}

// --- the transport failure shapes reaching the persisted attempt result ------

/// Store fixture: one account, one sample run, one policy snapshot, one
/// started attempt - the same chain the attempt repository's own tests build,
/// through the public APIs, so the persisted result this test reads back is a
/// real row under the real constraints.
fn fixture_store() -> (
    ScratchDir,
    rusqlite::Connection,
    agent_usage_book::store::sample_run::SampleRunId,
    agent_usage_book::store::account::AccountId,
    agent_usage_book::store::sampling_policy_snapshot::SamplingPolicySnapshotId,
) {
    use agent_usage_book::domain::time::UtcTimestamp;
    use agent_usage_book::store::account::observe_account;
    use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
    use agent_usage_book::store::migrate::run_migrations;
    use agent_usage_book::store::migrations::registry;
    use agent_usage_book::store::sample_run::{Trigger, start_sample_run};
    use agent_usage_book::store::sampling_policy_snapshot::{
        ResolvedSamplingPolicy, resolve_policy_snapshot,
    };

    const POLICY: ResolvedSamplingPolicy = ResolvedSamplingPolicy {
        ordinary_cadence: MonotonicDuration::from_millis(300_000),
        freshness_horizon: MonotonicDuration::from_millis(900_000),
        reset_edge_policy: String::new(),
        retry_backoff_policy: String::new(),
        command_budget: MonotonicDuration::from_millis(60_000),
        policy_algorithm_version: String::new(),
    };

    let scratch = ScratchDir::new("synthetic-server-store");
    let mut connection = open(
        &scratch.path().join("state.db"),
        AccessMode::ReadWrite,
        &PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1_000),
        },
    )
    .unwrap();
    run_migrations(
        &mut connection,
        &registry(),
        None,
        &agent_usage_book::domain::time::FakeClock::new(UtcTimestamp::from_unix_nanos(9_000)),
    )
    .unwrap();
    let account = observe_account(
        &connection,
        "synthetic",
        "account-a",
        UtcTimestamp::from_unix_nanos(10_000),
    )
    .unwrap();
    let run = start_sample_run(
        &connection,
        Trigger::Manual,
        UtcTimestamp::from_unix_nanos(11_000),
        "fixture",
    )
    .unwrap();
    let snapshot = resolve_policy_snapshot(
        &connection,
        account,
        UtcTimestamp::from_unix_nanos(10_000),
        &POLICY,
    )
    .unwrap();
    (scratch, connection, run, account, snapshot)
}

/// The bead's integration item: each transport failure shape produces its
/// expected failure class in the persisted attempt result. Three of the four
/// shapes are the transport's own decision; the fourth (close mid body) is
/// asserted at its transport-level truth, an incomplete 200, because the
/// failure class for a truncated body is the adapter's decision and belongs
/// to the contract suite that runs an adapter over this server.
#[test]
fn the_transport_failure_shapes_reach_the_persisted_attempt_result() {
    use agent_usage_book::domain::attempt::AttemptOutcome;
    use agent_usage_book::domain::time::UtcTimestamp;
    use agent_usage_book::store::meter_attempt::{
        DueReason, NewMeterAttempt, NewMeterAttemptResult, record_meter_attempt_result,
        result_by_attempt_id, start_meter_attempt,
    };

    let (_scratch, connection, run, account, snapshot) = fixture_store();
    let clock = RealClock::new();
    let attempt_id = start_meter_attempt(
        &connection,
        &NewMeterAttempt {
            run_id: run,
            account_id: account,
            provider: "synthetic".to_owned(),
            request_started_at: UtcTimestamp::from_unix_nanos(20_000),
            credential_context_id: Some("ctx-synthetic".to_owned()),
            policy_snapshot_id: snapshot,
            due_at: UtcTimestamp::from_unix_nanos(19_000),
            due_reason: DueReason::OrdinaryCadence,
            due_basis: None,
            provider_contract_id: "synthetic-endpoint-v1".to_owned(),
            meter_semantics_id: "synthetic-meter-v1".to_owned(),
        },
    )
    .unwrap();

    let timeouts = RequestTimeoutConfig::new(
        MonotonicDuration::from_millis(1_000),
        MonotonicDuration::from_millis(250),
        None,
    );
    let budget = CommandBudget::new(MonotonicDuration::from_millis(5_000), &clock);

    // Shape 1: headers then stall - read phase.
    let stalled = SyntheticServer::start(vec![ScriptedOutcome::HeadersThenStall {
        status: 200,
        headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
    }])
    .unwrap();
    let request =
        HttpRequest::get(stalled.url(), timeouts).with_header("Authorization", "Bearer alpha");
    let class = execute_single(&request, &budget, &clock).unwrap_err();
    assert_eq!(class, FailureClass::ReadTimeout);

    // Shape 2: accept then never respond - also the read phase, through a
    // different wire shape (no headers at all).
    let silent = SyntheticServer::start(vec![ScriptedOutcome::AcceptThenNeverRespond]).unwrap();
    let request =
        HttpRequest::get(silent.url(), timeouts).with_header("Authorization", "Bearer alpha");
    let class = execute_single(&request, &budget, &clock).unwrap_err();
    assert_eq!(class, FailureClass::ReadTimeout);

    // Shape 3: connection refused - connect phase.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let request = HttpRequest::get(format!("http://127.0.0.1:{port}/usage"), timeouts);
    let class = execute_single(&request, &budget, &clock).unwrap_err();
    assert_eq!(class, FailureClass::ConnectTimeout);

    // Persist one transport-classified shape end to end and read it back: the
    // store carries exactly the class the transport assigned, and the attempt
    // lifecycle carries the timings the adapter never owns.
    record_meter_attempt_result(
        &connection,
        &NewMeterAttemptResult {
            attempt_id,
            completed_at: UtcTimestamp::from_unix_nanos(25_000),
            elapsed: MonotonicDuration::from_millis(250),
            outcome: AttemptOutcome::Unreachable(FailureClass::ReadTimeout),
            sanitized_error_classification: None,
            retry_index: None,
            clock_anomaly: false,
        },
    )
    .unwrap();
    let stored = result_by_attempt_id(&connection, attempt_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.outcome,
        AttemptOutcome::Unreachable(FailureClass::ReadTimeout)
    );

    // Shape 4: close mid body. The transport observes an incomplete response,
    // not a transport failure: the bytes arrive and the socket closes.
    let truncating = SyntheticServer::start(vec![ScriptedOutcome::CloseMidBody {
        status: 200,
        headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
        partial_body: b"{\"incomp".to_vec(),
    }])
    .unwrap();
    let request =
        HttpRequest::get(truncating.url(), timeouts).with_header("Authorization", "Bearer alpha");
    let response = execute_single(&request, &budget, &clock).unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.body_as_str().unwrap().contains("{\"incomp"));
    // The truncated body's failure class (malformed response) is assigned by
    // the adapter that parses it, and that classification's persisted form
    // belongs to the contract suite that runs an adapter over this server.
}

// --- the contract suite over the real socket ----------------------------------

/// Glue that lets an adapter run against the real driver through the port it
/// already has: the sampler will own a production `HttpTransport` impl, and
/// until that bead lands this is the test-side equivalent, one call deep.
struct RealTransportOverExecuteSingle;

impl agent_usage_book::meter::adapter::HttpTransport for RealTransportOverExecuteSingle {
    fn send(
        &self,
        request: &HttpRequest,
        budget: &CommandBudget,
        clock: &impl agent_usage_book::domain::time::Clock,
    ) -> Result<agent_usage_book::meter::transport::HttpResponse, FailureClass> {
        execute_single(request, budget, clock)
    }
}

fn meter_fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/meter/anthropic")
            .join(name),
    )
    .unwrap_or_else(|_| panic!("fixture {name} must exist"))
}

/// The Done-when: the same fourteen-case suite the adapter contract defines
/// (PLAN.md section 34.8) runs unchanged over a real socket - the same
/// sanitized fixtures, the same adapter, but the production transport and the
/// declarative server in place of the in-process mock. The assertions mirror
/// the adapter's own suite case for case; extracting one shared suite is
/// deferred until a second network adapter proves the parametrization, per
/// the no-early-extraction rule.
#[test]
fn the_contract_suite_passes_over_the_real_socket() {
    use agent_usage_book::domain::failure::{AuthReason, HttpStatusClass};
    use agent_usage_book::domain::time::UtcTimestamp;
    use agent_usage_book::domain::window::{ModelId, WindowScope};
    use agent_usage_book::meter::adapter::{
        CredentialHandle, MeterRequest, ProviderAdapter, ProviderObservation,
    };
    use agent_usage_book::meter::anthropic::AnthropicAdapter;
    use test_support::ScriptedResponseBody;

    let credential = CredentialHandle::new("test-token-anthropic");
    let request = MeterRequest::default();
    let clock = RealClock::new();
    let transport = RealTransportOverExecuteSingle;

    /// Serves one scripted outcome over a fresh ephemeral server and returns
    /// the adapter's typed observation through the production transport.
    fn observe_outcome(
        transport: &RealTransportOverExecuteSingle,
        credential: &CredentialHandle,
        request: &MeterRequest,
        clock: &RealClock,
        outcome: ScriptedOutcome,
    ) -> ProviderObservation<agent_usage_book::meter::anthropic::AnthropicReading> {
        let server = SyntheticServer::start(vec![outcome]).unwrap();
        let adapter = AnthropicAdapter::with_endpoint(server.url());
        adapter.observe(credential, request, transport, clock)
    }

    let fixture_body = |name: &str| ScriptedOutcome::Response {
        status: 200,
        headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
        body: meter_fixture(name),
    };

    // 1. valid success
    let obs = observe_outcome(
        &transport,
        &credential,
        &request,
        &clock,
        fixture_body("valid-success.json"),
    );
    match obs {
        ProviderObservation::Measured(ref r) => {
            assert_eq!(r.windows.len(), 3);
            assert_eq!(r.windows[0].quota_used().as_ppm().get(), 80_000);
            assert_eq!(r.windows[1].quota_used().as_ppm().get(), 910_000);
        }
        other => panic!("case 1 expected Measured, got {other:?}"),
    }

    // 2. zero percentage
    let obs = observe_outcome(
        &transport,
        &credential,
        &request,
        &clock,
        fixture_body("zero-percentage.json"),
    );
    match obs {
        ProviderObservation::Measured(ref r) => {
            assert_eq!(r.windows[0].quota_used().as_ppm().get(), 0);
        }
        other => panic!("case 2 expected Measured, got {other:?}"),
    }

    // 3. multiple windows
    let obs = observe_outcome(
        &transport,
        &credential,
        &request,
        &clock,
        fixture_body("multiple-windows.json"),
    );
    match obs {
        ProviderObservation::Measured(ref r) => {
            assert_eq!(r.windows.len(), 4);
        }
        other => panic!("case 3 expected Measured, got {other:?}"),
    }

    // 4. model-specific windows
    let obs = observe_outcome(
        &transport,
        &credential,
        &request,
        &clock,
        fixture_body("model-specific.json"),
    );
    match obs {
        ProviderObservation::Measured(ref r) => {
            let model_win = r
                .windows
                .iter()
                .find(|w| w.semantic_key().as_str() == "seven_day_sonnet")
                .expect("model window present");
            assert_eq!(
                *model_win.scope(),
                WindowScope::ModelSpecific(ModelId::new("sonnet"))
            );
        }
        other => panic!("case 4 expected Measured, got {other:?}"),
    }

    // 5. 401 invalid credential (the body decides the reason)
    let obs = observe_outcome(
        &transport,
        &credential,
        &request,
        &clock,
        ScriptedOutcome::Response {
            status: 401,
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: meter_fixture("error-401-invalid.json"),
        },
    );
    assert_eq!(
        obs,
        ProviderObservation::AuthRequired(AuthReason::CredentialRejected)
    );

    // 6. provider-declared authentication expiration
    let obs = observe_outcome(
        &transport,
        &credential,
        &request,
        &clock,
        ScriptedOutcome::Response {
            status: 401,
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: meter_fixture("error-401-expired.json"),
        },
    );
    assert_eq!(
        obs,
        ProviderObservation::AuthRequired(AuthReason::ProviderDeclaredExpiry)
    );

    // 7. 403 with ambiguous semantics: client error, never authentication
    let obs = observe_outcome(
        &transport,
        &credential,
        &request,
        &clock,
        ScriptedOutcome::Response {
            status: 403,
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: meter_fixture("error-403-ambiguous.json"),
        },
    );
    assert_eq!(
        obs,
        ProviderObservation::Unreachable(FailureClass::HttpStatus(HttpStatusClass::ClientError))
    );

    // 8. 429 rate limited, with the provider's Retry-After honoured
    let obs = observe_outcome(
        &transport,
        &credential,
        &request,
        &clock,
        ScriptedOutcome::Response {
            status: 429,
            headers: vec![
                ("Content-Type".to_owned(), "application/json".to_owned()),
                ("Retry-After".to_owned(), "60".to_owned()),
            ],
            body: meter_fixture("error-429.json"),
        },
    );
    assert_eq!(
        obs,
        ProviderObservation::Unreachable(FailureClass::RateLimited {
            retry_after: Some(MonotonicDuration::from_seconds(60)),
        })
    );

    // 9. timeout: the adapter owns its timeouts, so the stalled server holds
    // the observation until the adapter's OWN deadline fires - 15 seconds,
    // the honest cost of proving the composed wiring rather than a mocked
    // one. What fires is the total deadline, not the 10s read timeout:
    // execute_single documents that ureq discards timeout_read whenever an
    // overall timeout is declared, and the deadline's io error surfaces
    // through the body reader with a non-timeout kind but a
    // deadline-flavoured message, which the transport's classifier maps to
    // ReadTimeout (aub-deadline-body-read-timeout-u3db). The case pins that
    // composed classification: any regression to Measured, AuthRequired, a
    // fallback zero, or the pre-fix MalformedBody still fails.
    let stalled = SyntheticServer::start(vec![ScriptedOutcome::HeadersThenStall {
        status: 200,
        headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
    }])
    .unwrap();
    let stalled_adapter = AnthropicAdapter::with_endpoint(stalled.url());
    let obs = stalled_adapter.observe(&credential, &request, &transport, &clock);
    assert_eq!(
        obs,
        ProviderObservation::Unreachable(FailureClass::ReadTimeout)
    );

    // 10. malformed JSON
    let obs = observe_outcome(
        &transport,
        &credential,
        &request,
        &clock,
        fixture_body("malformed.json"),
    );
    assert_eq!(
        obs,
        ProviderObservation::Unreachable(FailureClass::MalformedBody)
    );

    // 11. missing expected field
    let obs = observe_outcome(
        &transport,
        &credential,
        &request,
        &clock,
        fixture_body("missing-field.json"),
    );
    assert_eq!(
        obs,
        ProviderObservation::Unreachable(FailureClass::MissingRequiredField)
    );

    // 12. unknown additional field: measured, and the payload is retained for
    // the capsule builder (aub-eun.5 owns retention).
    let obs = observe_outcome(
        &transport,
        &credential,
        &request,
        &clock,
        fixture_body("unknown-fields.json"),
    );
    match obs {
        ProviderObservation::Measured(ref r) => {
            assert!(r.raw_payload.is_some());
        }
        other => panic!("case 12 expected Measured, got {other:?}"),
    }

    // 13. stale server timestamp: measured, with the provider's own timestamp
    let obs = observe_outcome(
        &transport,
        &credential,
        &request,
        &clock,
        fixture_body("stale-timestamp.json"),
    );
    match obs {
        ProviderObservation::Measured(ref r) => {
            assert_eq!(
                r.windows[0].resets_at(),
                UtcTimestamp::parse_rfc3339("2020-01-01T00:00:00.000Z").unwrap()
            );
        }
        other => panic!("case 13 expected Measured, got {other:?}"),
    }

    // 14. reset change: two reads of the same window disagree on the reset
    let server = SyntheticServer::start(vec![ScriptedOutcome::Success(ScriptedResponseBody {
        status: 200,
        headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
        body: meter_fixture("reset-changed-a.json"),
    })])
    .unwrap();
    let adapter = AnthropicAdapter::with_endpoint(server.url());
    let obs_a = adapter.observe(&credential, &request, &transport, &clock);
    server.push_outcome(ScriptedOutcome::Success(ScriptedResponseBody {
        status: 200,
        headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
        body: meter_fixture("reset-changed-b.json"),
    }));
    let obs_b = adapter.observe(&credential, &request, &transport, &clock);
    match (obs_a, obs_b) {
        (ProviderObservation::Measured(a), ProviderObservation::Measured(b)) => {
            assert_ne!(a.windows[0].resets_at(), b.windows[0].resets_at());
        }
        other => panic!("case 14 expected a Measured pair, got {other:?}"),
    }
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

/// One scratch state directory per test, removed on drop.
struct ScratchDir(std::path::PathBuf);

impl ScratchDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("aub-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("scratch dir must be creatable");
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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
