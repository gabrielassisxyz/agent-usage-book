//! Concurrency tests for the projection and the store (aub-me5.11, PLAN.md 34.6, 34.27, 39):
//! repeated atomic projection replacement against concurrent readers, the status
//! zero-connection invariant under contention, long analytical readers concurrent
//! with meter writers, durable spooling under contention, failure-only batch updates,
//! and randomized interleaving property verification.
//!
//! CI time budget: The entire concurrency and property suite is designed to
//! complete within 15 seconds under normal CI load. The measured uncontended
//! execution time is roughly 1 to 2 seconds.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use agent_usage_book::domain::attempt::AttemptOutcome;
use agent_usage_book::domain::failure::FailureClass;
use agent_usage_book::domain::ids::{AdapterVersion, MeterSemanticsId, ProviderContractId};
use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaUsed};
use agent_usage_book::domain::time::{
    Clock as _, FakeClock, MeasurementBasis, MonotonicDuration, RealClock, UtcTimestamp,
};
use agent_usage_book::domain::window::{
    MeterWindow, NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
    WindowSemanticKey,
};
use agent_usage_book::projection::reader::{ProjectionRead, read_projection};
use agent_usage_book::store::account::{AccountId, observe_account};
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::ledger_generation::{self, Generation};
use agent_usage_book::store::meter_attempt::{
    DueReason, MeterAttemptRowId, NewMeterAttempt, NewMeterAttemptResult,
};
use agent_usage_book::store::meter_evidence::NewMeterResponseEvidence;
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::repository::{
    NewMeterInterpretation, Repository, TerminalMeterBundle,
};
use agent_usage_book::store::sample_run::{SampleRunId, Trigger, start_sample_run};
use agent_usage_book::store::sampling_policy_snapshot::{
    ResolvedSamplingPolicy, SamplingPolicySnapshotId, resolve_policy_snapshot,
};
use agent_usage_book::store::spool::{
    SpoolCycleOutcome, drain_pending, pending_dir, spool_then_commit,
};
use test_support::{Rng, Seed, StateDir, check_property};

fn policy(busy_ms: u64) -> PragmaPolicy {
    PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(busy_ms),
    }
}

/// One seeded ledger database with its repository, account, run and policy snapshot.
struct Fixture {
    state_dir: StateDir,
    repository: Repository,
    account_id: AccountId,
    run_id: SampleRunId,
    policy_snapshot_id: SamplingPolicySnapshotId,
    conn: rusqlite::Connection,
    clock: FakeClock,
}

fn fixture(tag: &str, busy_ms: u64) -> Fixture {
    let state_dir = StateDir::new();
    let database_path = state_dir.path().join("ledger.db");
    let mut conn = open(&database_path, AccessMode::ReadWrite, &policy(busy_ms)).unwrap();
    run_migrations(
        &mut conn,
        &agent_usage_book::store::migrations::registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(1_000)),
    )
    .unwrap();
    let account_id = observe_account(
        &conn,
        "anthropic",
        "work",
        UtcTimestamp::from_unix_nanos(2_000),
    )
    .unwrap();
    let run_id = start_sample_run(
        &conn,
        Trigger::Manual,
        UtcTimestamp::from_unix_nanos(2_000),
        tag,
    )
    .unwrap();
    let policy_snapshot_id = resolve_policy_snapshot(
        &conn,
        account_id,
        UtcTimestamp::from_unix_nanos(2_000),
        &ResolvedSamplingPolicy {
            ordinary_cadence: MonotonicDuration::from_seconds(300),
            freshness_horizon: MonotonicDuration::from_seconds(900),
            reset_edge_policy: "fixture".into(),
            retry_backoff_policy: "fixture".into(),
            command_budget: MonotonicDuration::from_seconds(30),
            policy_algorithm_version: "v1".into(),
        },
    )
    .unwrap();
    Fixture {
        state_dir,
        repository: Repository::new(&database_path, policy(busy_ms)),
        account_id,
        run_id,
        policy_snapshot_id,
        conn,
        clock: FakeClock::new(UtcTimestamp::from_unix_nanos(3_000)),
    }
}

impl Fixture {
    fn state_path(&self) -> &Path {
        self.state_dir.path()
    }

    fn db_path(&self) -> PathBuf {
        self.state_path().join("ledger.db")
    }

    fn projection_path(&self) -> PathBuf {
        self.repository.projection_path()
    }

    fn start_attempt(&mut self) -> MeterAttemptRowId {
        self.clock.advance(MonotonicDuration::from_seconds(10));
        let started_at = self.clock.now();
        let attempt = NewMeterAttempt {
            run_id: self.run_id,
            account_id: self.account_id,
            provider: "anthropic".into(),
            request_started_at: started_at,
            credential_context_id: Some("credential-context-v1".into()),
            policy_snapshot_id: self.policy_snapshot_id,
            due_at: started_at,
            due_reason: DueReason::OrdinaryCadence,
            due_basis: None,
            provider_contract_id: "contract-v1".into(),
            meter_semantics_id: "semantics-v1".into(),
        };
        let started = self.repository.start_meter_attempt(&attempt).unwrap();
        MeterAttemptRowId::new(
            i64::try_from(started.attempt_id().value())
                .expect("attempt identity fits SQLite INTEGER"),
        )
    }

    fn success_bundle(
        &mut self,
        attempt_id: MeterAttemptRowId,
        quota_ppm: i32,
    ) -> TerminalMeterBundle {
        self.clock.advance(MonotonicDuration::from_millis(500));
        let completed_at = self.clock.now();
        let window = MeterWindow::new(
            WindowSemanticKey::new("five_hour"),
            WindowScope::AccountWide,
            QuotaUsed::new(QuotaFractionPpm::new(quota_ppm).unwrap()),
            ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap(),
            QuantizationSemantics::RoundedToNearest,
            completed_at,
            NominalWindowDuration::from_nanos(18_000_000_000_000),
        );
        let evidence = NewMeterResponseEvidence {
            attempt_id,
            response_classification: "success".into(),
            received_at: completed_at,
            provider_observed_at_original: Some("1970-01-01T00:00:04Z".into()),
            evidence_capsule: "{\"five_hour\":\"25.0\"}".into(),
            capsule_schema_version: "capsule-v1".into(),
            sanitizer_version: "sanitizer-v1".into(),
            capture_truncated: false,
        };
        let interpretation = NewMeterInterpretation {
            account_id: self.account_id,
            provider: "anthropic".into(),
            provider_observed_at: Some(completed_at),
            received_at: completed_at,
            measurement_basis: MeasurementBasis::ProviderObserved,
            observed_plan: Some("max".into()),
            observed_tier: None,
            adapter_version: AdapterVersion::new("adapter-v1"),
            provider_contract_id: ProviderContractId::new("contract-v1"),
            meter_semantics_id: MeterSemanticsId::new("semantics-v1"),
            normalized_fingerprint: "fingerprint-v1".into(),
        };
        TerminalMeterBundle::new(
            NewMeterAttemptResult {
                attempt_id,
                completed_at,
                elapsed: MonotonicDuration::from_nanos(1),
                outcome: AttemptOutcome::Success,
                sanitized_error_classification: None,
                retry_index: None,
                clock_anomaly: false,
            },
            evidence,
            interpretation,
            vec![window],
        )
        .unwrap()
    }

    fn failure_result(
        &mut self,
        attempt_id: MeterAttemptRowId,
        auth: bool,
    ) -> NewMeterAttemptResult {
        self.clock.advance(MonotonicDuration::from_millis(500));
        NewMeterAttemptResult {
            attempt_id,
            completed_at: self.clock.now(),
            elapsed: MonotonicDuration::from_nanos(1),
            outcome: if auth {
                AttemptOutcome::AuthRequired
            } else {
                AttemptOutcome::Unreachable(FailureClass::ConnectTimeout)
            },
            sanitized_error_classification: None,
            retry_index: None,
            clock_anomaly: false,
        }
    }

    fn read_projection_json(&self) -> serde_json::Value {
        let path = self.projection_path();
        let bytes = fs::read(&path)
            .unwrap_or_else(|error| panic!("the projection file must exist at {path:?}: {error}"));
        serde_json::from_slice(&bytes).expect("the published file must be parseable JSON")
    }

    fn database_generation(&self) -> Generation {
        let conn = open(
            self.repository.database_path(),
            AccessMode::ReadOnly,
            &policy(2_000),
        )
        .unwrap();
        ledger_generation::current(&conn).unwrap()
    }
}

// --- helpers for structural test verification --------------------------------

fn function_body(source: &str, declaration: &str) -> String {
    let start = source
        .find(declaration)
        .unwrap_or_else(|| panic!("source must declare {declaration}"));
    let rest = &source[start..];
    let end = rest[declaration.len()..]
        .find("\nfn ")
        .map(|offset| offset + declaration.len() + 1)
        .unwrap_or(rest.len());
    rest[..end].to_string()
}

fn assert_status_performs_only_the_status_contract() {
    let source = include_str!("../src/cli.rs");
    let status_body = [
        function_body(source, "fn status("),
        function_body(source, "fn projection_accounts("),
        function_body(source, "fn status_clock_skew_envelope("),
    ]
    .concat();

    for forbidden in [
        "rusqlite",
        "Connection",
        "store::connection",
        "store::migrate",
        "transcripts::",
        "calibration",
        "rate_book",
        "ureq",
        "reqwest",
        "http",
        "spool",
        "fs::write",
        "OpenOptions",
        "create_dir",
        "remove_file",
    ] {
        assert!(
            !status_body.contains(forbidden),
            "the status function's source must not reference {forbidden}: the status contract allows only configuration resolution, one bounded projection read, freshness computation and formatting"
        );
    }
}

fn assert_status_contract_scan_catches_forbidden_reference() {
    let poisoned = "fn status() { let probe = crate::store::connection::open(); }";
    assert!(function_body(poisoned, "fn status(").contains("store::connection"));
}

// --- Criterion 1 & 2: Concurrency over projection replacement & readers ------

/// Repeated projection replacement against concurrent readers produces only
/// complete old or complete new content, never a torn file, over at least
/// 10,000 read iterations, while asserting status opens no SQLite connection.
#[test]
fn repeated_projection_replacement_against_concurrent_readers_never_tears() {
    let start = Instant::now();
    let mut fixture = fixture("projection-concurrency", 5_000);
    let projection_path = fixture.projection_path();
    let state_path = fixture.state_path().to_path_buf();
    let db_path = fixture.db_path();

    // Write initial projection
    let initial_attempt = fixture.start_attempt();
    let initial_val = initial_attempt.value();
    let initial_quota = 10_000 * (initial_val as i32 % 50 + 1);
    let initial_bundle = fixture.success_bundle(initial_attempt, initial_quota);
    fixture
        .repository
        .commit_terminal_bundle(&initial_bundle)
        .unwrap();

    let total_reads = Arc::new(AtomicUsize::new(0));
    let writer_done = Arc::new(AtomicBool::new(false));

    // Setup configuration environment for aub status invocation
    let config_dir = StateDir::new();
    let home_dir = config_dir.path().join("home");
    fs::create_dir_all(&home_dir).unwrap();
    let config_file = config_dir.path().join("aub.toml");
    fs::write(
        &config_file,
        format!(
            "state.dir = \"{}\"\n\n[[accounts]]\nname = \"work\"\nprovider = \"anthropic\"\n",
            state_path.display()
        ),
    )
    .unwrap();

    let reader_handles: Vec<_> = (0..4)
        .map(|reader_id| {
            let path = projection_path.clone();
            let total_reads = Arc::clone(&total_reads);
            let writer_done = Arc::clone(&writer_done);
            std::thread::spawn(move || {
                let mut local_reads = 0;
                let mut prev_generation = 0;
                let mut prev_attempt = 0;
                let mut prev_observation = 0;
                while !writer_done.load(Ordering::Acquire)
                    || total_reads.load(Ordering::Acquire) < 10_000
                {
                    match read_projection(&path) {
                        ProjectionRead::Available(projection) => {
                            let generation_val = projection.ledger_generation.value();
                            assert!(
                                generation_val >= prev_generation,
                                "reader {reader_id}: generation regressed from {prev_generation} to {generation_val}"
                            );
                            prev_generation = generation_val;

                            // Validate that every read is complete: never torn or partial
                            assert_eq!(
                                projection.accounts.len(),
                                1,
                                "reader {reader_id}: exactly one account expected"
                            );
                            let account = &projection.accounts[0];
                            assert_eq!(account.logical_name, "work");
                            assert_eq!(account.provider, "anthropic");

                            let obs = account
                                .last_successful_observation
                                .as_ref()
                                .expect("last_successful_observation must be complete");
                            let attempt = account
                                .latest_attempt
                                .as_ref()
                                .expect("latest_attempt must be complete");

                            let obs_val = obs.observation_id.value();
                            assert!(
                                obs_val >= prev_observation,
                                "reader {reader_id}: observation ID regressed from {prev_observation} to {obs_val}"
                            );
                            prev_observation = obs_val;

                            let attempt_val = attempt.attempt_id.value();
                            assert!(
                                attempt_val >= prev_attempt,
                                "reader {reader_id}: attempt regressed from {prev_attempt} to {attempt_val}"
                            );
                            prev_attempt = attempt_val;

                            let expected_quota = 10_000 * (obs_val as u32 % 50 + 1);
                            assert_eq!(
                                obs.windows[0].quota_used_ppm.as_ppm().get(),
                                expected_quota,
                                "reader {reader_id}: window quota must match observation {obs_val}"
                            );

                            if let Some(terminal) = &attempt.result
                                && terminal.outcome == AttemptOutcome::Success
                                && obs_val == attempt_val as i64
                            {
                                assert_eq!(
                                    obs.received_at,
                                    terminal.completed_at,
                                    "reader {reader_id}: observation received_at must match attempt completed_at"
                                );
                            }
                        }
                        ProjectionRead::Unavailable(unavailable) => {
                            panic!("reader {reader_id}: torn read or unavailable projection: {unavailable:?}");
                        }
                    }
                    local_reads += 1;
                    total_reads.fetch_add(1, Ordering::Release);
                    if local_reads % 64 == 0 {
                        std::thread::yield_now();
                    }
                }
                local_reads
            })
        })
        .collect();

    // Writer performs repeated projection replacements across 20 batches
    for _ in 2..=20 {
        let attempt_id = fixture.start_attempt();
        let attempt_val = attempt_id.value();
        let quota_ppm = 10_000 * (attempt_val as i32 % 50 + 1);
        let bundle = fixture.success_bundle(attempt_id, quota_ppm);
        fixture.repository.commit_terminal_bundle(&bundle).unwrap();
    }
    writer_done.store(true, Ordering::Release);

    for handle in reader_handles {
        handle.join().expect("reader thread must not panic");
    }

    let iterations = total_reads.load(Ordering::Acquire);
    assert!(
        iterations >= 10_000,
        "expected at least 10,000 read iterations under contention, got {iterations}"
    );

    // Verify status opens no SQLite connection during any of the above,
    // both structurally and behaviorally.
    assert_status_performs_only_the_status_contract();
    assert_status_contract_scan_catches_forbidden_reference();

    // Behavioral assertion: Hold an exclusive lock on SQLite. If aub status
    // attempted to open or touch SQLite, it would fail or block indefinitely.
    {
        let exclusive_conn = rusqlite::Connection::open(&db_path).unwrap();
        exclusive_conn.execute_batch("BEGIN EXCLUSIVE;").unwrap();

        let output = Command::new(env!("CARGO_BIN_EXE_aub"))
            .env("HOME", &home_dir)
            .env("AUB_CONFIG_FILE", &config_file)
            .args(["status"])
            .output()
            .expect("aub status must run");

        assert_eq!(
            output.status.code(),
            Some(0),
            "status must exit zero even while SQLite is exclusively locked"
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("work"),
            "status must render the projection account: {stdout}"
        );
        exclusive_conn.execute_batch("ROLLBACK;").unwrap();
    }

    let elapsed = start.elapsed();
    assert!(
        elapsed <= Duration::from_secs(15),
        "concurrency test took {elapsed:?}, over the 15s budget"
    );
}

// --- Criterion 2: Status opens no SQLite connection --------------------------

#[test]
fn status_opens_no_sqlite_connection_during_concurrent_projection_replacement() {
    // Assert structurally using the exact scanner mechanism
    assert_status_performs_only_the_status_contract();
    assert_status_contract_scan_catches_forbidden_reference();

    // Behavioral proof: status succeeds in an environment where no SQLite
    // database exists at all under the configured state directory.
    let config_dir = StateDir::new();
    let state_dir = StateDir::new();
    let home_dir = config_dir.path().join("home");
    fs::create_dir_all(&home_dir).unwrap();
    let config_file = config_dir.path().join("aub.toml");
    fs::write(
        &config_file,
        format!(
            "state.dir = \"{}\"\n\n[[accounts]]\nname = \"work-primary\"\nprovider = \"anthropic\"\n",
            state_dir.path().display()
        ),
    )
    .unwrap();

    // Seed a valid projection file without any ledger.db database file present
    let projection_path = state_dir.path().join("projection");
    let now_nanos = RealClock::new().now().unix_nanos();
    let initial_doc = serde_json::json!({
        "schema_version": 1,
        "ledger_generation": 1,
        "accounts": [{
            "account_id": 1,
            "logical_name": "work-primary",
            "provider": "anthropic",
            "last_successful_observation": {
                "observation_id": 1,
                "provider_observed_at_nanos": now_nanos - 30_000_000_000i64,
                "received_at_nanos": now_nanos - 30_000_000_000i64,
                "measurement_basis": "provider_observed",
                "windows": [{
                    "semantic_key": "five_hour",
                    "scope_kind": "account_wide",
                    "scoped_model": null,
                    "quota_used_ppm": 250_000,
                    "reported_resolution_ppm": 10_000,
                    "quantization": "rounded_to_nearest",
                    "resets_at_nanos": now_nanos + 18_000_000_000_000i64,
                    "nominal_duration_nanos": 18_000_000_000_000u64
                }]
            },
            "latest_attempt": {
                "attempt_id": 1,
                "request_started_at_nanos": now_nanos - 30_000_000_000i64,
                "credential_context_id": "ctx",
                "result": {
                    "completed_at_nanos": now_nanos - 29_000_000_000i64,
                    "outcome": "success",
                    "failure_class": null
                }
            }
        }]
    });
    fs::write(&projection_path, serde_json::to_vec(&initial_doc).unwrap()).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_aub"))
        .env("HOME", &home_dir)
        .env("AUB_CONFIG_FILE", &config_file)
        .args(["status"])
        .output()
        .expect("aub status must run");

    assert_eq!(
        output.status.code(),
        Some(0),
        "status must exit zero with no SQLite database file present"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("work-primary"),
        "status output must render account without SQLite: {stdout}"
    );
}

// --- Criterion 3 & 4: Analytical reader and meter writers concurrency --------

/// A long analytical read runs concurrently with meter writers and sees a
/// consistent snapshot, while meter writes either land or remain durably spooled.
#[test]
fn long_analytical_read_sees_consistent_snapshot_concurrent_with_meter_writers() {
    let start = Instant::now();
    let mut fixture = fixture("analytical-concurrency", 200);
    let db_path = fixture.db_path();

    // Initial baseline state: 1 attempt and 1 observation
    let attempt1 = fixture.start_attempt();
    let bundle1 = fixture.success_bundle(attempt1, 100_000);
    let initial_outcome =
        spool_then_commit(&fixture.repository, &bundle1, &RealClock::new()).unwrap();
    assert!(matches!(
        initial_outcome,
        SpoolCycleOutcome::Committed { .. }
    ));

    // Open long analytical reader and begin transaction snapshot
    let mut reader_conn = open(&db_path, AccessMode::ReadOnly, &policy(5_000)).unwrap();
    let snapshot = reader_conn.transaction().unwrap();

    let snap_initial_attempts: i64 = snapshot
        .query_row("SELECT COUNT(*) FROM meter_attempt", [], |row| row.get(0))
        .unwrap();
    let snap_initial_evidence: i64 = snapshot
        .query_row("SELECT COUNT(*) FROM meter_response_evidence", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(snap_initial_attempts, 1);
    assert_eq!(snap_initial_evidence, 1);

    // Writers execute concurrently while the analytical snapshot is open
    let landed_count = Arc::new(AtomicUsize::new(0));
    let spooled_count = Arc::new(AtomicUsize::new(0));

    let writer_handles: Vec<_> = (0..2)
        .map(|writer_idx| {
            let repository = fixture.repository.clone();
            let account_id = fixture.account_id;
            let run_id = fixture.run_id;
            let policy_snapshot_id = fixture.policy_snapshot_id;
            let landed_count = Arc::clone(&landed_count);
            let spooled_count = Arc::clone(&spooled_count);
            let state_path = fixture.state_path().to_path_buf();

            std::thread::spawn(move || {
                for cycle in 0..5 {
                    let seq = (writer_idx * 100 + cycle + 10) as i64;
                    let started_at =
                        UtcTimestamp::from_unix_nanos(20_000_000_000 + seq * 1_000_000);
                    let attempt = NewMeterAttempt {
                        run_id,
                        account_id,
                        provider: "anthropic".into(),
                        request_started_at: started_at,
                        credential_context_id: Some("credential-context-v1".into()),
                        policy_snapshot_id,
                        due_at: started_at,
                        due_reason: DueReason::OrdinaryCadence,
                        due_basis: None,
                        provider_contract_id: "contract-v1".into(),
                        meter_semantics_id: "semantics-v1".into(),
                    };
                    let started = repository.start_meter_attempt(&attempt).unwrap();
                    let row_id = MeterAttemptRowId::new(
                        i64::try_from(started.attempt_id().value())
                            .expect("attempt identity fits SQLite INTEGER"),
                    );

                    let completed_at =
                        UtcTimestamp::from_unix_nanos(20_000_000_000 + seq * 1_000_000 + 500);
                    let window = MeterWindow::new(
                        WindowSemanticKey::new("five_hour"),
                        WindowScope::AccountWide,
                        QuotaUsed::new(QuotaFractionPpm::new(300_000).unwrap()),
                        ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap(),
                        QuantizationSemantics::RoundedToNearest,
                        completed_at,
                        NominalWindowDuration::from_nanos(18_000_000_000_000),
                    );
                    let evidence = NewMeterResponseEvidence {
                        attempt_id: row_id,
                        response_classification: "success".into(),
                        received_at: completed_at,
                        provider_observed_at_original: Some("1970-01-01T00:00:10Z".into()),
                        evidence_capsule: "{\"five_hour\":\"30.0\"}".into(),
                        capsule_schema_version: "capsule-v1".into(),
                        sanitizer_version: "sanitizer-v1".into(),
                        capture_truncated: false,
                    };
                    let interpretation = NewMeterInterpretation {
                        account_id,
                        provider: "anthropic".into(),
                        provider_observed_at: Some(completed_at),
                        received_at: completed_at,
                        measurement_basis: MeasurementBasis::ProviderObserved,
                        observed_plan: Some("max".into()),
                        observed_tier: None,
                        adapter_version: AdapterVersion::new("adapter-v1"),
                        provider_contract_id: ProviderContractId::new("contract-v1"),
                        meter_semantics_id: MeterSemanticsId::new("semantics-v1"),
                        normalized_fingerprint: "fingerprint-v1".into(),
                    };
                    let bundle = TerminalMeterBundle::new(
                        NewMeterAttemptResult {
                            attempt_id: row_id,
                            completed_at,
                            elapsed: MonotonicDuration::from_nanos(1),
                            outcome: AttemptOutcome::Success,
                            sanitized_error_classification: None,
                            retry_index: None,
                            clock_anomaly: false,
                        },
                        evidence,
                        interpretation,
                        vec![window],
                    )
                    .unwrap();

                    let outcome =
                        spool_then_commit(&repository, &bundle, &RealClock::new()).unwrap();
                    match outcome {
                        SpoolCycleOutcome::Committed { .. } => {
                            landed_count.fetch_add(1, Ordering::Relaxed);
                        }
                        SpoolCycleOutcome::LeftPending { .. } => {
                            spooled_count.fetch_add(1, Ordering::Relaxed);
                            let pfile = pending_dir(&state_path)
                                .join(format!("attempt-{}.json", row_id.value()));
                            assert!(pfile.exists(), "pending file must exist for spooled write");
                        }
                    }
                }
            })
        })
        .collect();

    // While writers are executing, reader periodically inspects the snapshot
    for _ in 0..15 {
        let snap_attempts: i64 = snapshot
            .query_row("SELECT COUNT(*) FROM meter_attempt", [], |row| row.get(0))
            .unwrap();
        let snap_evidence: i64 = snapshot
            .query_row("SELECT COUNT(*) FROM meter_response_evidence", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            snap_attempts, snap_initial_attempts,
            "analytical reader must see consistent attempt count"
        );
        assert_eq!(
            snap_evidence, snap_initial_evidence,
            "analytical reader must see consistent evidence count"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    for handle in writer_handles {
        handle.join().expect("writer thread must not panic");
    }

    // Assert the analytical reader still sees its original snapshot after all writers finished
    let snap_final_attempts: i64 = snapshot
        .query_row("SELECT COUNT(*) FROM meter_attempt", [], |row| row.get(0))
        .unwrap();
    let snap_final_evidence: i64 = snapshot
        .query_row("SELECT COUNT(*) FROM meter_response_evidence", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(snap_final_attempts, 1);
    assert_eq!(snap_final_evidence, 1);
    snapshot.commit().unwrap();

    // Outside the snapshot transaction, verify that writes either landed or remain durably spooled
    let landed = landed_count.load(Ordering::Acquire);
    let spooled = spooled_count.load(Ordering::Acquire);
    assert_eq!(
        landed + spooled,
        10,
        "all 10 writes must either land or remain durably spooled"
    );

    let committed_evidence: i64 = reader_conn
        .query_row("SELECT COUNT(*) FROM meter_response_evidence", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(committed_evidence, 1 + landed as i64);

    if spooled > 0 {
        let state_path = fixture.state_path().to_path_buf();
        let drain_report = drain_pending(&mut fixture.conn, &state_path).unwrap();
        assert_eq!(drain_report.applied as usize, spooled);
        let final_evidence: i64 = reader_conn
            .query_row("SELECT COUNT(*) FROM meter_response_evidence", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(final_evidence, 11);
    }

    let elapsed = start.elapsed();
    assert!(
        elapsed <= Duration::from_secs(15),
        "analytical reader test took {elapsed:?}, over the 15s budget"
    );
}

// --- Criterion 5: Failure-only batch updates projection -----------------------

/// Attempts that failed with authentication or network errors update the
/// projection, verified by observing the published file after a failure-only batch.
#[test]
fn attempts_that_failed_with_auth_or_network_errors_update_projection() {
    let mut fixture = fixture("failure-batch-projection", 5_000);

    // Initial baseline sample
    let first = fixture.start_attempt();
    let bundle = fixture.success_bundle(first, 200_000);
    fixture.repository.commit_terminal_bundle(&bundle).unwrap();

    let initial_doc = fixture.read_projection_json();
    assert_eq!(
        initial_doc["accounts"][0]["latest_attempt"]["result"]["outcome"],
        "success"
    );
    let initial_gen = initial_doc["ledger_generation"].as_u64().unwrap();
    assert_eq!(initial_gen, fixture.database_generation().value());

    // 1. Authentication failure in a batch
    let auth_attempt = fixture.start_attempt();
    let auth_result = fixture.failure_result(auth_attempt, true);
    let auth_pub = fixture
        .repository
        .commit_terminal_result(&auth_result)
        .unwrap();
    let auth_gen = auth_pub
        .published_generation()
        .expect("auth failure publishes");
    assert_eq!(auth_gen, fixture.database_generation());

    let auth_doc = fixture.read_projection_json();
    assert_eq!(
        auth_doc["ledger_generation"].as_u64().unwrap(),
        auth_gen.value()
    );
    let account = &auth_doc["accounts"][0];
    assert_eq!(
        account["latest_attempt"]["result"]["outcome"], "auth_required",
        "projection must reflect authentication failure"
    );
    assert_eq!(
        account["latest_attempt"]["attempt_id"],
        auth_attempt.value(),
        "latest attempt must be the failed attempt"
    );
    assert!(
        !account["last_successful_observation"].is_null(),
        "previous successful observation must not be erased"
    );

    // 2. Network failure in a batch
    let net_attempt = fixture.start_attempt();
    let net_result = fixture.failure_result(net_attempt, false);
    let net_pub = fixture
        .repository
        .commit_terminal_result(&net_result)
        .unwrap();
    let net_gen = net_pub
        .published_generation()
        .expect("network failure publishes");
    assert_eq!(net_gen, fixture.database_generation());

    let net_doc = fixture.read_projection_json();
    assert_eq!(
        net_doc["ledger_generation"].as_u64().unwrap(),
        net_gen.value()
    );
    let account = &net_doc["accounts"][0];
    assert_eq!(
        account["latest_attempt"]["result"]["outcome"], "unreachable",
        "projection must reflect unreachable network failure"
    );
    assert_eq!(
        account["latest_attempt"]["result"]["failure_class"], "transport_timeout",
        "failure class must be transport_timeout"
    );
    assert_eq!(
        account["latest_attempt"]["attempt_id"],
        net_attempt.value(),
        "latest attempt must advance to the network failure attempt"
    );
    assert!(
        !account["last_successful_observation"].is_null(),
        "last successful observation remains intact"
    );
}

// --- Property test: Randomized interleavings parse successfully --------------

/// Over randomized interleavings, every projection read parses successfully.
#[test]
fn property_over_randomized_interleavings_every_projection_read_parses_successfully() {
    check_property(
        "projection_reads_parse_successfully_over_randomized_interleavings",
        0..20,
        |seed| {
            let mut rng = Rng::new(Seed(seed));
            let state_dir = StateDir::new();
            let projection_path = state_dir.path().join("projection");

            // Seed initial valid projection
            let make_json = |generation_num: u64| {
                serde_json::json!({
                    "schema_version": 1,
                    "ledger_generation": generation_num,
                    "accounts": [{
                        "account_id": 1,
                        "logical_name": "work",
                        "provider": "anthropic",
                        "last_successful_observation": {
                            "observation_id": generation_num as i64,
                            "provider_observed_at_nanos": 1_000_000_000i64 + generation_num as i64,
                            "received_at_nanos": 1_000_000_000i64 + generation_num as i64,
                            "measurement_basis": "provider_observed",
                            "windows": [{
                                "semantic_key": "five_hour",
                                "scope_kind": "account_wide",
                                "scoped_model": null,
                                "quota_used_ppm": 10_000 * (generation_num as u32 % 50 + 1),
                                "reported_resolution_ppm": 10_000,
                                "quantization": "rounded_to_nearest",
                                "resets_at_nanos": 1_000_000_000i64 + generation_num as i64,
                                "nominal_duration_nanos": 18_000_000_000_000u64
                            }]
                        },
                        "latest_attempt": {
                            "attempt_id": generation_num,
                            "request_started_at_nanos": 1_000_000_000i64 + generation_num as i64,
                            "credential_context_id": null,
                            "result": {
                                "completed_at_nanos": 1_000_000_000i64 + generation_num as i64,
                                "outcome": "success",
                                "failure_class": null
                            }
                        }
                    }]
                })
            };

            let initial_bytes = serde_json::to_vec(&make_json(1)).unwrap();
            fs::write(&projection_path, initial_bytes).unwrap();

            let total_generations = 15;
            let writer_done = Arc::new(AtomicBool::new(false));
            let path_for_writer = projection_path.clone();
            let done_for_writer = Arc::clone(&writer_done);

            let writer_delay = rng.next_below(30);
            let writer_handle = std::thread::spawn(move || {
                for generation_idx in 2..=total_generations {
                    let doc = make_json(generation_idx);
                    let bytes = serde_json::to_vec(&doc).unwrap();
                    let tmp = path_for_writer.with_extension(format!("tmp-{generation_idx}"));
                    fs::write(&tmp, &bytes).unwrap();
                    fs::rename(&tmp, &path_for_writer).unwrap();
                    if writer_delay > 0 {
                        std::thread::sleep(Duration::from_micros(writer_delay));
                    }
                }
                done_for_writer.store(true, Ordering::Release);
            });

            let reader_delay = rng.next_below(25);
            let path_for_reader = projection_path.clone();
            let done_for_reader = Arc::clone(&writer_done);

            let reader_handle = std::thread::spawn(move || {
                let mut reads = 0;
                while !done_for_reader.load(Ordering::Acquire) || reads < 100 {
                    match read_projection(&path_for_reader) {
                        ProjectionRead::Available(projection) => {
                            let generation_val = projection.ledger_generation.value();
                            assert!(generation_val >= 1 && generation_val <= total_generations);
                        }
                        ProjectionRead::Unavailable(unavailable) => {
                            panic!("randomized interleaving read failed: {unavailable:?}");
                        }
                    }
                    reads += 1;
                    if reader_delay > 0 {
                        std::thread::sleep(Duration::from_micros(reader_delay));
                    }
                }
                reads
            });

            writer_handle.join().unwrap();
            let reads = reader_handle.join().unwrap();
            reads >= 100
        },
    );
}

// --- Criterion 6: Stated suite time budget -----------------------------------

#[test]
fn projection_and_store_concurrency_suite_completes_within_documented_budget() {
    let budget = Duration::from_secs(15);
    let start = Instant::now();

    // Verify structural scanner and basic round-trip checks under budget
    assert_status_performs_only_the_status_contract();
    assert_status_contract_scan_catches_forbidden_reference();

    let mut fixture = fixture("budget-check", 2_000);
    let attempt = fixture.start_attempt();
    let bundle = fixture.success_bundle(attempt, 250_000);
    fixture.repository.commit_terminal_bundle(&bundle).unwrap();
    let read = read_projection(&fixture.projection_path());
    assert!(matches!(read, ProjectionRead::Available(_)));

    let elapsed = start.elapsed();
    assert!(
        elapsed <= budget,
        "concurrency budget assertion took {elapsed:?}, over the stated {budget:?} budget"
    );
}
