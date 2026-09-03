//! Test-only comparison of projection and direct SQLite status reads.
//!
//! The benchmark is intentionally compiled only for tests. It provides the
//! evidence aub-c5m needs without weakening the production status boundary:
//! the direct path opens SQLite read-only, performs no migration, and writes no
//! ledger state. The E2E runner executes the ignored entry point with a child
//! timeout and retains the JSON artifact alongside its command log.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::TransactionBehavior;
use serde_json::{Value, json};

use agent_usage_book::domain::time::{
    Clock, ClockSkewEnvelope, FakeClock, MonotonicDuration, UtcTimestamp,
};
use agent_usage_book::projection::build::projection;
use agent_usage_book::projection::reader::{
    ProjectedReading, ProjectionRead, account_reading, read_projection,
};
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::ledger_generation::current;
use agent_usage_book::store::migrate::{Migration, run_migrations};
use agent_usage_book::store::projection_source::{account_meter_states, test_support::fixture};

const BENCHMARK_SCHEMA: &str = "aub.projection_benchmark.v1";
const SAMPLES_PER_CASE: usize = 8;
// The direct source reads account state individually, so this is deliberately
// large enough to expose that shape without turning an E2E regression check
// into a multi-minute machine benchmark.
const LARGE_ACCOUNT_COUNT: usize = 256;
const LOCK_HOLD: Duration = Duration::from_millis(400);
const DIRECT_BUSY_TIMEOUT: MonotonicDuration = MonotonicDuration::from_millis(25);
const STATUS_BUDGET: &str = "unmeasured";

static MIGRATION_STARTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy)]
enum CaseKind {
    Uncontended,
    LargePopulated,
    ActiveWriter,
    ActiveMigration,
}

impl CaseKind {
    fn identity(self) -> &'static str {
        match self {
            Self::Uncontended => "uncontended",
            Self::LargePopulated => "large_populated",
            Self::ActiveWriter => "active_writer",
            Self::ActiveMigration => "active_migration",
        }
    }
}

#[derive(Debug)]
enum ReadOutcome {
    Completed {
        readings: Vec<ProjectedReading>,
        elapsed_ns: u128,
    },
    Busy {
        elapsed_ns: u128,
        detail: String,
    },
    Failed {
        elapsed_ns: u128,
        detail: String,
    },
}

impl ReadOutcome {
    fn elapsed_ns(&self) -> u128 {
        match self {
            Self::Completed { elapsed_ns, .. }
            | Self::Busy { elapsed_ns, .. }
            | Self::Failed { elapsed_ns, .. } => *elapsed_ns,
        }
    }
}

/// The direct comparator's complete surface. It opens the existing ledger in
/// read-only mode, takes its snapshot through the store's read functions, maps
/// it through the same projection builder, and runs the same typed freshness
/// reader as the projection path. It cannot migrate or write because it never
/// receives a read-write connection or a migration registry.
fn direct_read_only_status(
    database_path: &Path,
    clock: &impl Clock,
) -> Result<Vec<ProjectedReading>, String> {
    let policy = PragmaPolicy {
        busy_timeout: DIRECT_BUSY_TIMEOUT,
    };
    let connection =
        open(database_path, AccessMode::ReadOnly, &policy).map_err(|error| error.to_string())?;
    let states = account_meter_states(&connection).map_err(|error| error.to_string())?;
    let direct = projection(
        current(&connection).map_err(|error| error.to_string())?,
        &states,
    );
    Ok(direct
        .accounts
        .iter()
        .map(|account| {
            account_reading(
                Some(account),
                None,
                MonotonicDuration::from_seconds(900),
                MonotonicDuration::from_seconds(30),
                ClockSkewEnvelope::new(MonotonicDuration::from_seconds(60)),
                clock,
            )
        })
        .collect())
}

fn projection_status(path: &Path, clock: &impl Clock) -> Result<Vec<ProjectedReading>, String> {
    let ProjectionRead::Available(projection) = read_projection(path) else {
        return Err("published projection was unavailable".to_string());
    };
    Ok(projection
        .accounts
        .iter()
        .map(|account| {
            account_reading(
                Some(account),
                None,
                MonotonicDuration::from_seconds(900),
                MonotonicDuration::from_seconds(30),
                ClockSkewEnvelope::new(MonotonicDuration::from_seconds(60)),
                clock,
            )
        })
        .collect())
}

fn measure(read: impl FnOnce() -> Result<Vec<ProjectedReading>, String>) -> ReadOutcome {
    let started = Instant::now();
    match read() {
        Ok(readings) => ReadOutcome::Completed {
            readings,
            elapsed_ns: started.elapsed().as_nanos(),
        },
        Err(detail)
            if detail.contains("database is locked") || detail.contains("database is busy") =>
        {
            ReadOutcome::Busy {
                elapsed_ns: started.elapsed().as_nanos(),
                detail,
            }
        }
        Err(detail) => ReadOutcome::Failed {
            elapsed_ns: started.elapsed().as_nanos(),
            detail,
        },
    }
}

fn percentile(sorted: &[u128], numerator: usize, denominator: usize) -> Option<u128> {
    if sorted.is_empty() {
        return None;
    }
    let index = (sorted.len() * numerator)
        .div_ceil(denominator)
        .saturating_sub(1);
    sorted.get(index).copied()
}

fn result_json(outcomes: &[ReadOutcome]) -> Value {
    let mut latencies: Vec<u128> = outcomes.iter().map(ReadOutcome::elapsed_ns).collect();
    latencies.sort_unstable();
    let busy = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, ReadOutcome::Busy { .. }))
        .count();
    let failed: Vec<&str> = outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            ReadOutcome::Failed { detail, .. } => Some(detail.as_str()),
            ReadOutcome::Completed { .. } | ReadOutcome::Busy { .. } => None,
        })
        .collect();
    let busy_details: Vec<&str> = outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            ReadOutcome::Busy { detail, .. } => Some(detail.as_str()),
            ReadOutcome::Completed { .. } | ReadOutcome::Failed { .. } => None,
        })
        .collect();
    json!({
        "samples": outcomes.len(),
        "completed": outcomes.len() - busy - failed.len(),
        "busy": busy,
        "failed": failed.len(),
        "p50_latency_ns": percentile(&latencies, 50, 100),
        "p99_latency_ns": percentile(&latencies, 99, 100),
        "busy_details": busy_details,
        "failures": failed,
    })
}

fn publish_seed(fixture: &agent_usage_book::store::projection_source::test_support::Fixture) {
    let publication =
        agent_usage_book::projection::publish(&fixture.conn, &fixture.projection_path());
    assert!(
        publication.published_generation().is_some(),
        "the benchmark seed must publish a readable projection: {publication:?}"
    );
}

fn seed_large_database(
    fixture: &mut agent_usage_book::store::projection_source::test_support::Fixture,
) {
    fixture.seed_additional_accounts(LARGE_ACCOUNT_COUNT);
    publish_seed(fixture);
}

fn wait_until(started: &AtomicBool, context: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !started.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "{context} did not start within the bound"
        );
        thread::yield_now();
    }
}

fn hold_writer(database_path: &Path) -> (mpsc::Sender<()>, thread::JoinHandle<()>) {
    let path = database_path.to_path_buf();
    let started = Arc::new(AtomicBool::new(false));
    let child_started = Arc::clone(&started);
    let (release_tx, release_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let policy = PragmaPolicy {
            busy_timeout: MonotonicDuration::from_seconds(2),
        };
        let mut connection = open(&path, AccessMode::ReadWrite, &policy).unwrap();
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        child_started.store(true, Ordering::Release);
        release_rx.recv_timeout(LOCK_HOLD).unwrap();
        transaction.commit().unwrap();
    });
    wait_until(&started, "writer lock");
    (release_tx, handle)
}

fn hold_migration(database_path: &Path) -> thread::JoinHandle<()> {
    MIGRATION_STARTED.store(false, Ordering::Release);
    let path = database_path.to_path_buf();
    let handle = thread::spawn(move || {
        let policy = PragmaPolicy {
            busy_timeout: MonotonicDuration::from_seconds(2),
        };
        let mut connection = open(&path, AccessMode::ReadWrite, &policy).unwrap();
        let mut migrations = agent_usage_book::store::migrations::registry();
        migrations.push(Migration {
            version: migrations.last().unwrap().version + 1,
            rewrites_irreplaceable: false,
            apply: hold_benchmark_migration_lock,
        });
        run_migrations(
            &mut connection,
            &migrations,
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(99)),
        )
        .unwrap();
    });
    wait_until(&MIGRATION_STARTED, "migration lock");
    handle
}

fn hold_benchmark_migration_lock(
    connection: &rusqlite::Connection,
) -> Result<(), agent_usage_book::error::Error> {
    connection
        .execute_batch("CREATE TABLE benchmark_migration_probe (id INTEGER PRIMARY KEY)")
        .map_err(|error| {
            agent_usage_book::error::Error::Store(format!(
                "cannot create benchmark migration probe: {error}"
            ))
        })?;
    MIGRATION_STARTED.store(true, Ordering::Release);
    thread::sleep(LOCK_HOLD);
    Ok(())
}

fn benchmark_case(kind: CaseKind) -> Value {
    let mut fixture = fixture(kind.identity());
    let attempt = fixture.start_attempt();
    fixture.commit_success_bundle(attempt);
    if matches!(kind, CaseKind::LargePopulated) {
        seed_large_database(&mut fixture);
    } else {
        publish_seed(&fixture);
    }

    let database_path = fixture.database_path();
    let projection_path = fixture.projection_path();
    let before = std::fs::read(&database_path).unwrap();
    let size = std::fs::metadata(&database_path).unwrap().len();

    let mut writer = None;
    let mut migration = None;
    match kind {
        CaseKind::ActiveWriter => writer = Some(hold_writer(&database_path)),
        CaseKind::ActiveMigration => migration = Some(hold_migration(&database_path)),
        CaseKind::Uncontended | CaseKind::LargePopulated => {}
    }

    let mut projection_outcomes = Vec::with_capacity(SAMPLES_PER_CASE);
    let mut direct_outcomes = Vec::with_capacity(SAMPLES_PER_CASE);
    for _ in 0..SAMPLES_PER_CASE {
        let projection_read = measure(|| projection_status(&projection_path, &fixture.clock));
        let direct_read = measure(|| direct_read_only_status(&database_path, &fixture.clock));
        if let (
            ReadOutcome::Completed {
                readings: projection,
                ..
            },
            ReadOutcome::Completed {
                readings: direct, ..
            },
        ) = (&projection_read, &direct_read)
        {
            assert_eq!(
                projection, direct,
                "projection and direct SQLite status must agree on the same seeded state"
            );
        }
        projection_outcomes.push(projection_read);
        direct_outcomes.push(direct_read);
    }

    if let Some((release, handle)) = writer {
        release.send(()).unwrap();
        handle.join().unwrap();
    }
    if let Some(handle) = migration {
        handle.join().unwrap();
    }

    if !matches!(kind, CaseKind::ActiveMigration) {
        let after = std::fs::read(&database_path).unwrap();
        assert_eq!(
            before, after,
            "the direct comparator must not write the ledger database"
        );
    }
    assert!(
        direct_outcomes
            .iter()
            .all(|outcome| !matches!(outcome, ReadOutcome::Failed { .. })),
        "a direct status read must complete or report busy, never fail unexpectedly"
    );
    json!({
        "case": kind.identity(),
        "database_size_bytes": size,
        "projection": result_json(&projection_outcomes),
        "direct_sqlite_read_only": result_json(&direct_outcomes),
    })
}

fn benchmark_report() -> Value {
    let cases = [
        CaseKind::Uncontended,
        CaseKind::LargePopulated,
        CaseKind::ActiveWriter,
        CaseKind::ActiveMigration,
    ]
    .into_iter()
    .map(benchmark_case)
    .collect::<Vec<_>>();
    json!({
        "schema": BENCHMARK_SCHEMA,
        "bead": "aub-me5.12",
        "environment": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "family": std::env::consts::FAMILY,
        },
        "sample_count_per_path": SAMPLES_PER_CASE,
        "status_budget": {
            "value": STATUS_BUDGET,
            "reason": "No numeric tolerance is measured or assumed by aub-me5.12; aub-c5m evaluates the recorded distributions against the status no-blocking contract."
        },
        "decision_input": {
            "retaining_projection_is_justified_when": "the direct read records a busy or blocking outcome in any measured contention case",
            "removing_projection_remains_available_when": "the direct read completes within aub-c5m's later stated budget in every measured case",
        },
        "cases": cases,
    })
}

#[test]
fn direct_read_only_and_projection_paths_have_equivalent_typed_reports() {
    let report = benchmark_case(CaseKind::Uncontended);
    assert_eq!(report["case"], "uncontended");
    assert_eq!(report["direct_sqlite_read_only"]["failed"], 0);
}

#[test]
fn active_writer_completes_or_reports_busy_without_hanging() {
    let report = benchmark_case(CaseKind::ActiveWriter);
    assert_eq!(report["direct_sqlite_read_only"]["failed"], 0);
    assert_eq!(
        report["direct_sqlite_read_only"]["samples"],
        SAMPLES_PER_CASE
    );
}

#[test]
fn active_migration_completes_or_reports_busy_without_hanging() {
    let report = benchmark_case(CaseKind::ActiveMigration);
    assert_eq!(report["direct_sqlite_read_only"]["failed"], 0);
    assert_eq!(
        report["direct_sqlite_read_only"]["samples"],
        SAMPLES_PER_CASE
    );
}

#[test]
fn report_records_every_required_case_and_refuses_to_invent_a_budget() {
    let report = benchmark_report();
    assert_eq!(report["schema"], BENCHMARK_SCHEMA);
    assert_eq!(
        report["status_budget"]["value"], "unmeasured",
        "the benchmark must record the pending budget instead of inventing one"
    );
    let cases = report["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 4);
    assert_eq!(
        cases
            .iter()
            .map(|case| case["case"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "uncontended",
            "large_populated",
            "active_writer",
            "active_migration",
        ],
        "aub-c5m needs every condition, not a same-sized substitute set"
    );
    for case in cases {
        for path in ["projection", "direct_sqlite_read_only"] {
            assert!(case[path]["p50_latency_ns"].is_number());
            assert!(case[path]["p99_latency_ns"].is_number());
            assert_eq!(case[path]["samples"], SAMPLES_PER_CASE);
        }
        assert!(case["database_size_bytes"].as_u64().unwrap() > 0);
    }
}

#[test]
fn direct_comparator_is_scoped_to_read_only_store_access() {
    let source = include_str!("projection_benchmark.rs");
    let start = source
        .find("fn direct_read_only_status(")
        .expect("the direct comparator must stay named");
    let end = source[start..]
        .find("\n}\n\nfn projection_status")
        .map(|offset| start + offset)
        .expect("the direct comparator must end before projection reading");
    let comparator = &source[start..end];
    assert!(comparator.contains("AccessMode::ReadOnly"));
    assert!(!comparator.contains("AccessMode::ReadWrite"));
    assert!(!comparator.contains("run_migrations"));
    assert!(!comparator.contains("crate::meter"));
}

/// The E2E runner sets the output path and bounds this child. The artifact
/// is therefore attached to the runner's lossless stdout/stderr and step
/// metadata, ready for aub-c5m to review without contacting a provider.
#[test]
#[ignore = "the E2E runner records this bounded benchmark run"]
fn emit_projection_benchmark_json() {
    let output = std::env::var("AUB_PROJECTION_BENCHMARK_OUTPUT")
        .expect("the E2E benchmark must provide an artifact output path");
    let report = benchmark_report();
    let rendered = serde_json::to_string_pretty(&report).unwrap();
    std::fs::write(&output, &rendered).expect("the benchmark artifact must be writable");
    println!("{rendered}");
}
