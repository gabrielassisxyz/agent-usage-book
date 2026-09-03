//! Integration tests for the sampling batch orchestrator (`aub-eun.3`) over
//! the real transport and the synthetic provider server: the mixed-outcome
//! batch cardinality, isolation of a hanging provider behind the command
//! budget, and the concurrency bound as the server itself records it.
//!
//! These tests drive the composed pieces the unit suite cannot reach: the
//! production `BlockingTransport` over real loopback sockets, the adapter's
//! own request shape, and the budget clipping that turns a provider hang into
//! a classified outcome. The in-process scripted transport in
//! `src/meter/sampler.rs` proves the orchestrator's logic; this file proves
//! the composition behaves the same way when the provider is a socket.
//!
//! May not depend on:
//! - the fixture corpus or transcript modules
//! - presentation

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_usage_book::domain::attempt::AttemptOutcome;
use agent_usage_book::domain::failure::FailureClass;
use agent_usage_book::domain::ids::AdapterVersion;
use agent_usage_book::domain::time::{MonotonicDuration, RealClock};
use agent_usage_book::meter::adapter::{CredentialHandle, MeterRequest};
use agent_usage_book::meter::anthropic::AnthropicAdapter;
use agent_usage_book::meter::sampler::{AccountDisposition, BatchAccount, SamplingOrchestrator};
use agent_usage_book::meter::transport::BlockingTransport;
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::ledger_generation;
use agent_usage_book::store::meter_attempt::{
    MeterAttemptRowId, attempt_by_row_id, count_attempts,
};
use agent_usage_book::store::meter_evidence::count_meter_observations;
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use agent_usage_book::store::repository::Repository;
use agent_usage_book::store::sample_run::{Trigger, count_sample_runs, sample_run_by_id};
use agent_usage_book::store::sampling_lease::{AccountName, LeaseHolder};
use agent_usage_book::store::sampling_policy_snapshot::ResolvedSamplingPolicy;
use test_support::{ScriptedOutcome, ScriptedResponseBody, SyntheticServer};

/// A valid Anthropic usage body, so the adapter measures a reading from it.
const ANTHROPIC_SUCCESS_BODY: &[u8] =
    br#"{"five_hour":{"utilization":10.0,"resets_at":"2026-01-01T00:00:00.000Z"},"seven_day":{"utilization":20.0,"resets_at":"2026-01-08T00:00:00.000Z"}}"#;

// --- fixture -----------------------------------------------------------------

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("aub-sampler-batch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("scratch dir must be creatable");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn busy_policy() -> PragmaPolicy {
    PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(2_000),
    }
}

/// A migrated ledger database in a scratch directory, and its repository.
fn fixture_repository(tag: &str) -> (ScratchDir, Repository) {
    let scratch = ScratchDir::new(tag);
    let database_path = scratch.path().join("ledger.db");
    let mut conn = open(&database_path, AccessMode::ReadWrite, &busy_policy()).unwrap();
    run_migrations(
        &mut conn,
        &registry(),
        None,
        &agent_usage_book::domain::time::FakeClock::new(
            agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(1_000),
        ),
    )
    .unwrap();
    drop(conn);
    (scratch, Repository::new(&database_path, busy_policy()))
}

fn batch_account(name: &str, endpoint: String) -> BatchAccount<AnthropicAdapter> {
    BatchAccount {
        name: AccountName::new(name),
        provider_key: "anthropic".to_string(),
        adapter: AnthropicAdapter::with_endpoint(endpoint),
        credential: CredentialHandle::new("test-token"),
        credential_context_id: Some("ctx-integration".to_string()),
        request: MeterRequest::default(),
        policy: ResolvedSamplingPolicy {
            ordinary_cadence: MonotonicDuration::from_seconds(300),
            freshness_horizon: MonotonicDuration::from_seconds(900),
            reset_edge_policy: "lead-120s".to_string(),
            retry_backoff_policy: "exponential-2-250ms".to_string(),
            command_budget: MonotonicDuration::from_seconds(30),
            policy_algorithm_version: "v1".to_string(),
        },
        reset_edge_lead: MonotonicDuration::from_seconds(120),
        forced: false,
        adapter_version: AdapterVersion::new("adapter-integration-v1"),
    }
}

fn success_server() -> SyntheticServer {
    SyntheticServer::start(vec![ScriptedOutcome::Success(
        ScriptedResponseBody::json_ok(ANTHROPIC_SUCCESS_BODY.to_vec()),
    )])
    .unwrap()
}

fn orchestrator<'a>(
    repository: &'a Repository,
    command_budget: MonotonicDuration,
    max_concurrent_requests: usize,
) -> SamplingOrchestrator<'a, BlockingTransport, RealClock> {
    SamplingOrchestrator {
        repository,
        transport: BlockingTransport,
        clock: RealClock::new(),
        trigger: Trigger::Manual,
        configuration_fingerprint: "integration-fixture".to_string(),
        holder: LeaseHolder::new("integration-test"),
        lease_ttl: MonotonicDuration::from_seconds(60),
        command_budget,
        max_concurrent_requests,
    }
}

/// Reads the recorded generation out of the published projection file.
fn published_generation(repository: &Repository) -> u64 {
    let path = repository.projection_path();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("the projection file must exist at {path:?}: {error}"));
    agent_usage_book::projection::recorded_generation(&text)
        .unwrap_or_else(|| panic!("the projection file must record a generation: {text}"))
}

fn database_generation(repository: &Repository) -> u64 {
    let conn = open(
        repository.database_path(),
        AccessMode::ReadOnly,
        &busy_policy(),
    )
    .unwrap();
    ledger_generation::current(&conn).unwrap().value()
}

/// The count of started attempts and terminal results for row ids `1..=n`,
/// read through the real store surface, asserting every one references `run`.
fn every_attempt_references_run(repository: &Repository, n: i64, run_id: i64) {
    let conn = open(
        repository.database_path(),
        AccessMode::ReadOnly,
        &busy_policy(),
    )
    .unwrap();
    for row in 1..=n {
        let stored = attempt_by_row_id(&conn, MeterAttemptRowId::new(row))
            .expect("the attempt read must succeed")
            .unwrap_or_else(|| panic!("attempt row {row} must exist"));
        assert_eq!(
            stored.run_id.value(),
            run_id,
            "attempt row {row} must reference the batch's sample run"
        );
    }
}

// --- the mixed-outcome batch ---------------------------------------------------

/// A batch with one success, one authentication failure and one timeout
/// persists three attempts, one observation, and publishes one projection.
/// Each outcome is served by its own provider endpoint, so the script each
/// server follows is independent of request interleaving.
#[test]
fn one_success_one_auth_failure_one_timeout_persists_three_attempts_one_observation_and_one_projection()
 {
    let (_scratch, repository) = fixture_repository("mixed");
    let mut measured = success_server();
    let mut refused = SyntheticServer::start(vec![ScriptedOutcome::Unauthorized401]).unwrap();
    let mut stalled = SyntheticServer::start(vec![ScriptedOutcome::HeadersThenStall {
        status: 200,
        headers: Vec::new(),
    }])
    .unwrap();

    let accounts = vec![
        batch_account("success", format!("{}/usage", measured.url())),
        batch_account("authfail", format!("{}/usage", refused.url())),
        batch_account("timeout", format!("{}/usage", stalled.url())),
    ];

    // An eight second budget: comfortably below the adapter's own ten second
    // read timeout, so the hang is still clipped by the budget rather than by
    // that timeout, which is the composed behaviour this test exists to
    // prove. The command budget covers stage 1 and 2 as well as the request
    // itself (it is the whole command's ceiling), and those stages commit
    // several `synchronous = FULL` writes before any request is issued; a
    // two second budget left that overhead no room to vary and made the
    // fast accounts spuriously expire too.
    let report = orchestrator(&repository, MonotonicDuration::from_seconds(8), 2)
        .run(&accounts)
        .expect("the batch must run");

    assert_eq!(report.accounts.len(), 3);
    let success = &report.accounts[0];
    let authfail = &report.accounts[1];
    let timeout = &report.accounts[2];
    match (
        &success.disposition,
        &authfail.disposition,
        &timeout.disposition,
    ) {
        (
            AccountDisposition::Sampled(sampled_success),
            AccountDisposition::Sampled(sampled_auth),
            AccountDisposition::Sampled(sampled_timeout),
        ) => {
            assert_eq!(sampled_success.outcome, AttemptOutcome::Success);
            assert!(
                sampled_success.observation_committed,
                "the success commits evidence, an observation and its windows"
            );
            assert_eq!(sampled_auth.outcome, AttemptOutcome::AuthRequired);
            assert!(
                !sampled_auth.observation_committed,
                "an authentication failure has no observation to commit"
            );
            assert_eq!(
                sampled_timeout.outcome,
                AttemptOutcome::Unreachable(FailureClass::TotalBudgetExpired),
                "the hang the budget expired on is reported as expired, not as its driver error"
            );
        }
        other => panic!("every account must sample, got {other:?}"),
    }

    // Cardinality: three attempts, three results, one observation, one run,
    // and every attempt references the batch's run.
    let conn = open(
        repository.database_path(),
        AccessMode::ReadOnly,
        &busy_policy(),
    )
    .unwrap();
    let (starts, results) = count_attempts(&conn).unwrap();
    assert_eq!(starts, 3, "three due accounts, three attempts");
    assert_eq!(
        results, 3,
        "every attempt reaches exactly one terminal result"
    );
    assert_eq!(
        count_meter_observations(&conn).unwrap(),
        1,
        "exactly one measured observation among the three attempts"
    );
    assert_eq!(count_sample_runs(&conn).unwrap(), 1, "one sample run row");
    drop(conn);
    every_attempt_references_run(&repository, 3, report.run_id.value());
    let run = sample_run_by_id(
        &open(
            repository.database_path(),
            AccessMode::ReadOnly,
            &busy_policy(),
        )
        .unwrap(),
        report.run_id,
    )
    .unwrap()
    .expect("the batch's run row exists");
    assert_eq!(run.trigger(), Trigger::Manual);

    // One published projection, recording exactly the committed generation.
    assert_eq!(
        published_generation(&repository),
        database_generation(&repository)
    );

    measured.stop();
    refused.stop();
    stalled.stop();
}

// --- the hanging provider ------------------------------------------------------

/// One provider hanging until the budget expires does not prevent another
/// account's successful observation from committing: the hanging account's
/// request is clipped by the command budget, its attempt records the
/// expiry, and the reachable account's observation is committed with its
/// windows regardless.
#[test]
fn a_provider_hanging_until_the_budget_expires_does_not_block_another_accounts_observation() {
    let (_scratch, repository) = fixture_repository("hang");
    let mut stalled = SyntheticServer::start(vec![ScriptedOutcome::HeadersThenStall {
        status: 200,
        headers: Vec::new(),
    }])
    .unwrap();
    let mut reachable = success_server();

    let accounts = vec![
        batch_account("hanging", format!("{}/usage", stalled.url())),
        batch_account("reachable", format!("{}/usage", reachable.url())),
    ];

    // Same eight second budget as the mixed-outcome test above, and for the
    // same reason: comfortable headroom below the adapter's ten second read
    // timeout, while giving stage 1 and 2's own writes room to vary.
    let report = orchestrator(&repository, MonotonicDuration::from_seconds(8), 2)
        .run(&accounts)
        .expect("the batch must run");

    let hanging_report = &report.accounts[0];
    let reachable_report = &report.accounts[1];
    match (&hanging_report.disposition, &reachable_report.disposition) {
        (
            AccountDisposition::Sampled(sampled_hanging),
            AccountDisposition::Sampled(sampled_reachable),
        ) => {
            assert_eq!(
                sampled_hanging.outcome,
                AttemptOutcome::Unreachable(FailureClass::TotalBudgetExpired)
            );
            assert_eq!(sampled_reachable.outcome, AttemptOutcome::Success);
            assert!(
                sampled_reachable.observation_committed,
                "the reachable account's observation commits while the other hangs"
            );
        }
        other => panic!("both accounts must sample, got {other:?}"),
    }

    let conn = open(
        repository.database_path(),
        AccessMode::ReadOnly,
        &busy_policy(),
    )
    .unwrap();
    let (starts, results) = count_attempts(&conn).unwrap();
    assert_eq!(
        starts, 2,
        "two due accounts, two attempts, whatever the outcomes"
    );
    assert_eq!(
        results, 2,
        "the hanging account's expiry is a terminal result too"
    );
    assert_eq!(
        count_meter_observations(&conn).unwrap(),
        1,
        "only the reachable account has an observation"
    );
    drop(conn);
    every_attempt_references_run(&repository, 2, report.run_id.value());
    assert_eq!(
        published_generation(&repository),
        database_generation(&repository)
    );

    stalled.stop();
    reachable.stop();
}

// --- bounded concurrency, as the server records it -----------------------------

/// Bounded concurrency respected, asserted by the synthetic server recording
/// no more than the configured number of simultaneous connections, and
/// reaching the bound rather than running one at a time: four accounts, a
/// bound of two, and responses slow enough that in-flight requests overlap.
#[test]
fn bounded_concurrency_is_recorded_by_the_synthetic_server() {
    let (_scratch, repository) = fixture_repository("bound");
    let script = vec![
        ScriptedOutcome::Success(ScriptedResponseBody::json_ok(
            ANTHROPIC_SUCCESS_BODY.to_vec()
        ));
        4
    ];
    let mut server =
        SyntheticServer::start_with_response_delay(script, Duration::from_millis(150)).unwrap();

    let accounts: Vec<BatchAccount<AnthropicAdapter>> = (0..4)
        .map(|index| batch_account(&format!("bound{index}"), format!("{}/usage", server.url())))
        .collect();

    let report = orchestrator(&repository, MonotonicDuration::from_seconds(30), 2)
        .run(&accounts)
        .expect("the batch must run");

    assert_eq!(
        report.workers_spawned, 2,
        "the bound, not the account count, sizes the pool"
    );
    assert_eq!(
        report.workers_completed, 2,
        "every worker finished before run returned"
    );
    for entry in &report.accounts {
        match &entry.disposition {
            AccountDisposition::Sampled(sampled) => {
                assert_eq!(sampled.outcome, AttemptOutcome::Success);
                assert!(sampled.observation_committed);
            }
            other => panic!("every account must sample, got {other:?}"),
        }
    }
    assert_eq!(
        server.request_count(),
        4,
        "every account's request reached the provider"
    );
    assert_eq!(
        server.max_simultaneous_connections(),
        2,
        "the provider saw the configured bound of simultaneous connections, reached and never exceeded"
    );

    server.stop();
}
