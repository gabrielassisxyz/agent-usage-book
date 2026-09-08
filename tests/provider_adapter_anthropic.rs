//! Integration tests and contract suite for the Anthropic provider adapter (`aub-eun.4`).
//!
//! Covers:
//! - All 14 contract test cases from PLAN.md section 34.8 against sanitized fixtures
//! - Unit tests for 403 classification, unknown fields, and missing fields
//! - Sanitization scan over the adapter's fixture directory against `test_support::sanitization`
//! - Semantic-identifier stability and applicability contract

use std::path::PathBuf;

use agent_usage_book::domain::failure::{AuthReason, FailureClass, HttpStatusClass};
use agent_usage_book::domain::ids::{MeterSemanticsId, ProviderContractId};
use agent_usage_book::domain::time::{
    Clock, FakeClock, MeasurementBasis, MonotonicDuration, UtcTimestamp,
};
use agent_usage_book::domain::window::{ModelId, WindowScope};
use agent_usage_book::meter::adapter::{
    AdapterDeclarations, CredentialHandle, HttpTransport, MeterRequest, ProviderAdapter,
    ProviderObservation,
};
use agent_usage_book::meter::anthropic::AnthropicAdapter;
use agent_usage_book::meter::transport::{CommandBudget, HttpRequest, HttpResponse};
use test_support::sanitization::matched_patterns;

const FIXTURES_DIR: &str = "tests/fixtures/meter/anthropic";

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture_path(name: &str) -> PathBuf {
    crate_root().join(FIXTURES_DIR).join(name)
}

fn read_fixture(name: &str) -> Vec<u8> {
    let path = fixture_path(name);
    std::fs::read(&path)
        .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", path.display()))
}

struct MockTransport {
    response: Result<HttpResponse, FailureClass>,
}

impl MockTransport {
    fn ok(status: u16, body: Vec<u8>) -> Self {
        Self {
            response: Ok(HttpResponse {
                status,
                headers: Vec::new(),
                body,
            }),
        }
    }

    fn ok_with_header(status: u16, header_name: &str, header_val: &str, body: Vec<u8>) -> Self {
        Self {
            response: Ok(HttpResponse {
                status,
                headers: vec![(header_name.to_string(), header_val.to_string())],
                body,
            }),
        }
    }

    fn err(failure: FailureClass) -> Self {
        Self {
            response: Err(failure),
        }
    }
}

impl HttpTransport for MockTransport {
    fn send(
        &self,
        _request: &HttpRequest,
        _budget: &CommandBudget,
        _clock: &impl Clock,
    ) -> Result<HttpResponse, FailureClass> {
        self.response.clone()
    }
}

fn test_credential() -> CredentialHandle {
    CredentialHandle::new("test-token-anthropic")
}

fn test_clock() -> FakeClock {
    FakeClock::new(UtcTimestamp::from_unix_nanos(1_700_000_000_000_000_000))
}

#[test]
fn fixture_corpus_sanitization_scan() {
    let dir = crate_root().join(FIXTURES_DIR);
    assert!(
        dir.is_dir(),
        "fixtures directory {} must exist",
        dir.display()
    );

    let entries = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("failed to read directory {}: {e}", dir.display()));

    let mut scanned_count = 0;
    for entry in entries {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_file() {
            let content = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("failed to read file {}: {e}", path.display()));
            let hits = matched_patterns(&content);
            assert!(
                hits.is_empty(),
                "fixture file {} matched forbidden patterns: {hits:?}",
                path.display()
            );
            scanned_count += 1;
        }
    }
    assert!(
        scanned_count >= 14,
        "expected at least 14 fixture files, scanned {scanned_count}"
    );
}

#[test]
fn adapter_semantics_table_names_limits_kinds() {
    let path = crate_root().join("docs/adapter-semantics-validation.md");
    let table = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    let adapter_row = table
        .lines()
        .find(|line| {
            line.starts_with("| Anthropic (`src/meter/anthropic.rs`",)
                && !line.starts_with("| Anthropic idle",)
        })
        .expect("the Anthropic adapter row must be present");
    for kind in ["session", "weekly_all", "weekly_scoped"] {
        assert!(
            adapter_row.contains(&format!("limits[].kind={kind}")),
            "the adapter semantics table must document limits[].kind={kind}"
        );
    }
    // The status-line contract's row (aub-gnke): the second contract on the
    // same adapter, the freshness rule that decides when the record file's
    // last line counts, and the window-name mapping the reader applies.
    let statusline_row = table
        .lines()
        .find(|line| line.starts_with("| Anthropic status line"))
        .expect("the Anthropic status-line row must be present");
    for fact in [
        "anthropic-statusline-rate-limits-v1",
        "anthropic-oauth-usage-limits-v1",
        "ordinary cadence",
        "five_hour",
        "seven_day",
    ] {
        assert!(
            table.contains(fact),
            "the adapter semantics table must name {fact} for the status-line contract"
        );
    }
    assert!(
        statusline_row.contains("subset source"),
        "the status-line row must state the subset-source rule"
    );
}

#[test]
fn contract_all_fourteen_cases() {
    let adapter = AnthropicAdapter::new();
    let cred = test_credential();
    let clock = test_clock();
    let req = MeterRequest::default();

    // 1. valid success
    let body = read_fixture("valid-success.json");
    let obs = adapter.observe(&cred, &req, &MockTransport::ok(200, body), &clock);
    match obs {
        ProviderObservation::Measured(r) => {
            assert_eq!(r.windows.len(), 3);
            assert_eq!(r.windows[0].quota_used().as_ppm().get(), 80_000);
            assert_eq!(r.windows[1].quota_used().as_ppm().get(), 910_000);
        }
        other => panic!("case 1 expected Measured, got {other:?}"),
    }

    // 2. zero percentage
    let body = read_fixture("zero-percentage.json");
    let obs = adapter.observe(&cred, &req, &MockTransport::ok(200, body), &clock);
    match obs {
        ProviderObservation::Measured(r) => {
            assert_eq!(r.windows[0].quota_used().as_ppm().get(), 0);
        }
        other => panic!("case 2 expected Measured, got {other:?}"),
    }

    // 3. multiple windows
    let body = read_fixture("multiple-windows.json");
    let obs = adapter.observe(&cred, &req, &MockTransport::ok(200, body), &clock);
    match obs {
        ProviderObservation::Measured(r) => {
            assert_eq!(r.windows.len(), 4);
        }
        other => panic!("case 3 expected Measured, got {other:?}"),
    }

    // 4. model-specific windows
    let body = read_fixture("model-specific.json");
    let obs = adapter.observe(&cred, &req, &MockTransport::ok(200, body), &clock);
    match obs {
        ProviderObservation::Measured(r) => {
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

    // 5. 401 invalid credential
    let body = read_fixture("error-401-invalid.json");
    let obs = adapter.observe(&cred, &req, &MockTransport::ok(401, body), &clock);
    assert_eq!(
        obs,
        ProviderObservation::AuthRequired(AuthReason::CredentialRejected)
    );

    // 6. provider-defined authentication expiration
    let body = read_fixture("error-401-expired.json");
    let obs = adapter.observe(&cred, &req, &MockTransport::ok(401, body), &clock);
    assert_eq!(
        obs,
        ProviderObservation::AuthRequired(AuthReason::ProviderDeclaredExpiry)
    );

    // 7. 403 with ambiguous semantics
    let body = read_fixture("error-403-ambiguous.json");
    let obs = adapter.observe(&cred, &req, &MockTransport::ok(403, body), &clock);
    assert_eq!(
        obs,
        ProviderObservation::Unreachable(FailureClass::HttpStatus(HttpStatusClass::ClientError))
    );

    // 8. 429 rate limited
    let body = read_fixture("error-429.json");
    let obs = adapter.observe(
        &cred,
        &req,
        &MockTransport::ok_with_header(429, "Retry-After", "60", body),
        &clock,
    );
    assert_eq!(
        obs,
        ProviderObservation::Unreachable(FailureClass::RateLimited {
            retry_after: Some(MonotonicDuration::from_seconds(60)),
        })
    );

    // 9. timeout
    let obs = adapter.observe(
        &cred,
        &req,
        &MockTransport::err(FailureClass::ReadTimeout),
        &clock,
    );
    assert_eq!(
        obs,
        ProviderObservation::Unreachable(FailureClass::ReadTimeout)
    );

    // 10. malformed JSON
    let body = read_fixture("malformed.json");
    let obs = adapter.observe(&cred, &req, &MockTransport::ok(200, body), &clock);
    assert_eq!(
        obs,
        ProviderObservation::Unreachable(FailureClass::MalformedBody)
    );

    // 11. missing expected field
    let body = read_fixture("missing-field.json");
    let obs = adapter.observe(&cred, &req, &MockTransport::ok(200, body), &clock);
    assert_eq!(
        obs,
        ProviderObservation::Unreachable(FailureClass::MissingRequiredField)
    );

    // 12. unknown additional field: retention is the evidence capsule's job
    // (aub-eun.5), not the normalized reading's.
    let body = read_fixture("unknown-fields.json");
    let captured =
        adapter.observe_with_evidence(&cred, &req, &MockTransport::ok(200, body), &clock);
    match captured.observation {
        ProviderObservation::Measured(_) => {
            let capsule = captured
                .evidence
                .expect("a 200 response must carry an evidence capsule");
            let parsed: serde_json::Value = serde_json::from_str(capsule.serialized()).unwrap();
            assert!(
                parsed["quota_response"]
                    .get("unknown_top_level_metric")
                    .is_some(),
                "the capsule's quota_response must retain the unknown field: {}",
                capsule.serialized()
            );
        }
        other => panic!("case 12 expected Measured, got {other:?}"),
    }

    // 13. stale server timestamp
    let body = read_fixture("stale-timestamp.json");
    let obs = adapter.observe(&cred, &req, &MockTransport::ok(200, body), &clock);
    match obs {
        ProviderObservation::Measured(r) => {
            assert_eq!(
                r.windows[0].resets_at(),
                Some(UtcTimestamp::parse_rfc3339("2020-01-01T00:00:00.000Z").unwrap())
            );
        }
        other => panic!("case 13 expected Measured, got {other:?}"),
    }

    // 14. reset change
    let body_a = read_fixture("reset-changed-a.json");
    let body_b = read_fixture("reset-changed-b.json");
    let obs_a = adapter.observe(&cred, &req, &MockTransport::ok(200, body_a), &clock);
    let obs_b = adapter.observe(&cred, &req, &MockTransport::ok(200, body_b), &clock);
    match (obs_a, obs_b) {
        (ProviderObservation::Measured(a), ProviderObservation::Measured(b)) => {
            assert_ne!(a.windows[0].resets_at(), b.windows[0].resets_at());
        }
        other => panic!("case 14 expected Measured pair, got {other:?}"),
    }

    // 15. idle 5-hour window with null reset
    let body = read_fixture("idle-five-hour.json");
    let obs = adapter.observe(&cred, &req, &MockTransport::ok(200, body), &clock);
    match obs {
        ProviderObservation::Measured(r) => {
            assert_eq!(r.windows.len(), 2);
            assert_eq!(r.windows[0].semantic_key().as_str(), "five_hour");
            assert!(r.windows[0].reset_state().is_not_started());
            assert_eq!(r.windows[0].resets_at(), None);
            assert_eq!(r.windows[1].semantic_key().as_str(), "seven_day");
            assert_eq!(
                r.windows[1].resets_at(),
                Some(UtcTimestamp::parse_rfc3339("2026-09-06T12:00:00.000Z").unwrap())
            );
        }
        other => panic!("case 15 expected Measured, got {other:?}"),
    }
}

#[test]
fn semantic_identifiers_and_changed_semantics_declaration() {
    let adapter = AnthropicAdapter::new();
    let decls = adapter.declarations();
    assert_eq!(decls.measurement_basis, MeasurementBasis::LocallyReceived);
    assert_eq!(
        decls.provider_contract_id.as_str(),
        AnthropicAdapter::LIMITS_CONTRACT_ID
    );
    assert_eq!(
        decls
            .required_window_kinds
            .iter()
            .map(|kind| kind.as_str())
            .collect::<Vec<_>>(),
        vec!["session", "weekly_all"]
    );
    assert_eq!(
        decls.meter_semantics_id.as_str(),
        "anthropic-subscription-v1"
    );

    let changed_decls = AdapterDeclarations::new(
        MeasurementBasis::LocallyReceived,
        ProviderContractId::new("anthropic-oauth-usage-v1"),
        MeterSemanticsId::new("anthropic-subscription-v2"),
    );
    let changed_adapter =
        AnthropicAdapter::with_declarations(AnthropicAdapter::DEFAULT_ENDPOINT, changed_decls);
    assert_ne!(
        adapter.declarations().meter_semantics_id,
        changed_adapter.declarations().meter_semantics_id
    );
}

#[test]
fn limits_fixture_preserves_kinds_scope_activity_and_severity() {
    let adapter = AnthropicAdapter::new();
    let observation = adapter.observe(
        &test_credential(),
        &MeterRequest::default(),
        &MockTransport::ok(200, read_fixture("limits-success.json")),
        &test_clock(),
    );
    let ProviderObservation::Measured(reading) = observation else {
        panic!("limits fixture must produce a measured reading");
    };

    assert_eq!(reading.windows.len(), 3);
    assert_eq!(reading.windows[0].semantic_key().as_str(), "session");
    assert_eq!(reading.windows[0].scope(), &WindowScope::AccountWide);
    assert!(reading.windows[0].is_active());
    assert_eq!(reading.windows[0].severity().as_str(), "normal");
    assert_eq!(reading.windows[1].semantic_key().as_str(), "weekly_all");
    let scoped = reading
        .windows
        .iter()
        .find(|window| window.scope().scoped_model().is_some())
        .expect("weekly_scoped window must be present");
    assert_eq!(scoped.semantic_key().as_str(), "weekly_scoped_sonnet");
    assert_eq!(
        scoped.scope(),
        &WindowScope::ModelSpecific(ModelId::new("sonnet"))
    );
    assert_eq!(scoped.severity().as_str(), "critical");
    assert_eq!(
        reading.calibration_applicability,
        agent_usage_book::meter::anthropic::CalibrationApplicability::CarryOver
    );
    assert_eq!(
        reading.provider_contract_id.as_str(),
        AnthropicAdapter::LIMITS_CONTRACT_ID
    );
}

#[test]
fn limits_preserve_an_inactive_provider_fact() {
    let mut body: serde_json::Value =
        serde_json::from_slice(&read_fixture("limits-success.json")).unwrap();
    body["limits"][2]["is_active"] = serde_json::json!(false);
    let observation = AnthropicAdapter::new().observe(
        &test_credential(),
        &MeterRequest::default(),
        &MockTransport::ok(200, serde_json::to_vec(&body).unwrap()),
        &test_clock(),
    );
    let ProviderObservation::Measured(reading) = observation else {
        panic!("an inactive constraint remains a measured response");
    };
    let scoped = reading
        .windows
        .iter()
        .find(|window| window.semantic_key().as_str() == "weekly_scoped_sonnet")
        .expect("the scoped constraint must remain present");
    assert!(!scoped.is_active());
}

fn limits_body(include_scoped: bool, include_weekly_all: bool) -> Vec<u8> {
    let mut limits = vec![serde_json::json!({
        "kind": "session",
        "percent": 8.0,
        "severity": "normal",
        "resets_at": "2026-09-05T17:00:00.000Z",
        "scope": null,
        "is_active": true
    })];
    if include_weekly_all {
        limits.push(serde_json::json!({
            "kind": "weekly_all",
            "percent": 21.0,
            "severity": "warning",
            "resets_at": "2026-09-06T12:00:00.000Z",
            "scope": null,
            "is_active": true
        }));
    }
    if include_scoped {
        limits.push(serde_json::json!({
            "kind": "weekly_scoped",
            "percent": 24.0,
            "severity": "critical",
            "resets_at": "2026-09-06T12:00:00.000Z",
            "scope": {"model": "sonnet"},
            "is_active": true
        }));
    }
    serde_json::to_vec(&serde_json::json!({"limits": limits})).unwrap()
}

#[test]
fn limits_require_weekly_all_but_not_weekly_scoped() {
    let adapter = AnthropicAdapter::new();
    let missing_required = adapter.observe(
        &test_credential(),
        &MeterRequest::default(),
        &MockTransport::ok(200, limits_body(true, false)),
        &test_clock(),
    );
    assert_eq!(
        missing_required,
        ProviderObservation::Unreachable(FailureClass::MissingRequiredField)
    );

    let without_scoped = adapter.observe(
        &test_credential(),
        &MeterRequest::default(),
        &MockTransport::ok(200, limits_body(false, true)),
        &test_clock(),
    );
    let ProviderObservation::Measured(reading) = without_scoped else {
        panic!("weekly_scoped is optional");
    };
    assert_eq!(reading.windows.len(), 2);
    assert!(reading.anomalies.is_empty());
}

#[test]
fn matching_named_blocks_allow_calibration_and_disagreement_is_an_anomaly() {
    let adapter = AnthropicAdapter::new();
    let mut agreeing: serde_json::Value =
        serde_json::from_slice(&read_fixture("limits-success.json")).unwrap();
    let ProviderObservation::Measured(reading) = adapter.observe(
        &test_credential(),
        &MeterRequest::default(),
        &MockTransport::ok(200, serde_json::to_vec(&agreeing).unwrap()),
        &test_clock(),
    ) else {
        panic!("matching shapes must parse");
    };
    assert_eq!(
        reading.calibration_applicability,
        agent_usage_book::meter::anthropic::CalibrationApplicability::CarryOver
    );
    assert!(reading.anomalies.is_empty());

    agreeing["limits"][1]["percent"] = serde_json::json!(22.0);
    let ProviderObservation::Measured(reading) = adapter.observe(
        &test_credential(),
        &MeterRequest::default(),
        &MockTransport::ok(200, serde_json::to_vec(&agreeing).unwrap()),
        &test_clock(),
    ) else {
        panic!("a disagreement still leaves a measured provider response");
    };
    assert_eq!(
        reading.calibration_applicability,
        agent_usage_book::meter::anthropic::CalibrationApplicability::Inapplicable
    );
    assert_eq!(
        reading.anomalies[0].code,
        "limits_named_window_disagreement"
    );
}

/// The status-line reader (`aub-gnke`): the account's record file read
/// through the adapter's local-file arm, its last line interpreted under the
/// status-line contract when it is fresh, and the endpoint taken when it is
/// not. Tests over canned bodies cannot carry the freshness rule, which
/// needs a real file whose last line ages against a clock, so these run over
/// scratch files the way the real transport reads them.
mod statusline_reader {
    use std::cell::Cell;
    use std::path::PathBuf;

    use agent_usage_book::domain::failure::FailureClass;
    use agent_usage_book::domain::time::{
        FakeClock, MeasurementBasis, MonotonicDuration, ProviderObservedAt, UtcTimestamp,
    };
    use agent_usage_book::domain::window::WindowScope;
    use agent_usage_book::meter::adapter::{AnthropicStatuslineSource, MeterRequest};
    use agent_usage_book::meter::sampler::MeteredReading as _;
    use agent_usage_book::meter::transport::{
        CommandBudget, HttpRequest, HttpResponse, LOCAL_FILE_MTIME_HEADER, LOCAL_FILE_PATH_HEADER,
    };
    use test_support::StateDir;

    use super::{
        AnthropicAdapter, HttpTransport, ProviderAdapter, ProviderObservation, read_fixture,
        test_credential,
    };

    /// The tick every test samples at: 2026-09-08T02:34:56Z.
    const TICK_NANOS: i64 = 1_788_834_896_000_000_000;
    /// The session window's reset: 2026-09-08T06:34:56Z.
    const FIVE_HOUR_RESET: i64 = 1_788_849_296;
    /// The weekly window's reset: 2026-09-15T02:34:56Z.
    const SEVEN_DAY_RESET: i64 = 1_789_439_696;
    /// The fixture cwd the capsule must never carry. Distinctive on purpose,
    /// so the grep proves absence by value and not by key.
    const FIXTURE_CWD: &str = "/tmp/worktree/project";
    const SESSION_ID: &str = "8cd9c60a-e10a-4d44-857e-6b2b931b4d9d";

    /// One record line as the tee writes it: the tee's own receive stamp at
    /// RFC 3339 UTC second precision, the session, the cwd, and the windows
    /// map. `received_at` is 60 s before the tick.
    fn record_line() -> String {
        format!(
            "{{\"received_at\":\"2026-09-08T02:33:56Z\",\"session_id\":\"{SESSION_ID}\",\"cwd\":\"{FIXTURE_CWD}\",\"windows\":{{\"five_hour\":{{\"used_percentage\":40,\"resets_at\":{FIVE_HOUR_RESET}}},\"seven_day\":{{\"used_percentage\":12,\"resets_at\":{SEVEN_DAY_RESET}}}}}}}\n"
        )
    }

    /// The same line re-stamped, so a test can age the last line past the
    /// freshness window or push it ahead of the tick without rebuilding the
    /// rest of the shape.
    fn record_line_with_received_at(received_at: &str) -> String {
        record_line().replace("2026-09-08T02:33:56Z", received_at)
    }

    /// A scratch state directory holding the record file at the path the
    /// caller resolves for the account.
    fn record_file(state: &StateDir, contents: &str) -> PathBuf {
        let dir = state.path().join("statusline");
        std::fs::create_dir_all(&dir).expect("the record directory must be creatable");
        let path = dir.join("gmail.jsonl");
        std::fs::write(&path, contents).expect("the record file must be writable");
        path
    }

    fn request(record_path: &std::path::Path) -> MeterRequest {
        MeterRequest {
            anthropic_statusline: Some(AnthropicStatuslineSource {
                record_path: record_path.to_path_buf(),
                // The ordinary cadence every freshness case is held to.
                fresh_window: MonotonicDuration::from_seconds(300),
            }),
            ..MeterRequest::default()
        }
    }

    fn clock() -> FakeClock {
        FakeClock::new(UtcTimestamp::from_unix_nanos(TICK_NANOS))
    }

    /// The local-file arm as the real transport serves it - the file's bytes
    /// with its path and mtime as headers, a malformed-body failure when
    /// there is no file - and the endpoint arm counted, so a test can prove
    /// the endpoint was never called. The endpoint answers with one of the
    /// committed sanitized fixtures, named by `endpoint_fixture`.
    struct StatuslineTransport {
        endpoint_fixture: &'static str,
        endpoint_calls: Cell<usize>,
    }

    impl StatuslineTransport {
        fn serving_endpoint_from_fixture(name: &'static str) -> Self {
            Self {
                endpoint_fixture: name,
                endpoint_calls: Cell::new(0),
            }
        }
    }

    impl HttpTransport for StatuslineTransport {
        fn send(
            &self,
            request: &HttpRequest,
            _budget: &CommandBudget,
            _clock: &impl agent_usage_book::domain::time::Clock,
        ) -> Result<HttpResponse, FailureClass> {
            if let Some(local) = &request.local_file {
                let body = std::fs::read(&local.path).map_err(|_| FailureClass::MalformedBody)?;
                return Ok(HttpResponse {
                    status: 200,
                    headers: vec![
                        (
                            LOCAL_FILE_PATH_HEADER.to_string(),
                            local.path.display().to_string(),
                        ),
                        (LOCAL_FILE_MTIME_HEADER.to_string(), TICK_NANOS.to_string()),
                    ],
                    body,
                });
            }
            self.endpoint_calls.set(self.endpoint_calls.get() + 1);
            Ok(HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: read_fixture(self.endpoint_fixture),
            })
        }
    }

    fn expect_measured(
        obs: ProviderObservation<agent_usage_book::meter::anthropic::AnthropicReading>,
    ) -> agent_usage_book::meter::anthropic::AnthropicReading {
        match obs {
            ProviderObservation::Measured(reading) => reading,
            ProviderObservation::AuthRequired(reason) => {
                panic!("expected Measured, got AuthRequired({reason:?})")
            }
            ProviderObservation::Unreachable(failure) => {
                panic!("expected Measured, got Unreachable({failure:?})")
            }
        }
    }

    #[test]
    fn a_fresh_status_line_is_observed_without_any_endpoint_request() {
        let state = StateDir::new();
        let path = record_file(&state, &record_line());
        let transport = StatuslineTransport::serving_endpoint_from_fixture("limits-success.json");
        let adapter = AnthropicAdapter::new();

        let reading = expect_measured(
            adapter
                .observe_with_evidence(&test_credential(), &request(&path), &transport, &clock())
                .observation,
        );

        assert_eq!(
            reading.provider_contract_id.as_str(),
            AnthropicAdapter::STATUSLINE_CONTRACT_ID,
            "a fresh line is one observation under the status-line contract"
        );
        assert_eq!(
            reading.source_measurement_basis(),
            Some(MeasurementBasis::ProviderObserved),
            "the line is provider-observed at the render instant"
        );
        assert_eq!(
            reading.provider_observed_at,
            Some(ProviderObservedAt::new(
                UtcTimestamp::parse_rfc3339("2026-09-08T02:33:56Z").unwrap()
            )),
        );
        assert_eq!(transport.endpoint_calls.get(), 0, "no request was sent");

        // The two windows the line carries, under the names the shared
        // vocabulary knows them by, with the percentages at the endpoint's
        // whole-percent resolution and the resets as known instants.
        assert_eq!(reading.windows.len(), 2);
        let session = &reading.windows[0];
        assert_eq!(session.semantic_key().as_str(), "session");
        assert_eq!(*session.scope(), WindowScope::AccountWide);
        assert_eq!(session.quota_used().as_ppm().get(), 400_000);
        assert_eq!(
            session.reset_state().instant(),
            Some(UtcTimestamp::from_unix_nanos(
                FIVE_HOUR_RESET * 1_000_000_000
            ))
        );
        let weekly = &reading.windows[1];
        assert_eq!(weekly.semantic_key().as_str(), "weekly_all");
        assert_eq!(weekly.quota_used().as_ppm().get(), 120_000);
        assert_eq!(
            weekly.reset_state().instant(),
            Some(UtcTimestamp::from_unix_nanos(
                SEVEN_DAY_RESET * 1_000_000_000
            ))
        );
    }

    #[test]
    fn a_stale_line_and_a_missing_file_take_the_endpoint() {
        let adapter = AnthropicAdapter::new();

        // 301 s old: past the ordinary cadence, so the endpoint runs.
        let state = StateDir::new();
        let path = record_file(
            &state,
            &record_line_with_received_at("2026-09-08T02:29:55Z"),
        );
        let transport = StatuslineTransport::serving_endpoint_from_fixture("limits-success.json");
        let reading = expect_measured(
            adapter
                .observe_with_evidence(&test_credential(), &request(&path), &transport, &clock())
                .observation,
        );
        assert_eq!(
            reading.provider_contract_id.as_str(),
            AnthropicAdapter::LIMITS_CONTRACT_ID,
            "a stale line falls through to the endpoint contract"
        );
        assert_eq!(transport.endpoint_calls.get(), 1);

        // No file at all: the endpoint, and nothing from the record path.
        let state = StateDir::new();
        let path = state.path().join("statusline").join("gmail.jsonl");
        let transport = StatuslineTransport::serving_endpoint_from_fixture("limits-success.json");
        let reading = expect_measured(
            adapter
                .observe_with_evidence(&test_credential(), &request(&path), &transport, &clock())
                .observation,
        );
        assert_eq!(
            reading.provider_contract_id.as_str(),
            AnthropicAdapter::LIMITS_CONTRACT_ID
        );
        assert_eq!(transport.endpoint_calls.get(), 1);
    }

    #[test]
    fn a_line_dated_ahead_of_the_tick_is_stale() {
        let state = StateDir::new();
        let path = record_file(
            &state,
            &record_line_with_received_at("2026-09-08T02:35:56Z"),
        );
        let transport = StatuslineTransport::serving_endpoint_from_fixture("limits-success.json");
        let adapter = AnthropicAdapter::new();
        let reading = expect_measured(
            adapter
                .observe_with_evidence(&test_credential(), &request(&path), &transport, &clock())
                .observation,
        );
        assert_eq!(
            reading.provider_contract_id.as_str(),
            AnthropicAdapter::LIMITS_CONTRACT_ID,
            "a line from the future is the one direction the skew envelope never excuses"
        );
        assert_eq!(transport.endpoint_calls.get(), 1);
    }

    #[test]
    fn an_unknown_window_name_maps_to_no_row_and_is_kept_in_the_capsule() {
        let state = StateDir::new();
        // A model-scoped window, admitted by the tee's shape rule, whose
        // name this reader does not know. It maps to no meter window row;
        // the name survives in the capsule, which is where the evidence
        // records it.
        let line = record_line().replace(
            "\"windows\":{",
            "\"windows\":{\"seven_day_opus\":{\"used_percentage\":9,\"resets_at\":1789439697},",
        );
        let path = record_file(&state, &line);
        let transport = StatuslineTransport::serving_endpoint_from_fixture("limits-success.json");
        let adapter = AnthropicAdapter::new();
        let captured = adapter.observe_with_evidence(
            &test_credential(),
            &request(&path),
            &transport,
            &clock(),
        );
        let reading = expect_measured(captured.observation);
        let keys: Vec<&str> = reading
            .windows
            .iter()
            .map(|window| window.semantic_key().as_str())
            .collect();
        assert_eq!(
            keys,
            vec!["session", "weekly_all"],
            "no row for the unknown name"
        );
        let capsule = captured
            .evidence
            .as_ref()
            .expect("a measured reading carries its capsule")
            .serialized();
        assert!(
            capsule.contains("seven_day_opus"),
            "the unknown window name is recorded in the evidence: {capsule}"
        );
    }

    #[test]
    fn the_capsule_never_carries_the_cwd() {
        let state = StateDir::new();
        let path = record_file(&state, &record_line());
        let transport = StatuslineTransport::serving_endpoint_from_fixture("limits-success.json");
        let adapter = AnthropicAdapter::new();
        let captured = adapter.observe_with_evidence(
            &test_credential(),
            &request(&path),
            &transport,
            &clock(),
        );
        let _ = expect_measured(captured.observation);
        let capsule = captured
            .evidence
            .as_ref()
            .expect("a measured reading carries its capsule")
            .serialized();
        assert!(
            !capsule.contains(FIXTURE_CWD),
            "a path that can carry a project name never reaches the evidence: {capsule}"
        );
        assert!(!capsule.contains("cwd"), "not even the key: {capsule}");
        assert!(
            capsule.contains(SESSION_ID),
            "the session id is part of the capsule the brief names"
        );
    }

    #[test]
    fn a_window_value_outside_the_percentage_range_is_dropped_not_guessed() {
        let state = StateDir::new();
        let line = record_line().replace("\"used_percentage\":40", "\"used_percentage\":150");
        let path = record_file(&state, &line);
        let transport = StatuslineTransport::serving_endpoint_from_fixture("limits-success.json");
        let adapter = AnthropicAdapter::new();
        let reading = expect_measured(
            adapter
                .observe_with_evidence(&test_credential(), &request(&path), &transport, &clock())
                .observation,
        );
        let keys: Vec<&str> = reading
            .windows
            .iter()
            .map(|window| window.semantic_key().as_str())
            .collect();
        assert_eq!(
            keys,
            vec!["weekly_all"],
            "an impossible percentage is no window"
        );
        assert_eq!(reading.dropped_windows.len(), 1);
        assert_eq!(
            reading.dropped_windows[0].semantic_key.as_str(),
            "five_hour"
        );
    }

    #[test]
    fn a_null_resets_at_reads_as_a_window_that_has_not_started() {
        let state = StateDir::new();
        let line = record_line().replace(
            "\"five_hour\":{\"used_percentage\":40,\"resets_at\":1788849296}",
            "\"five_hour\":{\"used_percentage\":0,\"resets_at\":null}",
        );
        let path = record_file(&state, &line);
        let transport = StatuslineTransport::serving_endpoint_from_fixture("limits-success.json");
        let adapter = AnthropicAdapter::new();
        let reading = expect_measured(
            adapter
                .observe_with_evidence(&test_credential(), &request(&path), &transport, &clock())
                .observation,
        );
        let session = reading
            .windows
            .iter()
            .find(|window| window.semantic_key().as_str() == "session")
            .expect("the session window is still carried");
        assert!(session.reset_state().is_not_started());
        assert_eq!(session.quota_used().as_ppm().get(), 0);
    }
}
