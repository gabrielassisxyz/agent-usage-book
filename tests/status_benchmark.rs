//! Release-binary latency measurement and the accepted budget for `aub
//! status` (aub-n27.3). `tests/projection_benchmark.rs` (aub-me5.12,
//! aub-c5m) measures the internal projection reader against a direct SQLite
//! comparator; it never spawns the compiled binary, so it cannot see process
//! start cost. This file measures what a user's status line actually pays:
//! a real subprocess, built in release mode, invoked at least a thousand
//! times per case.
//!
//! Four cases, matching aub-c5m's own: an uncontended state directory, a
//! large seeded database, an active writer holding the real ledger, and a
//! migration in flight on that same ledger. `status` never opens SQLite
//! (PLAN.md 16.2, invariant 15, `tests::the_status_function_performs_only_the_status_contract`
//! in src/cli.rs), so all four cases exist to demonstrate, by measurement,
//! that the contended cases cost the same as the uncontended one -- not to
//! discover a difference the structural test already forbids.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::TransactionBehavior;
use serde_json::{Value, json};

use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::migrate::{Migration, run_migrations};
use agent_usage_book::store::projection_source::test_support::{Fixture, fixture};

const BENCHMARK_SCHEMA: &str = "aub.status_benchmark.v1";
const SAMPLES_PER_CASE: usize = 1_000;
// Large enough to expose the shape of a big projection document without
// turning this into a multi-minute run; matches the account count
// `tests/projection_benchmark.rs` already uses for the same reason.
const LARGE_ACCOUNT_COUNT: usize = 256;
// A bound on a stuck benchmark, not a pacing sleep: the writer and the
// migration are released explicitly, right after the last of
// `SAMPLES_PER_CASE` subprocesses returns, never on a fixed timer. 1000
// release-binary invocations measured on this machine take about a second
// (see the recorded artifact), so 60s is generous headroom for a slower
// CI runner, not an expected hold time.
const LOCK_TIMEOUT: Duration = Duration::from_secs(60);

/// The accepted p99 budget for one release-binary `aub status` invocation,
/// wall clock, including process start (PLAN.md section 39 "Status": one
/// projection-file read, local freshness computation, formatting, no
/// network; history size and lock state are irrelevant to this path).
///
/// Chosen from measurement, not assumed: this bead's own uncontended
/// baseline on linux/x86_64 (see `emit_status_benchmark_json`'s recorded
/// artifact) is p50 ~1.0ms, p95 ~1.2ms, p99 ~1.4ms over 1000 real
/// invocations. This budget sits roughly ten times above that observed p99,
/// wide enough to survive a slower or more loaded CI runner and a second
/// platform without turning into noise, while staying tight enough that a
/// status path that started opening SQLite or reaching the network (which
/// this benchmark's own direct-read comparator in
/// `tests/projection_benchmark.rs` shows costs low milliseconds even
/// uncontended, and far more once a writer or a migration is holding the
/// database) would still overrun it. A future revision changes this number
/// only from a new measurement with recorded rationale
/// (AGENTS.md "Recovery path"); removing the assertion is not a revision.
const STATUS_P99_BUDGET_NS: u128 = 15_000_000;

static MIGRATION_STARTED: AtomicBool = AtomicBool::new(false);
static MIGRATION_RELEASE: AtomicBool = AtomicBool::new(false);

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

/// Writes the config a real `aub status` subprocess resolves against: the
/// fixture's own scratch directory as the state directory (so the projection
/// file the fixture publishes is exactly the one the binary reads), one
/// configured account matching the fixture's seeded account.
fn write_config_for_fixture(fixture_dir: &Path) -> PathBuf {
    let config_path = fixture_dir.join("aub.toml");
    std::fs::write(
        &config_path,
        format!(
            "state.dir = \"{}\"\n\n[[accounts]]\nname = \"work\"\nprovider = \"anthropic\"\n",
            fixture_dir.display()
        ),
    )
    .expect("the benchmark config must be writable");
    config_path
}

fn publish_seed(fixture: &Fixture) {
    let publication =
        agent_usage_book::projection::publish(&fixture.conn, &fixture.projection_path());
    assert!(
        publication.published_generation().is_some(),
        "the benchmark seed must publish a readable projection: {publication:?}"
    );
}

fn seed_large_database(fixture: &mut Fixture) {
    fixture.seed_additional_accounts(LARGE_ACCOUNT_COUNT);
    publish_seed(fixture);
}

fn run_status_once(config_path: &Path, home_dir: &Path) -> u128 {
    let started = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_aub"))
        .env("HOME", home_dir)
        .env("AUB_CONFIG_FILE", config_path)
        .arg("status")
        .output()
        .expect("the release binary must spawn");
    let elapsed_ns = started.elapsed().as_nanos();
    assert!(
        output.status.success(),
        "status must exit zero: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    elapsed_ns
}

fn percentile(sorted: &[u128], numerator: usize, denominator: usize) -> u128 {
    let index = (sorted.len() * numerator)
        .div_ceil(denominator)
        .saturating_sub(1);
    sorted[index.min(sorted.len() - 1)]
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

/// Holds a real write transaction open on the fixture's ledger database
/// until the returned sender is used, so every sampled `status` invocation
/// in the caller's loop races an actual writer -- the same database status
/// never opens.
fn hold_writer(database_path: &Path) -> (mpsc::Sender<()>, thread::JoinHandle<()>) {
    let path = database_path.to_path_buf();
    let started = std::sync::Arc::new(AtomicBool::new(false));
    let child_started = std::sync::Arc::clone(&started);
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
        release_rx.recv_timeout(LOCK_TIMEOUT).unwrap();
        transaction.commit().unwrap();
    });
    wait_until(&started, "writer lock");
    (release_tx, handle)
}

fn hold_benchmark_migration_lock(
    connection: &rusqlite::Connection,
) -> Result<(), agent_usage_book::error::Error> {
    connection
        .execute_batch("CREATE TABLE status_benchmark_migration_probe (id INTEGER PRIMARY KEY)")
        .map_err(|error| {
            agent_usage_book::error::Error::Store(format!(
                "cannot create status benchmark migration probe: {error}"
            ))
        })?;
    MIGRATION_STARTED.store(true, Ordering::Release);
    let deadline = Instant::now() + LOCK_TIMEOUT;
    while !MIGRATION_RELEASE.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "migration release did not arrive within the bound"
        );
        thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}

/// Holds a real migration open on the fixture's ledger database, in the
/// same way, until [`release_migration`] is called.
fn hold_migration(database_path: &Path) -> thread::JoinHandle<()> {
    MIGRATION_STARTED.store(false, Ordering::Release);
    MIGRATION_RELEASE.store(false, Ordering::Release);
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

fn release_migration(handle: thread::JoinHandle<()>) {
    MIGRATION_RELEASE.store(true, Ordering::Release);
    handle.join().unwrap();
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

    let config_path = write_config_for_fixture(fixture._scratch.path());
    let home_dir = fixture._scratch.path().join("home");
    std::fs::create_dir_all(&home_dir).unwrap();
    let database_path = fixture.database_path();
    let database_size_bytes = std::fs::metadata(&database_path).unwrap().len();

    let mut writer = None;
    let mut migration = None;
    match kind {
        CaseKind::ActiveWriter => writer = Some(hold_writer(&database_path)),
        CaseKind::ActiveMigration => migration = Some(hold_migration(&database_path)),
        CaseKind::Uncontended | CaseKind::LargePopulated => {}
    }

    let mut latencies_ns = Vec::with_capacity(SAMPLES_PER_CASE);
    for _ in 0..SAMPLES_PER_CASE {
        latencies_ns.push(run_status_once(&config_path, &home_dir));
    }

    if let Some((release, handle)) = writer {
        release.send(()).unwrap();
        handle.join().unwrap();
    }
    if let Some(handle) = migration {
        release_migration(handle);
    }

    latencies_ns.sort_unstable();
    json!({
        "case": kind.identity(),
        "database_size_bytes": database_size_bytes,
        "samples": latencies_ns.len(),
        "min_latency_ns": latencies_ns[0],
        "p50_latency_ns": percentile(&latencies_ns, 50, 100),
        "p95_latency_ns": percentile(&latencies_ns, 95, 100),
        "p99_latency_ns": percentile(&latencies_ns, 99, 100),
        "max_latency_ns": latencies_ns[latencies_ns.len() - 1],
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
        "bead": "aub-n27.3",
        "environment": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "family": std::env::consts::FAMILY,
        },
        "sample_count_per_case": SAMPLES_PER_CASE,
        "status_p99_budget_ns": STATUS_P99_BUDGET_NS,
        "cases": cases,
    })
}

/// A pure comparator, tested on its own so the enforcement logic is proven
/// independently of a slow subprocess run: lowering the accepted budget
/// below an already-observed result makes the comparison fail, which is
/// exactly what makes CI fail when `emit_status_benchmark_json` runs this
/// same comparison against a real measurement below.
fn latency_within_budget(observed_p99_ns: u128, budget_ns: u128) -> bool {
    observed_p99_ns <= budget_ns
}

#[test]
fn lowering_the_accepted_budget_below_a_recorded_observation_fails_the_comparison() {
    // This bead's own recorded uncontended p99 (see the module doc comment
    // and the artifact `emit_status_benchmark_json` writes).
    let observed_p99_ns: u128 = 1_400_000;
    assert!(
        latency_within_budget(observed_p99_ns, STATUS_P99_BUDGET_NS),
        "the accepted budget must cover the recorded observation"
    );
    assert!(
        !latency_within_budget(observed_p99_ns, observed_p99_ns - 1),
        "a budget set below an observed result must fail the comparison"
    );
}

/// The E2E runner sets the output path and bounds this child, exactly as
/// `tests/projection_benchmark.rs`'s `emit_projection_benchmark_json` does.
/// Run with `--release` (via `bin/checks/85-status-latency-budget`), this
/// is also the CI enforcement point: the final assertion fails the process,
/// and therefore the check, the moment the uncontended case's own p99
/// exceeds the accepted budget.
#[test]
#[ignore = "spawns 4000 real release-binary subprocesses; the E2E runner and the CI budget check record this bounded run"]
fn emit_status_benchmark_json() {
    let report = benchmark_report();
    let rendered = serde_json::to_string_pretty(&report).unwrap();
    if let Ok(output) = std::env::var("AUB_STATUS_BENCHMARK_OUTPUT") {
        std::fs::write(&output, &rendered).expect("the benchmark artifact must be writable");
    }
    println!("{rendered}");

    let cases = report["cases"].as_array().unwrap();
    assert_eq!(
        cases.len(),
        4,
        "every contention condition must be recorded, not a same-sized substitute set"
    );
    for case in cases {
        assert_eq!(case["samples"], SAMPLES_PER_CASE);
        assert!(case["p50_latency_ns"].is_number());
        assert!(case["p95_latency_ns"].is_number());
        assert!(case["p99_latency_ns"].is_number());
    }

    // Every case is held to the same budget, not just the uncontended one:
    // the whole point of measuring `large_populated`, `active_writer` and
    // `active_migration` is that none of them should cost more than the
    // baseline. A budget that only watched the uncontended case would miss
    // exactly the regression this bead exists to catch -- status starting
    // to wait on the database.
    for case in cases {
        let name = case["case"].as_str().unwrap();
        let p99 = case["p99_latency_ns"].as_u64().unwrap() as u128;
        assert!(
            latency_within_budget(p99, STATUS_P99_BUDGET_NS),
            "case {name}'s p99 ({p99}ns) exceeded the accepted budget ({STATUS_P99_BUDGET_NS}ns); \
             either status started doing more work, or the budget needs a new measurement and recorded rationale"
        );
    }
}
