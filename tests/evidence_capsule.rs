//! Integration tests for the sanitized response evidence capsule (`aub-eun.5`).
//!
//! Ties the capsule builder (`meter::evidence`), the Anthropic adapter's replay
//! seam (`meter::anthropic::replay_anthropic_capsule`) and the durable evidence
//! substrate (`store::meter_evidence`, closed under `aub-sth.7`) together, so
//! what is proven here is the pipeline, not any one module in isolation.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_usage_book::domain::ids::{AdapterVersion, MeterSemanticsId, ProviderContractId};
use agent_usage_book::domain::time::{
    FakeClock, MeasurementBasis, MonotonicDuration, UtcTimestamp,
};
use agent_usage_book::meter::adapter::{
    CredentialHandle, HttpTransport, MeterRequest, ProviderAdapter, ProviderObservation,
};
use agent_usage_book::meter::anthropic::AnthropicAdapter;
use agent_usage_book::meter::evidence::{
    SensitiveResponseMaterial, capture_json_body, quota_response_from_capsule,
};
use agent_usage_book::meter::transport::{CommandBudget, HttpRequest, HttpResponse};
use agent_usage_book::store::account::observe_account;
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::meter_attempt::{DueReason, NewMeterAttempt, start_meter_attempt};
use agent_usage_book::store::meter_evidence::{
    NewMeterObservation, NewMeterResponseEvidence, content_hash_of, current_observation_id,
    evidence_by_row_id, insert_observation, insert_response_evidence, observation_by_row_id,
    switch_current_observation,
};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use agent_usage_book::store::sample_run::{Trigger, start_sample_run};
use agent_usage_book::store::sampling_policy_snapshot::{
    ResolvedSamplingPolicy, resolve_policy_snapshot,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestDb {
    path: PathBuf,
}

impl TestDb {
    fn new() -> Self {
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aub-test-evidence-capsule-{}-{count}.sqlite3",
            std::process::id()
        ));
        Self { path }
    }

    fn open(&self) -> rusqlite::Connection {
        let policy = PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(5000),
        };
        let mut conn = open(&self.path, AccessMode::ReadWrite, &policy).unwrap();
        run_migrations(
            &mut conn,
            &registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .unwrap();
        conn
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

const POLICY: ResolvedSamplingPolicy = ResolvedSamplingPolicy {
    ordinary_cadence: MonotonicDuration::from_millis(300_000),
    freshness_horizon: MonotonicDuration::from_millis(900_000),
    reset_edge_policy: String::new(),
    retry_backoff_policy: String::new(),
    command_budget: MonotonicDuration::from_millis(60_000),
    policy_algorithm_version: String::new(),
};

/// A migrated connection holding one account, one sample run, one policy
/// snapshot and one started attempt: everything an evidence row needs to
/// reference before it can insert.
fn fixture_attempt(
    conn: &rusqlite::Connection,
) -> agent_usage_book::store::meter_attempt::MeterAttemptRowId {
    let account = observe_account(
        conn,
        "anthropic",
        "test-account",
        UtcTimestamp::from_unix_nanos(10_000),
    )
    .expect("fixture account must insert");
    let run = start_sample_run(
        conn,
        Trigger::Manual,
        UtcTimestamp::from_unix_nanos(10_000),
        "test",
    )
    .expect("fixture sample run must insert");
    let snapshot = resolve_policy_snapshot(
        conn,
        account,
        UtcTimestamp::from_unix_nanos(10_000),
        &POLICY,
    )
    .expect("fixture policy snapshot must insert");
    start_meter_attempt(
        conn,
        &NewMeterAttempt {
            run_id: run,
            account_id: account,
            provider: "anthropic".into(),
            request_started_at: UtcTimestamp::from_unix_nanos(20_000),
            credential_context_id: Some("ctx-1".into()),
            policy_snapshot_id: snapshot,
            due_at: UtcTimestamp::from_unix_nanos(19_000),
            due_reason: DueReason::OrdinaryCadence,
            due_basis: None,
            provider_contract_id: "anthropic-oauth-usage-v1".into(),
            meter_semantics_id: "anthropic-subscription-v1".into(),
        },
    )
    .expect("fixture attempt must insert")
}

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_fixture(name: &str) -> Vec<u8> {
    let path = crate_root()
        .join("tests/fixtures/meter/anthropic")
        .join(name);
    std::fs::read(&path)
        .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", path.display()))
}

struct MockTransport {
    response: HttpResponse,
}

impl HttpTransport for MockTransport {
    fn send(
        &self,
        _request: &HttpRequest,
        _budget: &CommandBudget,
        _clock: &impl agent_usage_book::domain::time::Clock,
    ) -> Result<HttpResponse, agent_usage_book::domain::failure::FailureClass> {
        Ok(self.response.clone())
    }
}

fn test_clock() -> FakeClock {
    FakeClock::new(UtcTimestamp::from_unix_nanos(1_700_000_000_000_000_000))
}

fn test_credential() -> CredentialHandle {
    CredentialHandle::new("test-oauth-token")
}

/// A response whose schema failed (a required field absent from otherwise
/// valid JSON) still produces a capsule with whatever is safely sanitizable,
/// plus the body hash: the capsule is built from JSON syntax alone and knows
/// nothing about the adapter's own field requirements.
#[test]
fn schema_failure_still_produces_a_capsule_with_the_sanitizable_remainder_and_body_hash() {
    let body = read_fixture("missing-field.json");
    let adapter = AnthropicAdapter::new();
    let transport = MockTransport {
        response: HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: body.clone(),
        },
    };

    let captured = adapter.observe_with_evidence(
        &test_credential(),
        &MeterRequest::default(),
        &transport,
        &test_clock(),
    );

    assert_eq!(
        captured.observation,
        ProviderObservation::Unreachable(
            agent_usage_book::domain::failure::FailureClass::MissingRequiredField
        )
    );

    let capsule = captured
        .evidence
        .expect("a schema failure still carries an evidence capsule");
    // The field that IS present survives sanitization; only the required
    // "five_hour.utilization" is missing from the fixture.
    assert!(
        capsule.serialized().contains("seven_day"),
        "the safely sanitizable remainder must be retained: {}",
        capsule.serialized()
    );
    let expected_hash = {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(&body);
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    assert_eq!(capsule.body_hash(), expected_hash);

    // The bounded failure-path store this bead's caller feeds gets the
    // sanitized body, not the raw one.
    let failed_body = captured
        .failed_body
        .expect("a schema failure retains the sanitized body for diagnosis");
    assert!(
        String::from_utf8(failed_body)
            .unwrap()
            .contains("seven_day")
    );
}

/// Capsule schema version, sanitizer version, content hash and truncation
/// status all persist through the evidence table, in both the ordinary and
/// the truncated case.
#[test]
fn persisted_capsule_carries_schema_version_sanitizer_version_hash_and_truncation_status() {
    let db = TestDb::new();
    let conn = db.open();
    let attempt = fixture_attempt(&conn);

    let body = read_fixture("valid-success.json");
    let capsule = capture_json_body(&body, &SensitiveResponseMaterial::default());
    assert!(!capsule.capture_truncated());

    let evidence_id = insert_response_evidence(
        &conn,
        &NewMeterResponseEvidence {
            attempt_id: attempt,
            response_classification: "200".into(),
            received_at: UtcTimestamp::from_unix_nanos(30_000),
            provider_observed_at_original: None,
            evidence_capsule: capsule.serialized().to_owned(),
            capsule_schema_version: capsule.schema_version().to_owned(),
            sanitizer_version: capsule.sanitizer_version().to_owned(),
            capture_truncated: capsule.capture_truncated(),
        },
    )
    .expect("the evidence must insert");

    let stored = evidence_by_row_id(&conn, evidence_id)
        .expect("the evidence must read")
        .expect("the evidence must exist");
    assert_eq!(stored.capsule_schema_version, capsule.schema_version());
    assert_eq!(stored.sanitizer_version, capsule.sanitizer_version());
    assert_eq!(stored.content_hash, content_hash_of(capsule.serialized()));
    assert!(!stored.capture_truncated);

    // A capsule large enough to trip the byte ceiling persists with
    // capture_truncated = true and the minimal, hash-only body.
    let oversized_value = "x".repeat(300 * 1024);
    let oversized_body =
        serde_json::to_vec(&serde_json::json!({ "padding": oversized_value })).unwrap();
    let oversized_capsule =
        capture_json_body(&oversized_body, &SensitiveResponseMaterial::default());
    assert!(oversized_capsule.capture_truncated());

    let truncated_evidence_id = insert_response_evidence(
        &conn,
        &NewMeterResponseEvidence {
            attempt_id: attempt,
            response_classification: "200".into(),
            received_at: UtcTimestamp::from_unix_nanos(31_000),
            provider_observed_at_original: None,
            evidence_capsule: oversized_capsule.serialized().to_owned(),
            capsule_schema_version: oversized_capsule.schema_version().to_owned(),
            sanitizer_version: oversized_capsule.sanitizer_version().to_owned(),
            capture_truncated: oversized_capsule.capture_truncated(),
        },
    )
    .expect("the oversized evidence must insert");
    let stored_truncated = evidence_by_row_id(&conn, truncated_evidence_id)
        .expect("the truncated evidence must read")
        .expect("the truncated evidence must exist");
    assert!(stored_truncated.capture_truncated);
    assert_eq!(
        stored_truncated.content_hash,
        content_hash_of(oversized_capsule.serialized())
    );
}

/// Raw body capture is disabled by default: a successful, schema-conformant
/// response carries no failure-path body at all, only the sanitized capsule.
#[test]
fn raw_body_capture_is_disabled_by_default() {
    let body = read_fixture("valid-success.json");
    let adapter = AnthropicAdapter::new();
    let transport = MockTransport {
        response: HttpResponse {
            status: 200,
            headers: Vec::new(),
            body,
        },
    };

    let captured = adapter.observe_with_evidence(
        &test_credential(),
        &MeterRequest::default(),
        &transport,
        &test_clock(),
    );

    assert!(matches!(
        captured.observation,
        ProviderObservation::Measured(_)
    ));
    assert!(
        captured.failed_body.is_none(),
        "a successful response must carry no body outside the sanitized capsule"
    );
}

/// The Done-when: replaying a stored capsule through a corrected adapter
/// produces a new interpretation, with the original interpretation still
/// readable. "Corrected" is simulated by re-deriving a second, distinguishable
/// interpretation from the same stored evidence and never from a fresh
/// network fetch, which is exactly the property this bead exists to prove.
#[test]
fn replaying_a_stored_capsule_produces_a_new_interpretation_with_the_original_still_readable() {
    let db = TestDb::new();
    let conn = db.open();
    let attempt = fixture_attempt(&conn);
    let request = MeterRequest::default();

    let body = read_fixture("valid-success.json");
    let capsule = capture_json_body(&body, &SensitiveResponseMaterial::default());
    let evidence_id = insert_response_evidence(
        &conn,
        &NewMeterResponseEvidence {
            attempt_id: attempt,
            response_classification: "200".into(),
            received_at: UtcTimestamp::from_unix_nanos(30_000),
            provider_observed_at_original: None,
            evidence_capsule: capsule.serialized().to_owned(),
            capsule_schema_version: capsule.schema_version().to_owned(),
            sanitizer_version: capsule.sanitizer_version().to_owned(),
            capture_truncated: capsule.capture_truncated(),
        },
    )
    .expect("the evidence must insert");

    // The original body is gone; every subsequent read comes only from the
    // stored capsule text, exactly as a replay after the sampling moment
    // has passed would see it.
    let stored = evidence_by_row_id(&conn, evidence_id)
        .expect("the evidence must read")
        .expect("the evidence must exist");

    let v1_reading = agent_usage_book::meter::anthropic::replay_anthropic_capsule(
        &stored.evidence_capsule,
        &request,
    )
    .expect("v1 must interpret the stored capsule");
    let v1_fingerprint = fingerprint_of(&v1_reading);
    let account = observe_account(
        &conn,
        "anthropic",
        "test-account",
        UtcTimestamp::from_unix_nanos(10_000),
    )
    .expect("account must already exist and read back idempotently");
    let semantics = MeterSemanticsId::new("anthropic-subscription-v1");
    let v1_obs = insert_observation(
        &conn,
        &NewMeterObservation {
            attempt_id: attempt,
            evidence_id,
            account_id: account,
            provider: "anthropic".into(),
            provider_observed_at: None,
            received_at: UtcTimestamp::from_unix_nanos(31_000),
            measurement_basis: MeasurementBasis::LocallyReceived,
            observed_plan: None,
            observed_tier: None,
            adapter_version: AdapterVersion::new("adapter-v1"),
            provider_contract_id: ProviderContractId::new("anthropic-oauth-usage-v1"),
            meter_semantics_id: semantics.clone(),
            normalized_fingerprint: v1_fingerprint.clone(),
        },
    )
    .expect("v1 interpretation must insert");

    // A corrected adapter reinterprets the SAME stored evidence: replay
    // reads only `stored.evidence_capsule`, never the original response.
    let corrected_value =
        quota_response_from_capsule(&stored.evidence_capsule).expect("capsule must reparse");
    let v2_fingerprint = format!(
        "corrected:{}",
        corrected_value
            .get("five_hour")
            .and_then(|w| w.get("utilization"))
            .expect("five_hour.utilization must be present in the retained subtree")
    );
    assert_ne!(
        v1_fingerprint, v2_fingerprint,
        "the corrected interpretation must be distinguishable from the original"
    );
    let v2_obs = insert_observation(
        &conn,
        &NewMeterObservation {
            attempt_id: attempt,
            evidence_id,
            account_id: account,
            provider: "anthropic".into(),
            provider_observed_at: None,
            received_at: UtcTimestamp::from_unix_nanos(32_000),
            measurement_basis: MeasurementBasis::LocallyReceived,
            observed_plan: None,
            observed_tier: None,
            adapter_version: AdapterVersion::new("adapter-v2-corrected"),
            provider_contract_id: ProviderContractId::new("anthropic-oauth-usage-v1"),
            meter_semantics_id: semantics.clone(),
            normalized_fingerprint: v2_fingerprint.clone(),
        },
    )
    .expect("v2 interpretation must insert");

    switch_current_observation(&conn, evidence_id, &semantics, v2_obs)
        .expect("switching the current interpretation must succeed");

    // Both interpretations remain readable, unchanged, from the one evidence
    // row: the original is not overwritten by the correction.
    let stored_v1 = observation_by_row_id(&conn, v1_obs)
        .expect("v1 must read")
        .expect("v1 must still exist");
    let stored_v2 = observation_by_row_id(&conn, v2_obs)
        .expect("v2 must read")
        .expect("v2 must exist");
    assert_eq!(stored_v1.normalized_fingerprint, v1_fingerprint);
    assert_eq!(stored_v1.adapter_version.as_str(), "adapter-v1");
    assert_eq!(stored_v2.normalized_fingerprint, v2_fingerprint);
    assert_eq!(
        current_observation_id(&conn, evidence_id, &semantics).expect("the preference must read"),
        Some(v2_obs)
    );
}

/// A stand-in normalized fingerprint distinguishing one interpretation from
/// another, built from the reading's own window values rather than a literal.
fn fingerprint_of(reading: &agent_usage_book::meter::anthropic::AnthropicReading) -> String {
    reading
        .windows
        .iter()
        .map(|w| {
            format!(
                "{}={}",
                w.semantic_key().as_str(),
                w.quota_used().as_ppm().get()
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}
