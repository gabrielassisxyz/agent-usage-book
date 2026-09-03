//! The identity and privacy scan over a full sampling run (aub-n27.4).
//!
//! Runs a complete sampling round trip against a scripted transport standing
//! in for a real provider (the same shape `tests/evidence_capsule.rs` calls
//! synthetic), the way `aub sample` would when a network response carries a
//! real credential: the credential travels in the Authorization header, and
//! the provider even echoes it back in an unexpected response field, the
//! worst case a leak has. The evidence capsule, the database row it lands in
//! and the spooled durable copy are then scanned with the one forbidden-
//! pattern list every scan in this project reads
//! (`test_support::sanitization::matched_patterns`), so a pattern added
//! there protects this scan too.
//!
//! PLAN.md 3.4, 13.1, 34.29; AGENTS.md correctness invariant 4.

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
use agent_usage_book::meter::transport::{CommandBudget, HttpRequest, HttpResponse};
use agent_usage_book::store::account::observe_account;
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::meter_attempt::{DueReason, NewMeterAttempt, start_meter_attempt};
use agent_usage_book::store::meter_evidence::{
    NewMeterObservation, NewMeterResponseEvidence, insert_observation, insert_response_evidence,
};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use agent_usage_book::store::sample_run::{Trigger, start_sample_run};
use agent_usage_book::store::sampling_policy_snapshot::{
    ResolvedSamplingPolicy, resolve_policy_snapshot,
};
use agent_usage_book::store::spool::{PendingTerminalBundle, PendingWindow, spool_pending};
use test_support::sanitization::matched_patterns;

/// A credential shaped to trip the shared forbidden-pattern list on its own
/// (it carries the `ghp_` prefix), independent of the direct substring check
/// this test also runs. Deliberately not one of `sensitive_value`'s own
/// hardcoded fallback shapes ("bearer ", "basic ", "sk-ant-",
/// "session_token=" in `src/meter/evidence.rs`): the point of this test is
/// the adapter telling the sanitizer about the credential it already knows
/// (`SensitiveResponseMaterial::new([credential.expose(), token.as_str()])`),
/// and a credential shape the hardcoded fallback would also catch cannot
/// prove that path is the one doing the work.
const CREDENTIAL: &str = "ghp_privacyScanMustNeverLeak9f2cAAAA";

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh scratch directory under the system temp dir, removed on drop.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(label: &str) -> Self {
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aub-identity-privacy-scan-{label}-{}-{count}",
            std::process::id()
        ));
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

struct ScriptedTransport {
    response: HttpResponse,
}

impl HttpTransport for ScriptedTransport {
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

const POLICY: ResolvedSamplingPolicy = ResolvedSamplingPolicy {
    ordinary_cadence: MonotonicDuration::from_millis(300_000),
    freshness_horizon: MonotonicDuration::from_millis(900_000),
    reset_edge_policy: String::new(),
    retry_backoff_policy: String::new(),
    command_budget: MonotonicDuration::from_millis(60_000),
    policy_algorithm_version: String::new(),
};

/// A response body that parses as a valid quota reading and additionally
/// echoes the credential back in a field the schema does not expect: the
/// worst case a provider-side leak could take, and the one
/// `SensitiveResponseMaterial` (built from the credential the caller
/// already knows) exists to strip regardless of which field carries it.
fn response_body_echoing_the_credential(credential: &str) -> Vec<u8> {
    serde_json::json!({
        "five_hour": {"utilization": 8.0, "resets_at": "2026-08-30T19:00:00.000Z"},
        "seven_day": {"utilization": 91.0, "resets_at": "2026-09-06T12:00:00.000Z"},
        "debug_echo": credential,
    })
    .to_string()
    .into_bytes()
}

/// Every stored value, from every user table, as text: the SQL-level content
/// of the database rather than its raw file bytes. The file format's own
/// b-tree page headers and varints are arbitrary binary data, and scanning
/// them for a one-character pattern like `@` produces a match on structural
/// noise that has nothing to do with a stored value (confirmed while writing
/// this test: a raw-byte scan of an otherwise clean database failed on `@`
/// found in page-header bytes). Reading through SQL is what "no credential
/// bytes enter the database" actually means: the values a query returns.
fn every_stored_value_as_text(conn: &rusqlite::Connection) -> String {
    let mut tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    tables.sort();

    let mut dump = String::new();
    for table in tables {
        let column_count = conn
            .prepare(&format!("SELECT * FROM {table} LIMIT 0"))
            .unwrap()
            .column_count();
        let mut statement = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
        let mut rows = statement.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            for index in 0..column_count {
                match row.get_ref(index).unwrap() {
                    rusqlite::types::ValueRef::Null => {}
                    rusqlite::types::ValueRef::Integer(v) => dump.push_str(&v.to_string()),
                    rusqlite::types::ValueRef::Real(v) => dump.push_str(&v.to_string()),
                    rusqlite::types::ValueRef::Text(v) => {
                        dump.push_str(&String::from_utf8_lossy(v))
                    }
                    rusqlite::types::ValueRef::Blob(v) => {
                        dump.push_str(&String::from_utf8_lossy(v))
                    }
                }
                dump.push('\n');
            }
        }
    }
    dump
}

/// The full round trip: a scripted response carrying the credential in its
/// Authorization header and echoed into an unexpected body field, captured
/// through the real adapter's evidence pipeline, committed to a real SQLite
/// file, and spooled to a real pending file. Neither persisted surface may
/// contain the credential, nor any pattern from the shared forbidden list.
#[test]
fn a_full_sampling_run_leaves_no_credential_bytes_in_the_database_or_the_spool() {
    let scratch = ScratchDir::new("run");
    let db_path = scratch.path().join("state.db");
    let mut conn = open(
        &db_path,
        AccessMode::ReadWrite,
        &PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(5000),
        },
    )
    .unwrap();
    run_migrations(&mut conn, &registry(), None, &test_clock()).unwrap();

    let account = observe_account(
        &conn,
        "anthropic",
        "test-account",
        UtcTimestamp::from_unix_nanos(10_000),
    )
    .unwrap();
    let run = start_sample_run(
        &conn,
        Trigger::Manual,
        UtcTimestamp::from_unix_nanos(10_000),
        "identity-privacy-scan",
    )
    .unwrap();
    let snapshot = resolve_policy_snapshot(
        &conn,
        account,
        UtcTimestamp::from_unix_nanos(10_000),
        &POLICY,
    )
    .unwrap();
    let attempt = start_meter_attempt(
        &conn,
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
    .unwrap();

    // The credential travels only in the request the adapter builds and in
    // the scripted response the test hands back; the adapter under test
    // never sees a database or a spool directly.
    let adapter = AnthropicAdapter::new();
    let transport = ScriptedTransport {
        response: HttpResponse {
            status: 200,
            headers: vec![("Authorization".to_owned(), format!("Bearer {CREDENTIAL}"))],
            body: response_body_echoing_the_credential(CREDENTIAL),
        },
    };
    let captured = adapter.observe_with_evidence(
        &CredentialHandle::new(CREDENTIAL),
        &MeterRequest::default(),
        &transport,
        &test_clock(),
    );
    assert!(
        matches!(captured.observation, ProviderObservation::Measured(_)),
        "the scripted response must parse as a measured reading: {:?}",
        captured.observation
    );
    let capsule = captured
        .evidence
        .expect("a 200 response must carry an evidence capsule");

    // The capsule itself must already be clean before it ever reaches a
    // durable surface: the earliest point a leak could be caught.
    assert!(
        !capsule.serialized().contains(CREDENTIAL),
        "the evidence capsule must not carry the credential verbatim: {}",
        capsule.serialized()
    );
    assert!(
        matched_patterns(capsule.serialized()).is_empty(),
        "the evidence capsule matches forbidden patterns: {:?}",
        matched_patterns(capsule.serialized())
    );

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
    .unwrap();
    insert_observation(
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
            meter_semantics_id: MeterSemanticsId::new("anthropic-subscription-v1"),
            normalized_fingerprint: "fp-identity-privacy-scan".into(),
        },
    )
    .unwrap();

    // A durable spool copy of the same terminal bundle, the other surface
    // the outcome document names: the pending-record shape carries only
    // typed, flattened fields and the same already-sanitized capsule.
    let bundle = PendingTerminalBundle {
        attempt_id: attempt.value(),
        completed_at_nanos: 32_000,
        elapsed_nanos: 2_000,
        outcome: "success".into(),
        failure_class: None,
        retry_after_nanos: None,
        sanitized_error_classification: None,
        retry_index: None,
        clock_anomaly: false,
        response_classification: "200".into(),
        received_at_nanos: 30_000,
        provider_observed_at_original: None,
        evidence_capsule: capsule.serialized().to_owned(),
        capsule_schema_version: capsule.schema_version().to_owned(),
        sanitizer_version: capsule.sanitizer_version().to_owned(),
        capture_truncated: capsule.capture_truncated(),
        account_id: account.value(),
        provider: "anthropic".into(),
        provider_observed_at_nanos: None,
        measurement_basis: "locally_received".into(),
        observed_plan: None,
        observed_tier: None,
        adapter_version: "adapter-v1".into(),
        provider_contract_id: "anthropic-oauth-usage-v1".into(),
        meter_semantics_id: "anthropic-subscription-v1".into(),
        normalized_fingerprint: "fp-identity-privacy-scan-spool".into(),
        windows: Vec::<PendingWindow>::new(),
    };
    let spool_state_dir = scratch.path().join("spool-state");
    spool_pending(&spool_state_dir, &bundle).unwrap();
    let spool_file =
        agent_usage_book::store::spool::pending_file_path(&spool_state_dir, attempt.value());
    assert!(spool_file.exists(), "the pending record must be spooled");

    // Every stored value, read through SQL rather than the file's raw
    // bytes: see `every_stored_value_as_text` for why the file bytes
    // themselves are the wrong thing to scan.
    let db_text = every_stored_value_as_text(&conn);
    drop(conn);
    assert!(
        !db_text.contains(CREDENTIAL),
        "the database must not carry the credential verbatim"
    );
    let db_hits = matched_patterns(&db_text);
    assert!(
        db_hits.is_empty(),
        "the database matches forbidden patterns: {db_hits:?}"
    );

    let spool_bytes = std::fs::read(&spool_file).unwrap();
    let spool_text = String::from_utf8_lossy(&spool_bytes);
    assert!(
        !spool_text.contains(CREDENTIAL),
        "the spool must not carry the credential verbatim"
    );
    let spool_hits = matched_patterns(&spool_text);
    assert!(
        spool_hits.is_empty(),
        "the spool matches forbidden patterns: {spool_hits:?}"
    );
}
