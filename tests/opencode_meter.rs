//! The opencode meter adapter over the persisted ledger (aub-8hu3).
//!
//! The unit cases in `src/meter/opencode.rs` prove the parse and the request
//! shape; this file proves what sampling actually persists. One test owns the
//! bead's never-persisted rule at the storage layer: the evidence row written
//! for an opencode observation carries the raw state fields the reading was
//! derived from, and no byte of the cookie material anywhere in it.
//!
//! May not depend on:
//! - the fixture corpus or transcript modules
//! - presentation

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_usage_book::domain::time::{Clock, FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::meter::adapter::{
    CredentialHandle, HttpTransport, MeterRequest, ProviderAdapter, ProviderObservation,
};
use agent_usage_book::meter::opencode::OpenCodeAdapter;
use agent_usage_book::meter::transport::{CommandBudget, HttpRequest, HttpResponse};
use agent_usage_book::store::account::observe_account;
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::meter_attempt::{DueReason, NewMeterAttempt, start_meter_attempt};
use agent_usage_book::store::meter_evidence::{
    NewMeterResponseEvidence, evidence_by_row_id, insert_response_evidence,
};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use agent_usage_book::store::sample_run::{Trigger, start_sample_run};
use agent_usage_book::store::sampling_policy_snapshot::{
    ResolvedSamplingPolicy, resolve_policy_snapshot,
};

/// The fixture under test, shared with the unit cases and the e2e run.
const FIXTURE_VALID: &str = include_str!("fixtures/meter/opencode/valid.html");

/// Distinctive on purpose, so a grep matches nothing but a leak. The bare
/// cookie value, matching the credential material the adapter receives.
const FIXTURE_COOKIE: &str = "fixture-session-cookie-9f2c-not-a-real-value";

struct TestDb {
    path: PathBuf,
}

impl TestDb {
    fn new() -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aub-test-opencode-meter-{}-{count}.sqlite3",
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

/// A transport answering with one programmed response, so the adapter runs
/// its real observe path without a socket.
struct OneResponseTransport {
    response: HttpResponse,
}

impl HttpTransport for OneResponseTransport {
    fn send(
        &self,
        _request: &HttpRequest,
        _budget: &CommandBudget,
        _clock: &impl agent_usage_book::domain::time::Clock,
    ) -> Result<HttpResponse, agent_usage_book::domain::failure::FailureClass> {
        Ok(self.response.clone())
    }
}

/// A migrated ledger holding one opencode account, one sample run, one
/// policy snapshot and one started attempt: everything an evidence row needs
/// to reference before it can insert.
fn fixture_attempt(
    conn: &rusqlite::Connection,
) -> agent_usage_book::store::meter_attempt::MeterAttemptRowId {
    let account = observe_account(
        conn,
        "opencode",
        "opencode-primary",
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
            provider: "opencode".into(),
            request_started_at: UtcTimestamp::from_unix_nanos(20_000),
            credential_context_id: Some("ctx-1".into()),
            policy_snapshot_id: snapshot,
            due_at: UtcTimestamp::from_unix_nanos(19_000),
            due_reason: DueReason::OrdinaryCadence,
            due_basis: None,
            provider_contract_id: "opencode-go-workspace-page-v1".into(),
            meter_semantics_id: "opencode-go-subscription-v1".into(),
        },
    )
    .expect("fixture attempt must insert")
}

/// The persisted `meter_response_evidence` row for an opencode observation
/// holds the raw state fields and never the cookie material.
#[test]
fn the_persisted_evidence_row_holds_the_raw_state_and_never_the_cookie() {
    let db = TestDb::new();
    let conn = db.open();
    let attempt = fixture_attempt(&conn);

    let adapter = OpenCodeAdapter::new(None);
    let transport = OneResponseTransport {
        response: HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: FIXTURE_VALID.as_bytes().to_vec(),
        },
    };
    let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1_000_000_000));
    let request = MeterRequest {
        model: None,
        workspace_id: Some("wrk_2345ABCDEFGHJKLMNOPQRSTuvwx".to_string()),
    };
    let captured = adapter.observe_with_evidence(
        &CredentialHandle::new(FIXTURE_COOKIE),
        &request,
        &transport,
        &clock,
    );
    assert!(
        matches!(captured.observation, ProviderObservation::Measured(_)),
        "the valid fixture must measure: {:?}",
        captured.observation
    );
    let capsule = captured
        .evidence
        .expect("a measured page keeps its capsule");

    let evidence_id = insert_response_evidence(
        &conn,
        &NewMeterResponseEvidence {
            attempt_id: attempt,
            response_classification: "200".into(),
            received_at: clock.now(),
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
    // The raw provider facts ride in the capsule exactly as the markup
    // stated them, so the derived instants stay auditable beside them.
    for field in [
        "\"percent\":\"0\"",
        "\"percent\":\"35.5\"",
        "\"percent\":\"64.8\"",
    ] {
        assert!(
            stored.evidence_capsule.contains(field),
            "the raw {field} lexeme must persist: {}",
            stored.evidence_capsule
        );
    }
    for field in ["5 hours 0 minutes", "4 days 10 hours", "7 days 5 hours"] {
        assert!(
            stored.evidence_capsule.contains(field),
            "the raw reset text {field} must persist: {}",
            stored.evidence_capsule
        );
    }
    // The never-persisted rule: the fixture cookie string matches nothing in
    // the stored row, capsule or classification.
    assert!(
        !stored.evidence_capsule.contains(FIXTURE_COOKIE),
        "the cookie material must never persist in the evidence capsule"
    );
    assert!(
        !stored.response_classification.contains(FIXTURE_COOKIE)
            && !stored.capsule_schema_version.contains(FIXTURE_COOKIE)
            && !stored.sanitizer_version.contains(FIXTURE_COOKIE)
            && !stored.content_hash.contains(FIXTURE_COOKIE),
        "no column of the evidence row may carry the cookie material"
    );
}
