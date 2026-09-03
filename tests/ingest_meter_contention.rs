//! Deterministic contention schedules over the one SQLite writer slot
//! (`aub-lqe.18`, PLAN.md 11.2, 17, 33 Phase 5, 34.6): bounded ingest batches,
//! meter evidence cycles and a long analytical reader, forced into
//! meter-first, ingest-first and interleaved orderings by channel handoffs
//! rather than by timing, so every test below is reproducible.
//!
//! The claim under proof: bounded-batch transcript ingestion cannot starve
//! irreplaceable meter writes, corrupt reader snapshots or lose a terminal
//! bundle. Meter semantics are untouched: every meter write here goes through
//! the same repository boundary and the same spool-then-commit cycle the live
//! sampling flow is specified as (PLAN.md section 13, steps 5 to 7).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

use agent_usage_book::config::{FakeEnv, Overrides, resolve};
use agent_usage_book::domain::attempt::AttemptOutcome;
use agent_usage_book::domain::ids::{AdapterVersion, MeterSemanticsId, ProviderContractId};
use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaUsed};
use agent_usage_book::domain::time::{
    FakeClock, MeasurementBasis, MonotonicDuration, RealClock, UtcTimestamp,
};
use agent_usage_book::domain::window::{
    MeterWindow, NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
    WindowSemanticKey,
};
use agent_usage_book::ingest::{IngestOptions, LandedBatch, run as run_ingest_with_sink};
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::ingest::{IngestPass, PersistEvent, WRITER_SLOT_BUDGET_PER_BATCH};
use agent_usage_book::store::meter_attempt::{
    DueReason, MeterAttemptRowId, NewMeterAttempt, NewMeterAttemptResult,
};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::repository::{NewMeterInterpretation, Repository, TerminalMeterBundle};
use agent_usage_book::store::sample_run::{Trigger, start_sample_run};
use agent_usage_book::store::sampling_policy_snapshot::{
    ResolvedSamplingPolicy, resolve_policy_snapshot,
};
use agent_usage_book::store::spool::{
    SpoolCycleOutcome, drain_pending, pending_dir, spool_pending,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aub-ingest-contention-{tag}-{}-{suffix}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch dir must be creatable");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn policy(busy_ms: u64) -> PragmaPolicy {
    PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(busy_ms),
    }
}

/// One ledger database with its repository and the fixture account, run and
/// policy snapshot a meter attempt references, built through the real store
/// APIs. This is the substrate both workloads contend over.
struct Fixture {
    _scratch: ScratchDir,
    repository: Repository,
    account_id: agent_usage_book::store::account::AccountId,
    run_id: agent_usage_book::store::sample_run::SampleRunId,
    policy_snapshot_id:
        agent_usage_book::store::sampling_policy_snapshot::SamplingPolicySnapshotId,
    conn: rusqlite::Connection,
}

fn fixture(tag: &str, busy_ms: u64) -> Fixture {
    let scratch = ScratchDir::new(tag);
    let database_path = scratch.path().join("ledger.db");
    let mut conn = open(&database_path, AccessMode::ReadWrite, &policy(busy_ms)).unwrap();
    run_migrations(
        &mut conn,
        &agent_usage_book::store::migrations::registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(1_000)),
    )
    .unwrap();
    let account_id = agent_usage_book::store::account::observe_account(
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
        "fixture",
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
        _scratch: scratch,
        repository: Repository::new(&database_path, policy(busy_ms)),
        account_id,
        run_id,
        policy_snapshot_id,
        conn,
    }
}

impl Fixture {
    fn state_dir(&self) -> &Path {
        self._scratch.path()
    }

    fn db_path(&self) -> PathBuf {
        self._scratch.path().join("ledger.db")
    }

    /// Starts one durable attempt through the production repository path and
    /// returns the row identity the terminal bundle carries.
    fn start_attempt(&self, sequence: u64) -> MeterAttemptRowId {
        let started_at = UtcTimestamp::from_unix_nanos(3_000 + sequence as i64);
        let attempt = NewMeterAttempt {
            run_id: self.run_id,
            account_id: self.account_id,
            provider: "anthropic".into(),
            request_started_at: started_at,
            credential_context_id: Some("credential-context-v1".into()),
            policy_snapshot_id: self.policy_snapshot_id,
            due_at: started_at,
            due_reason: DueReason::ForcedOrManual,
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

    /// One complete terminal bundle for `attempt_id`, response-evidence capsule
    /// included: the complete evidence a meter write must land or spool.
    fn terminal_bundle(&self, attempt_id: MeterAttemptRowId) -> TerminalMeterBundle {
        terminal_bundle_for(self.account_id, attempt_id)
    }

    /// The full meter evidence cycle (PLAN.md section 13, steps 5 to 7) for one
    /// already-started attempt: spool, commit, delete on success.
    fn meter_cycle(&self, attempt_id: MeterAttemptRowId) -> SpoolCycleOutcome {
        let bundle = self.terminal_bundle(attempt_id);
        agent_usage_book::store::spool::spool_then_commit(&self.repository, &bundle, &RealClock::new())
            .unwrap()
    }

    fn observation_count(&self) -> u64 {
        self.conn
            .query_row("SELECT COUNT(*) FROM meter_observation", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap() as u64
    }

    fn evidence_count(&self) -> u64 {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM meter_response_evidence",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap() as u64
    }

    /// The reconciliation invariant the run must preserve: every occurrence
    /// names an existing event, and this corpus's unique identities land one
    /// occurrence each, so canonical and occurrence counts agree.
    fn usage_reconciliation(&self) -> (u64, u64, u64) {
        self.conn
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM usage_event),
                    (SELECT COUNT(*) FROM usage_occurrence),
                    (SELECT COUNT(*) FROM usage_occurrence o
                     JOIN usage_event e ON e.id = o.event_id)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
    }

    fn pending_record_exists(&self, attempt_id: MeterAttemptRowId) -> bool {
        pending_dir(self.state_dir())
            .join(format!("attempt-{}.json", attempt_id.value()))
            .exists()
    }
}

/// One complete terminal bundle, built free of the fixture so threads the
/// schedules spawn can build one from cloned identities alone.
fn terminal_bundle_for(
    account_id: agent_usage_book::store::account::AccountId,
    attempt_id: MeterAttemptRowId,
) -> TerminalMeterBundle {
    let completed_at = UtcTimestamp::from_unix_nanos(4_000);
    let window = MeterWindow::new(
        WindowSemanticKey::new("five_hour"),
        WindowScope::AccountWide,
        QuotaUsed::new(QuotaFractionPpm::new(250_000).unwrap()),
        ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap(),
        QuantizationSemantics::RoundedToNearest,
        UtcTimestamp::from_unix_nanos(9_000),
        NominalWindowDuration::from_nanos(18_000_000_000_000),
    );
    TerminalMeterBundle::new(
        NewMeterAttemptResult {
            attempt_id,
            completed_at,
            elapsed: MonotonicDuration::from_nanos(1_000),
            outcome: AttemptOutcome::Success,
            sanitized_error_classification: None,
            retry_index: None,
            clock_anomaly: false,
        },
        agent_usage_book::store::meter_evidence::NewMeterResponseEvidence {
            attempt_id,
            response_classification: "success".into(),
            received_at: completed_at,
            provider_observed_at_original: Some("1970-01-01T00:00:04Z".into()),
            evidence_capsule: "{\"five_hour\":\"25.0\"}".into(),
            capsule_schema_version: "capsule-v1".into(),
            sanitizer_version: "sanitizer-v1".into(),
            capture_truncated: false,
        },
        NewMeterInterpretation {
            account_id,
            provider: "anthropic".into(),
            provider_observed_at: Some(UtcTimestamp::from_unix_nanos(3_900)),
            received_at: completed_at,
            measurement_basis: MeasurementBasis::ProviderObserved,
            observed_plan: Some("max".into()),
            observed_tier: None,
            adapter_version: AdapterVersion::new("adapter-v1"),
            provider_contract_id: ProviderContractId::new("contract-v1"),
            meter_semantics_id: MeterSemanticsId::new("semantics-v1"),
            normalized_fingerprint: "fingerprint-v1".into(),
        },
        vec![window],
    )
    .unwrap()
}

// --- ingest corpus fixture ---------------------------------------------------

/// Writes `events` distinct claude-code messages across `files` files, so a
/// pass over the corpus parses every file and lands `events` canonical rows.
fn write_corpus(root: &Path, events: u64, files: u64) {
    fs::create_dir_all(root).expect("corpus root must be creatable");
    let per_file = events / files;
    for file in 0..files {
        let mut body = String::new();
        for message in 0..per_file {
            let id = file * per_file + message;
            body.push_str(&format!(
                r#"{{"type":"assistant","timestamp":"2026-08-25T10:{:02}:00.000Z","sessionId":"s{file}","message":{{"id":"m{id}","usage":{{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}}}"#,
                (message % 60) as u64
            ));
            body.push('\n');
        }
        fs::write(root.join(format!("file{file}.jsonl")), body)
            .expect("corpus file must be writable");
    }
}

fn config_for(corpus: &Path, max_batch_events: u64) -> agent_usage_book::config::Config {
    let toml = format!(
        r#"
[ingest]
max_batch_events = {max_batch_events}

[[transcripts]]
name = "claude-code"
root = "{}"
pattern = "**/*.jsonl"
format = "claude-code"
"#,
        corpus.display()
    );
    resolve(
        &Overrides::new(),
        &FakeEnv::new(),
        Some(&toml),
        "/virtual/aub.toml",
    )
    .expect("resolve test config")
    .0
}

/// A second connection onto the fixture's ledger, for a writer or drainer that
/// must not borrow the fixture's own connection.
fn extra_conn(fixture: &Fixture, busy_ms: u64) -> rusqlite::Connection {
    open(&fixture.db_path(), AccessMode::ReadWrite, &policy(busy_ms)).unwrap()
}

/// Runs one ingest pass, collecting every landed batch the sink observes.
fn run_ingest_collecting(
    conn: &mut rusqlite::Connection,
    config: &agent_usage_book::config::Config,
) -> (agent_usage_book::ingest::IngestReport, Vec<LandedBatch>) {
    let mut batches = Vec::new();
    let report = run_ingest_with_sink(
        conn,
        config,
        &IngestOptions::default(),
        &RealClock::new(),
        &mut |batch: &LandedBatch| {
            batches.push(batch.clone());
            Ok(())
        },
    )
    .expect("the ingest pass must land");
    (report, batches)
}

/// A migrated ingest connection beside the fixture's own: the schedules run
/// ingest and meter work on separate connections the way the separate
/// processes do, over the one database.
fn ingest_conn(fixture: &Fixture, busy_ms: u64) -> rusqlite::Connection {
    let mut conn = extra_conn(fixture, busy_ms);
    run_migrations(
        &mut conn,
        &agent_usage_book::store::migrations::registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
    )
    .unwrap();
    conn
}

// --- the schedules -----------------------------------------------------------

/// Schedule: meter-first. A meter writer holds the writer slot mid-write; the
/// ingest batch waits for the slot; the meter write completes; the batch lands
/// complete anyway. The meter's hold is absent from the writer-slot number the
/// budget judges, because that number starts at acquisition, not at the BEGIN.
#[test]
fn meter_first_holds_the_slot_and_the_ingest_batch_waits_without_losing_it() {
    let fixture = fixture("meter-first", 5_000);
    let corpus = fixture.state_dir().join("corpus");
    write_corpus(&corpus, 2, 1);
    let config = config_for(&corpus, 10);
    let attempt_id = fixture.start_attempt(1);

    // The meter writer's in-flight hold on the one write slot.
    let mut holder = extra_conn(&fixture, 5_000);
    let held = holder
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();

    let mut writer = ingest_conn(&fixture, 5_000);
    let started = std::time::Instant::now();
    let handle = std::thread::spawn(move || run_ingest_collecting(&mut writer, &config));
    // Let the pass reach its first write transaction and block there.
    std::thread::sleep(std::time::Duration::from_millis(200));

    // The meter write completes once the slot is free; the batch lands after.
    drop(held);
    let outcome = fixture.meter_cycle(attempt_id);
    assert!(
        matches!(outcome, SpoolCycleOutcome::Committed { .. }),
        "the meter write must commit once the slot is free"
    );

    let (report, batches) = handle.join().unwrap();
    let wall = started.elapsed();
    assert_eq!(report.batches.len(), 1);
    assert_eq!(report.outcome.events_written.value(), 2);
    assert!(
        u128::from(batches[0].writer_slot.as_nanos()) < wall.as_nanos(),
        "the writer-slot measurement must exclude the wait for a held slot: slot {}ns, wall {}ns",
        batches[0].writer_slot.as_nanos(),
        wall.as_nanos()
    );
    assert_eq!(fixture.observation_count(), 1);
    assert_eq!(fixture.usage_reconciliation(), (2, 2, 2));
    assert!(
        !fixture.pending_record_exists(attempt_id),
        "a committed meter write must not leave its pending record"
    );
}

/// Schedule: ingest-first. An ingest batch holds the writer slot; the meter's
/// commit waits past its own busy bound and gives up; the spool keeps the
/// complete evidence capsule; the drain applies it exactly once after the
/// batch lands. This is the "commit or remain durably spooled" arm under the
/// worst ingest behaviour the bounded-batch design permits.
#[test]
fn ingest_first_refuses_the_meter_commit_which_spools_and_drains_exactly_once() {
    let mut fixture = fixture("ingest-first", 150);
    let attempt_id = fixture.start_attempt(1);

    // The ingest batch's in-flight hold, longer than the meter's busy bound.
    let mut holder = extra_conn(&fixture, 5_000);
    let held = holder
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();

    let outcome = fixture.meter_cycle(attempt_id);
    match outcome {
        SpoolCycleOutcome::LeftPending { error, commit_wait } => {
            assert!(
                commit_wait.as_nanos() > 0,
                "the refused commit must have waited for the slot first"
            );
            assert!(
                error.to_string().contains("busy") || error.to_string().contains("locked"),
                "the refusal must be the SQLite busy class: {error}"
            );
        }
        SpoolCycleOutcome::Committed { .. } => {
            panic!("a slot held past the meter's busy bound must refuse the commit")
        }
    }
    assert!(
        fixture.pending_record_exists(attempt_id),
        "the refused evidence must remain durably spooled"
    );
    drop(held);

    // Draining after contention applies exactly once: one bundle, one
    // observation, and a second drain is a counted no-op.
    let state_dir = fixture.state_dir().to_path_buf();
    let first = drain_pending(&mut fixture.conn, &state_dir).unwrap();
    assert_eq!(first.applied, 1);
    assert_eq!(fixture.observation_count(), 1);
    assert_eq!(fixture.evidence_count(), 1);
    let second = drain_pending(&mut fixture.conn, &state_dir).unwrap();
    // The applied record was deleted with its application, so the next pass
    // finds an empty spool rather than a replayable record.
    assert_eq!((second.applied, second.already_applied), (0, 0));
    assert_eq!(
        fixture.observation_count(),
        1,
        "draining twice must not duplicate the terminal bundle"
    );
}

/// Schedule: interleaved, at the orchestrator level. The ingest pass lands its
/// bounded batches, and between two consecutive batches the sink hands control
/// to the meter writer, which commits a full evidence cycle before the pass
/// may continue. Every meter write lands while the pass is in flight, and the
/// pass still completes: bounded batches make progress without monopolizing
/// the writer slot.
#[test]
fn meter_writes_land_between_ingest_batches_while_the_pass_is_in_flight() {
    let fixture = fixture("interleaved", 5_000);
    let corpus = fixture.state_dir().join("corpus");
    write_corpus(&corpus, 6, 2);
    let config = config_for(&corpus, 2);

    let (batch_landed_tx, batch_landed_rx) = mpsc::channel::<LandedBatch>();
    let (meter_done_tx, meter_done_rx) = mpsc::channel::<()>();
    let repository = fixture.repository.clone();
    let (account_id, run_id, policy_snapshot_id) = (
        fixture.account_id,
        fixture.run_id,
        fixture.policy_snapshot_id,
    );
    let mut writer = ingest_conn(&fixture, 5_000);

    let landed_meter_writes = std::thread::scope(|scope| {
        let mut sink = move |batch: &LandedBatch| {
            batch_landed_tx.send(batch.clone()).unwrap();
            // Hand the writer slot to the meter writer between batches: this
            // blocks the pass until the meter's cycle has committed.
            meter_done_rx
                .recv()
                .expect("the schedule must interleave a meter write per batch");
            Ok(())
        };
        let ingest = scope.spawn(move || {
            run_ingest_with_sink(
                &mut writer,
                &config,
                &IngestOptions::default(),
                &RealClock::new(),
                &mut sink,
            )
            .expect("the pass must land under interleaved meter writes")
        });

        let mut landed = 0;
        for _ in 0..3 {
            let batch = batch_landed_rx
                .recv()
                .expect("the pass must announce each batch");
            assert!(batch.events <= 2, "no batch may exceed the bound");
            // One meter write, start to terminal commit, between batches.
            let attempt = NewMeterAttempt {
                run_id,
                account_id,
                provider: "anthropic".into(),
                request_started_at: UtcTimestamp::from_unix_nanos(3_000),
                credential_context_id: Some("credential-context-v1".into()),
                policy_snapshot_id,
                due_at: UtcTimestamp::from_unix_nanos(2_500),
                due_reason: DueReason::ForcedOrManual,
                due_basis: None,
                provider_contract_id: "contract-v1".into(),
                meter_semantics_id: "semantics-v1".into(),
            };
            let started = repository.start_meter_attempt(&attempt).unwrap();
            let row = MeterAttemptRowId::new(
                i64::try_from(started.attempt_id().value())
                    .expect("attempt identity fits SQLite INTEGER"),
            );
            let bundle = terminal_bundle_for(account_id, row);
            let outcome = agent_usage_book::store::spool::spool_then_commit(
                &repository,
                &bundle,
                &RealClock::new(),
            )
            .unwrap();
            assert!(
                matches!(outcome, SpoolCycleOutcome::Committed { .. }),
                "the meter write must land between ingest batches"
            );
            landed += 1;
            meter_done_tx.send(()).unwrap();
        }
        ingest.join().unwrap();
        landed
    });

    assert_eq!(
        landed_meter_writes, 3,
        "every batch must interleave one meter write"
    );
    assert_eq!(fixture.observation_count(), 3);
    assert_eq!(fixture.evidence_count(), 3);
    let (canonical, occurrences, linked) = fixture.usage_reconciliation();
    assert_eq!(canonical, 6);
    assert_eq!(occurrences, 6);
    assert_eq!(linked, 6, "every occurrence must name an existing event");
    let spool_entries = fs::read_dir(pending_dir(fixture.state_dir()))
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(
        spool_entries, 0,
        "the spool must be empty when every meter write committed"
    );
}

/// Schedule: long analytical reader. A read snapshot is held open across both
/// writers' commits; the reader observes its original complete snapshot the
/// whole time and both writers proceed. WAL gives the reader a consistent
/// view; nothing here may tear it.
#[test]
fn a_long_analytical_reader_keeps_a_consistent_snapshot_across_both_writers() {
    let fixture = fixture("long-reader", 5_000);
    let corpus = fixture.state_dir().join("corpus");
    write_corpus(&corpus, 2, 1);
    let config = config_for(&corpus, 10);
    let attempt_id = fixture.start_attempt(1);

    let mut reader = open(&fixture.db_path(), AccessMode::ReadOnly, &policy(5_000)).unwrap();
    let snapshot = reader.transaction().unwrap();
    let snapshot_events: i64 = snapshot
        .query_row("SELECT COUNT(*) FROM usage_event", [], |row| row.get(0))
        .unwrap();

    // Both writers run while the snapshot stays open.
    let outcome = fixture.meter_cycle(attempt_id);
    assert!(matches!(outcome, SpoolCycleOutcome::Committed { .. }));
    let mut writer = ingest_conn(&fixture, 5_000);
    let (report, _) = run_ingest_collecting(&mut writer, &config);
    assert_eq!(report.outcome.events_written.value(), 2);

    // The reader's snapshot is unchanged by both commits.
    let snapshot_after: i64 = snapshot
        .query_row("SELECT COUNT(*) FROM usage_event", [], |row| row.get(0))
        .unwrap();
    assert_eq!(snapshot_events, snapshot_after);
    snapshot.commit().unwrap();

    // After the snapshot closes, everything both writers landed is visible and
    // the reconciliation invariant holds.
    assert_eq!(fixture.usage_reconciliation(), (2, 2, 2));
    assert_eq!(fixture.observation_count(), 1);
}

/// Every landed batch holds the writer slot for at most the stated per-batch
/// budget, and a pass over a corpus larger than the bound splits into exactly
/// the batches the bound implies, each with its own generation advance.
#[test]
fn every_batch_respects_the_stated_writer_slot_budget_and_the_pass_splits() {
    let fixture = fixture("budget", 5_000);
    let corpus = fixture.state_dir().join("corpus");
    write_corpus(&corpus, 5, 1);
    let config = config_for(&corpus, 2);
    let mut writer = ingest_conn(&fixture, 5_000);

    let (report, batches) = run_ingest_collecting(&mut writer, &config);
    assert_eq!(
        report.batches.len(),
        3,
        "five events under a bound of two must split into three batches"
    );
    for (index, batch) in batches.iter().enumerate() {
        assert_eq!(batch.index, (index + 1) as u64, "batch indices count from 1");
        assert!(
            batch.writer_slot.as_nanos() <= WRITER_SLOT_BUDGET_PER_BATCH.as_nanos(),
            "batch {} held the writer slot {}ns, over the stated budget {}ns",
            batch.index,
            batch.writer_slot.as_nanos(),
            WRITER_SLOT_BUDGET_PER_BATCH.as_nanos()
        );
        assert!(
            batch.events <= 2,
            "no batch may exceed the configured bound: batch {} carried {}",
            batch.index,
            batch.events
        );
    }
    assert_eq!(batches[0].generation.value(), 1);
    assert_eq!(batches[1].generation.value(), 2);
    assert_eq!(batches[2].generation.value(), 3);
    assert_eq!(report.generation.value(), 3);
    assert_eq!(report.outcome.events_written.value(), 5);
    assert_eq!(fixture.usage_reconciliation(), (5, 5, 5));
}

/// A batch that fails mid-flight commits nothing. The injection is real store
/// behaviour, not a seam: a batch carrying one valid event and one watermark
/// naming an absolute path, which the store refuses. The refusal fires after
/// the event rows were written inside the same transaction, so what the
/// rollback leaves is the proof: no event, no occurrence, no component, no
/// generation advance, and the next batch lands the same event cleanly.
#[test]
fn a_batch_whose_watermark_refuses_mid_transaction_rolls_back_its_event_rows() {
    let fixture = fixture("mid-batch-primitive", 5_000);
    let mut writer = ingest_conn(&fixture, 5_000);

    let pass_with = |relative_path: String| IngestPass {
        events: vec![persist_event("m1", "corpus/a.jsonl", 1_000, 10, 5, &relative_path)],
        sessions: Vec::new(),
        watermarks: vec![agent_usage_book::transcripts::Watermark {
            source_key: "claude-code".into(),
            relative_path,
            size: 10,
            mtime_nanos: 0,
            identity: "identity-v1".into(),
            parser_version: "claude-code-v1".into(),
            consumed_offset: 5,
        }],
        quarantined: Vec::new(),
        collisions: Vec::new(),
        whole_file_sources: Vec::new(),
        created_at: UtcTimestamp::from_unix_nanos(2_000),
    };

    let error = agent_usage_book::store::ingest::persist_ingest_batch(
        &mut writer,
        &pass_with("/absolute/path/session.jsonl".into()),
        &RealClock::new(),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("relative"),
        "the store must refuse the absolute watermark path: {error}"
    );
    assert_eq!(
        fixture.usage_reconciliation(),
        (0, 0, 0),
        "a batch that fails after its event rows were written must roll them back"
    );
    assert_eq!(
        agent_usage_book::store::ingestion_generation::current(&writer).unwrap(),
        agent_usage_book::store::ingestion_generation::Generation::new(0),
        "a failed batch must leave the ingestion generation where it was"
    );

    // The same event under an indexable watermark lands whole.
    let outcome = agent_usage_book::store::ingest::persist_ingest_batch(
        &mut writer,
        &pass_with("corpus/a.jsonl".into()),
        &RealClock::new(),
    )
    .unwrap();
    assert_eq!(outcome.events_written.value(), 1);
    assert_eq!(fixture.usage_reconciliation(), (1, 1, 1));
}

/// One strong-identity persist event, built through the shared identity
/// framework exactly the orchestrator builds them, so the batch under test
/// carries a row the real path would produce.
fn persist_event(
    id: &str,
    file: &str,
    occurred_nanos: i64,
    input: u64,
    output: u64,
    relative_path: &str,
) -> PersistEvent {
    use agent_usage_book::dedup::{canonical_identity, canonical_payload_digest};
    use agent_usage_book::domain::ids::{NativeSessionId, SessionId, SourceNamespace};
    use agent_usage_book::domain::tokens::{
        CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens,
        UsageVector,
    };
    use agent_usage_book::evidence::{CoverageCompleteness, EvidenceQuality, Provenance};
    use agent_usage_book::transcripts::NormalizedUsageEvent;
    use agent_usage_book::transcripts::parser::{
        EvidenceClassification, ParserVersion, STRONG_IDENTITY_PREFIX,
    };

    let event = NormalizedUsageEvent::new(
        UsageVector::new(
            KnownTokenVector::new(
                InputTokens::new(input),
                OutputTokens::new(output),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(0),
            ),
            std::collections::BTreeMap::new(),
            CoverageCompleteness::Complete,
            EvidenceQuality::Measured,
        ),
        EvidenceClassification::Reported,
        Provenance::new(vec![file.to_string(), format!("{STRONG_IDENTITY_PREFIX}{id}")]),
        ParserVersion::new("test-1"),
    )
    .with_occurred_at(UtcTimestamp::from_unix_nanos(occurred_nanos))
    .with_session(SessionId::new(
        SourceNamespace::new("test"),
        NativeSessionId::new("s1"),
    ));
    let identity = canonical_identity(&event);
    PersistEvent {
        event: event.clone(),
        namespace: SourceNamespace::new("test"),
        canonical_event_id: identity.canonical_event_id,
        native_event_id: identity.native_event_id,
        heuristic_key: identity.heuristic_key,
        heuristic_algorithm_version: None,
        canonical_payload_digest: canonical_payload_digest(&event),
        relative_path: Some(relative_path.to_string()),
    }
}

/// A pass whose second batch cannot land (a competing writer holds the slot
/// past the pass's busy bound) stops whole: batch one stays complete, batch
/// two leaves no row, and a re-run converges onto the full corpus.
#[test]
fn a_refused_batch_stops_the_pass_whole_and_a_rerun_converges() {
    let fixture = fixture("refused-batch", 5_000);
    let corpus = fixture.state_dir().join("corpus");
    write_corpus(&corpus, 4, 1);
    let config = config_for(&corpus, 2);
    let mut writer = ingest_conn(&fixture, 150);

    let (batch_landed_tx, batch_landed_rx) = mpsc::channel::<()>();
    let (slot_taken_tx, slot_taken_rx) = mpsc::channel::<()>();
    let rerun_config = config.clone();
    let result = std::thread::scope(|scope| {
        let mut sink = move |_batch: &LandedBatch| {
            batch_landed_tx.send(()).unwrap();
            // The schedule must be holding the write slot by the time the sink
            // returns, so the pass's next batch waits past its busy bound and
            // refuses. The sink waits for that confirmation, then lets the
            // pass continue into the refusal.
            slot_taken_rx
                .recv()
                .expect("the schedule must take the slot");
            Ok(())
        };
        let ingest = scope.spawn(move || {
            run_ingest_with_sink(
                &mut writer,
                &config,
                &IngestOptions::default(),
                &RealClock::new(),
                &mut sink,
            )
        });

        batch_landed_rx
            .recv()
            .expect("batch one must land before the schedule holds the slot");
        let mut holder = extra_conn(&fixture, 150);
        let held = holder
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        slot_taken_tx.send(()).unwrap();
        // Give the pass's batch two its whole busy window to refuse.
        std::thread::sleep(std::time::Duration::from_millis(400));
        let result = ingest.join().unwrap();
        drop(held);
        result
    });

    let error = result.unwrap_err();
    assert!(
        error.to_string().contains("another writer holds"),
        "a refused batch must refuse whole, not partially land: {error}"
    );
    // Batch one is complete; batch two is entirely absent.
    assert_eq!(fixture.usage_reconciliation(), (2, 2, 2));
    assert_eq!(
        agent_usage_book::store::ingestion_generation::current(&fixture.conn).unwrap(),
        agent_usage_book::store::ingestion_generation::Generation::new(1),
        "the landed batch's generation is the counter's value"
    );

    // A re-run converges: the pass re-parses the corpus whole, replaces the
    // landed file's contribution, and lands the rest, so the store ends with
    // the full corpus exactly once and the generation advanced once more.
    let mut rerun = ingest_conn(&fixture, 5_000);
    let (report, batches) = run_ingest_collecting(&mut rerun, &rerun_config);
    assert_eq!(report.outcome.events_written.value(), 4);
    assert_eq!(report.outcome.events_already_ingested.value(), 0);
    assert_eq!(batches.len(), 2);
    assert_eq!(report.generation.value(), 3);
    assert_eq!(fixture.usage_reconciliation(), (4, 4, 4));
}

/// Crash semantics under contention: a bundle spooled and never committed (the
/// exact durable state an interruption after PLAN.md section 13's step 5
/// leaves) sits in the spool while ingest batches run; the drain applies it
/// exactly once, no duplicate usage occurrence appears, and the reconciliation
/// invariant holds before and after recovery.
#[test]
fn an_interruption_after_spooling_drains_exactly_once_beside_ingest_batches() {
    let mut fixture = fixture("crash-spool", 5_000);
    let corpus = fixture.state_dir().join("corpus");
    write_corpus(&corpus, 4, 1);
    let config = config_for(&corpus, 2);
    let attempt_id = fixture.start_attempt(1);
    let bundle = fixture.terminal_bundle(attempt_id);

    // Step 5 completes; the commit (step 6) never happens: the interruption.
    let pending = agent_usage_book::store::spool::PendingTerminalBundle::from_bundle(&bundle);
    spool_pending(fixture.state_dir(), &pending).unwrap();

    // Ingest batches run beside the stranded record and never touch it.
    let mut writer = ingest_conn(&fixture, 5_000);
    let (report, batches) = run_ingest_collecting(&mut writer, &config);
    assert_eq!(report.outcome.events_written.value(), 4);
    assert_eq!(batches.len(), 2);
    assert_eq!(
        fixture.usage_reconciliation(),
        (4, 4, 4),
        "the reconciliation holds with the record still pending"
    );

    // Recovery: one drain applies the interrupted evidence exactly once.
    let crash_state_dir = fixture.state_dir().to_path_buf();
    let report = drain_pending(&mut fixture.conn, &crash_state_dir).unwrap();
    assert_eq!(report.applied, 1);
    assert_eq!(fixture.observation_count(), 1);
    assert_eq!(fixture.evidence_count(), 1);
    assert!(
        !fixture.pending_record_exists(attempt_id),
        "the drained record must leave the spool"
    );
    // A second drain is a counted no-op: no duplicate occurrence, no second
    // bundle, and the usage reconciliation is unchanged by recovery.
    let again = drain_pending(&mut fixture.conn, &crash_state_dir).unwrap();
    assert_eq!((again.applied, again.already_applied), (0, 0));
    assert_eq!(fixture.observation_count(), 1);
    assert_eq!(fixture.usage_reconciliation(), (4, 4, 4));
}