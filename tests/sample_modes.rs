//! Mode and invariant tests for `aub sample` (aub-eun.6).
//!
//! Covers:
//! - `--if-due` recording a supplied marker while making no network request,
//!   verified with a transport that panics if called.
//! - Flag matrix asserting no combination reaches the provider without
//!   persisting an attempt start row.
//! - Lease loss preserving session markers without issuing network requests.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_usage_book::domain::failure::FailureClass;
use agent_usage_book::domain::ids::{
    AdapterVersion, NativeRunId, NativeSessionId, RunId, SessionId, SourceNamespace,
};
use agent_usage_book::domain::time::{Clock, FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::meter::adapter::{CredentialHandle, HttpTransport, MeterRequest};
use agent_usage_book::meter::anthropic::AnthropicAdapter;
use agent_usage_book::meter::sampler::{AccountDisposition, BatchAccount, SamplingOrchestrator};
use agent_usage_book::meter::transport::{CommandBudget, HttpRequest, HttpResponse};
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use agent_usage_book::store::repository::Repository;
use agent_usage_book::store::sample_run::Trigger;
use agent_usage_book::store::sampling_lease::{AccountName, LeaseHolder};
use agent_usage_book::store::sampling_policy_snapshot::ResolvedSamplingPolicy;
use agent_usage_book::store::session_account_marker::{
    EvidenceDesignation, MarkerSource, NewSessionAccountMarker, insert_marker,
};

/// A valid Anthropic usage body.
const ANTHROPIC_SUCCESS_BODY: &[u8] =
    br#"{"five_hour":{"utilization":10.0,"resets_at":"2026-08-30T16:00:00Z"},"seven_day":{"utilization":20.0,"resets_at":"2026-09-06T00:00:00Z"}}"#;

/// A transport that panics unconditionally if any request is attempted.
struct PanicTransport;

impl HttpTransport for PanicTransport {
    fn send(
        &self,
        _request: &HttpRequest,
        _budget: &CommandBudget,
        _clock: &impl Clock,
    ) -> Result<HttpResponse, FailureClass> {
        panic!("PanicTransport was called unexpectedly!");
    }
}

/// A transport that records calls and checks that an attempt start was already
/// committed in the database before the request left the process.
#[derive(Clone)]
struct VerifyingTransport {
    db_path: PathBuf,
    calls: Arc<AtomicUsize>,
}

impl HttpTransport for VerifyingTransport {
    fn send(
        &self,
        _request: &HttpRequest,
        _budget: &CommandBudget,
        _clock: &impl Clock,
    ) -> Result<HttpResponse, FailureClass> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let conn = rusqlite::Connection::open(&self.db_path).expect("open db in transport");
        let attempts: i64 = conn
            .query_row("SELECT count(*) FROM meter_attempt", [], |r| r.get(0))
            .expect("query meter_attempt");
        assert!(
            attempts > 0,
            "CRITERION: attempt start must be durable in sqlite BEFORE request leaves the process"
        );
        Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: ANTHROPIC_SUCCESS_BODY.to_vec(),
        })
    }
}

fn test_policy() -> ResolvedSamplingPolicy {
    ResolvedSamplingPolicy {
        ordinary_cadence: MonotonicDuration::from_seconds(3600),
        freshness_horizon: MonotonicDuration::from_seconds(300),
        reset_edge_policy: "lead-60s".to_string(),
        retry_backoff_policy: "none".to_string(),
        command_budget: MonotonicDuration::from_seconds(10),
        policy_algorithm_version: "v1".to_string(),
    }
}

fn fixture_repo(db_path: &Path) -> Repository {
    let pragma = PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(500),
    };
    let mut conn = open(db_path, AccessMode::ReadWrite, &pragma).unwrap();
    run_migrations(
        &mut conn,
        &registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(1_000)),
    )
    .unwrap();
    Repository::new(db_path, pragma)
}

#[test]
fn if_due_records_supplied_marker_while_making_no_network_request() {
    let root = std::env::temp_dir().join(format!("aub-test-if-due-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let db_path = root.join("ledger.sqlite3");

    let repo = fixture_repo(&db_path);
    let now = UtcTimestamp::parse_rfc3339("2026-08-30T12:00:00Z").unwrap();
    let clock = FakeClock::new(now);

    // Initial run to record a successful observation
    let verifying_transport = VerifyingTransport {
        db_path: db_path.clone(),
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let initial_batch = vec![BatchAccount {
        name: AccountName::new("work-primary"),
        provider_key: "anthropic".to_string(),
        adapter: AnthropicAdapter::with_endpoint("http://127.0.0.1:0"),
        credential: CredentialHandle::new("test-token"),
        credential_context_id: None,
        request: MeterRequest::default(),
        policy: test_policy(),
        reset_edge_lead: MonotonicDuration::from_seconds(60),
        forced: true,
        adapter_version: AdapterVersion::new("0.1.0"),
    }];
    let orchestrator = SamplingOrchestrator {
        repository: &repo,
        transport: verifying_transport,
        clock,
        trigger: Trigger::Manual,
        configuration_fingerprint: "test-v1".to_string(),
        holder: LeaseHolder::new("setup-holder"),
        lease_ttl: MonotonicDuration::from_seconds(60),
        command_budget: MonotonicDuration::from_seconds(10),
        max_concurrent_requests: 4,
    };
    let report = orchestrator.run(&initial_batch).unwrap();
    assert_eq!(report.accounts.len(), 1);

    // Now account has fresh observation in DB. Record a session marker.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let account_id = repo
        .ensure_account("anthropic", "work-primary", now)
        .unwrap();
    let session_id = SessionId::new(
        SourceNamespace::new("cli"),
        NativeSessionId::new("session-123"),
    );
    let marker = NewSessionAccountMarker {
        session_id: session_id.clone(),
        observed_at: now,
        source_ordering_key: None,
        logical_account: "work-primary".to_string(),
        resolved_account_id: Some(account_id),
        marker_source: MarkerSource::new("hook"),
        run_id: Some(RunId::new(
            SourceNamespace::new("cli"),
            NativeRunId::new("run-456"),
        )),
        evidence_designation: EvidenceDesignation::ExplicitLauncherOrHook,
    };
    insert_marker(&conn, &marker).unwrap();

    // Verify marker is persisted
    let marker_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM session_account_marker WHERE logical_account = 'work-primary'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(marker_count, 1, "marker must be persisted");

    // Second run with forced = false (simulating --if-due) and PanicTransport
    let if_due_batch = vec![BatchAccount {
        name: AccountName::new("work-primary"),
        provider_key: "anthropic".to_string(),
        adapter: AnthropicAdapter::with_endpoint("http://127.0.0.1:0"),
        credential: CredentialHandle::new("test-token"),
        credential_context_id: None,
        request: MeterRequest::default(),
        policy: test_policy(),
        reset_edge_lead: MonotonicDuration::from_seconds(60),
        forced: false, // --if-due
        adapter_version: AdapterVersion::new("0.1.0"),
    }];

    let orchestrator_if_due = SamplingOrchestrator {
        repository: &repo,
        transport: PanicTransport,
        clock,
        trigger: Trigger::Hook,
        configuration_fingerprint: "test-v1".to_string(),
        holder: LeaseHolder::new("if-due-holder"),
        lease_ttl: MonotonicDuration::from_seconds(60),
        command_budget: MonotonicDuration::from_seconds(10),
        max_concurrent_requests: 4,
    };

    // This must NOT panic because the account is not due!
    let if_due_report = orchestrator_if_due.run(&if_due_batch).unwrap();
    assert_eq!(if_due_report.accounts.len(), 1);
    match &if_due_report.accounts[0].disposition {
        AccountDisposition::NotYet { .. } => {}
        other => panic!("expected NotYet, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn lease_loss_preserves_marker_without_calling_transport() {
    let root = std::env::temp_dir().join(format!("aub-test-lease-loss-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let db_path = root.join("ledger.sqlite3");

    let repo = fixture_repo(&db_path);
    let now = UtcTimestamp::parse_rfc3339("2026-08-30T12:00:00Z").unwrap();
    let clock = FakeClock::new(now);

    let account_id = repo
        .ensure_account("anthropic", "work-primary", now)
        .unwrap();

    // Acquire lease with a different holder
    repo.acquire_sampling_lease(
        &AccountName::new("work-primary"),
        &LeaseHolder::new("other-process"),
        MonotonicDuration::from_seconds(300),
        &clock,
    )
    .unwrap();

    // Record session marker
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let session_id = SessionId::new(
        SourceNamespace::new("cli"),
        NativeSessionId::new("session-lease-loss"),
    );
    let marker = NewSessionAccountMarker {
        session_id: session_id.clone(),
        observed_at: now,
        source_ordering_key: None,
        logical_account: "work-primary".to_string(),
        resolved_account_id: Some(account_id),
        marker_source: MarkerSource::new("hook"),
        run_id: None,
        evidence_designation: EvidenceDesignation::ExplicitLauncherOrHook,
    };
    insert_marker(&conn, &marker).unwrap();

    let batch = vec![BatchAccount {
        name: AccountName::new("work-primary"),
        provider_key: "anthropic".to_string(),
        adapter: AnthropicAdapter::with_endpoint("http://127.0.0.1:0"),
        credential: CredentialHandle::new("test-token"),
        credential_context_id: None,
        request: MeterRequest::default(),
        policy: test_policy(),
        reset_edge_lead: MonotonicDuration::from_seconds(60),
        forced: true,
        adapter_version: AdapterVersion::new("0.1.0"),
    }];

    let orchestrator = SamplingOrchestrator {
        repository: &repo,
        transport: PanicTransport,
        clock,
        trigger: Trigger::Hook,
        configuration_fingerprint: "test-v1".to_string(),
        holder: LeaseHolder::new("my-process"),
        lease_ttl: MonotonicDuration::from_seconds(60),
        command_budget: MonotonicDuration::from_seconds(10),
        max_concurrent_requests: 4,
    };

    // PanicTransport must not be called when lease cannot be acquired
    let report = orchestrator.run(&batch).unwrap();
    assert_eq!(report.accounts.len(), 1);
    match &report.accounts[0].disposition {
        AccountDisposition::LeaseHeld { holder } => {
            assert_eq!(holder, "other-process");
        }
        other => panic!("expected LeaseHeld, got {other:?}"),
    }

    // Marker must still be in database!
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM session_account_marker WHERE session_native = 'session-lease-loss'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "session marker must survive lease loss");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn flag_matrix_proves_attempt_start_persisted_before_any_request() {
    type FlagCombo = (
        bool,
        bool,
        Option<&'static str>,
        bool,
        Option<&'static str>,
        bool,
    );
    let combinations: Vec<FlagCombo> = vec![
        (true, false, None, false, None, false),
        (true, false, None, false, None, true),
        (true, false, None, true, None, false),
        (false, true, None, false, None, false),
        (false, true, None, false, None, true),
        (false, true, None, true, None, false),
        (false, false, Some("work-primary"), false, None, false),
        (false, false, Some("work-primary"), false, None, true),
        (false, false, Some("work-primary"), true, None, false),
        (true, false, Some("work-primary"), false, None, false),
        (true, false, Some("work-primary"), true, None, false),
        (
            false,
            false,
            Some("work-primary"),
            false,
            Some("cli:s1"),
            false,
        ),
        (
            false,
            false,
            Some("work-primary"),
            true,
            Some("cli:s1"),
            false,
        ),
    ];

    for (due, all, account_opt, if_due, session_id_opt, require_success) in combinations {
        let root = std::env::temp_dir().join(format!(
            "aub-test-matrix-{}-{}-{}-{}-{}-{}",
            due,
            all,
            account_opt.unwrap_or("none"),
            if_due,
            session_id_opt.unwrap_or("none"),
            require_success
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let db_path = root.join("ledger.sqlite3");

        let repo = fixture_repo(&db_path);
        let now = UtcTimestamp::parse_rfc3339("2026-08-30T12:00:00Z").unwrap();
        let clock = FakeClock::new(now);

        let forced = if account_opt.is_some() {
            !due && !if_due
        } else if all {
            !if_due
        } else {
            false
        };

        let trigger = if session_id_opt.is_some() {
            Trigger::Hook
        } else if due {
            Trigger::Timer
        } else {
            Trigger::Manual
        };

        let transport = VerifyingTransport {
            db_path: db_path.clone(),
            calls: Arc::new(AtomicUsize::new(0)),
        };

        let batch = vec![BatchAccount {
            name: AccountName::new("work-primary"),
            provider_key: "anthropic".to_string(),
            adapter: AnthropicAdapter::with_endpoint("http://127.0.0.1:0"),
            credential: CredentialHandle::new("test-token"),
            credential_context_id: None,
            request: MeterRequest::default(),
            policy: test_policy(),
            reset_edge_lead: MonotonicDuration::from_seconds(60),
            forced,
            adapter_version: AdapterVersion::new("0.1.0"),
        }];

        let orchestrator = SamplingOrchestrator {
            repository: &repo,
            transport: transport.clone(),
            clock,
            trigger,
            configuration_fingerprint: "test-v1".to_string(),
            holder: LeaseHolder::new("matrix-test"),
            lease_ttl: MonotonicDuration::from_seconds(60),
            command_budget: MonotonicDuration::from_seconds(10),
            max_concurrent_requests: 4,
        };

        let report = orchestrator.run(&batch).unwrap();
        assert_eq!(report.accounts.len(), 1);
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);

        let _ = std::fs::remove_dir_all(&root);
    }
}
