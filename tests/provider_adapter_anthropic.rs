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
                UtcTimestamp::parse_rfc3339("2020-01-01T00:00:00.000Z").unwrap()
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
}

#[test]
fn semantic_identifiers_and_changed_semantics_declaration() {
    let adapter = AnthropicAdapter::new();
    let decls = adapter.declarations();
    assert_eq!(decls.measurement_basis, MeasurementBasis::LocallyReceived);
    assert_eq!(
        decls.provider_contract_id.as_str(),
        "anthropic-oauth-usage-v1"
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
