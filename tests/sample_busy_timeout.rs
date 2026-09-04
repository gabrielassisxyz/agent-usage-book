//! Integration proof for `aub-va6s`: a sampler that opens the store with its
//! configured busy timeout gets through a batched ingest's brief writer-slot
//! holds without ever exiting refused, and a sampler that waits past a lock
//! genuinely held the whole time still refuses, naming how long it waited.
//!
//! The stub ingest here reproduces the shape `crate::ingest::run`'s bounded
//! batches actually produce (a short `BEGIN IMMEDIATE` hold, then the writer
//! slot released) without the parsing machinery around it, so the schedule
//! is deterministic and the test runs in milliseconds rather than the
//! minutes a real corpus would take.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration as StdDuration;

use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::store::connection::{AccessMode, LEDGER_DATABASE_FILE, PragmaPolicy, open};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations;
use agent_usage_book::store::sample_run::{Trigger, start_sample_run};
use test_support::StateDir;

/// A migrated ledger at `state_dir`'s conventional database path, ready for
/// both the stub ingest thread and the sampler loop to open their own
/// connections against.
fn migrated_db_path(state_dir: &Path) -> PathBuf {
    let db_path = state_dir.join(LEDGER_DATABASE_FILE);
    let policy = PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(1_000),
    };
    let mut conn = open(&db_path, AccessMode::ReadWrite, &policy).expect("ledger must open");
    run_migrations(
        &mut conn,
        &migrations::registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
    )
    .expect("ledger must migrate");
    db_path
}

/// A stub batched ingest: takes the write lock for `hold`, releases it,
/// waits `yield_for`, repeats for `iterations` cycles or until `stop` is set.
/// Mirrors `crate::ingest::run`'s own pattern (`IngestPass` commits, then
/// `INTER_BATCH_YIELD`) at the level that actually contends for the writer
/// slot.
fn run_stub_batched_ingest(
    db_path: &Path,
    hold: StdDuration,
    yield_for: StdDuration,
    iterations: u32,
    stop: &AtomicBool,
) {
    let policy = PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(1_000),
    };
    for _ in 0..iterations {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let mut conn = open(db_path, AccessMode::ReadWrite, &policy)
            .expect("stub ingest connection must open");
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .expect("stub ingest batch must acquire the writer slot");
        thread::sleep(hold);
        tx.commit().expect("stub ingest batch must commit");
        thread::sleep(yield_for);
    }
}

/// **Test 1 (acceptance criterion 3):** with a stub ingest holding the write
/// lock in short batches and a sampler ticking throughout, every sample
/// attempt within the configured busy timeout succeeds; none is the refusal
/// `aub-va6s` was filed over.
#[test]
fn every_sample_tick_gets_through_a_batched_ingest_within_the_busy_timeout() {
    let state = StateDir::new();
    let db_path = migrated_db_path(state.path());

    let stop = Arc::new(AtomicBool::new(false));
    let ingest_stop = Arc::clone(&stop);
    let ingest_db_path = db_path.clone();
    let ingest_thread = thread::spawn(move || {
        // 25 batches of an 80ms hold and a 20ms yield: about 2.5s of a
        // corpus-sized pass's writer-slot pattern compressed for test speed,
        // long enough to span every sampler tick below.
        run_stub_batched_ingest(
            &ingest_db_path,
            StdDuration::from_millis(80),
            StdDuration::from_millis(20),
            25,
            &ingest_stop,
        );
    });

    // The sampler's own busy timeout: what `sample_command` now opens the
    // store with (`config.sampling.busy_timeout`), generous enough to
    // outlast one batch's hold, unlike the hardcoded 500ms this bead fixed
    // to be exactly this instead of a hardcoded value below any batch hold.
    let sampler_policy = PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(500),
    };
    let mut refusals: Vec<String> = Vec::new();
    for tick in 0..20u32 {
        let conn = open(&db_path, AccessMode::ReadWrite, &sampler_policy)
            .expect("sampler connection must open");
        if let Err(error) = start_sample_run(
            &conn,
            Trigger::Timer,
            UtcTimestamp::from_unix_nanos(tick as i64),
            "aub-va6s-test",
        ) {
            refusals.push(error.to_string());
        }
        thread::sleep(StdDuration::from_millis(60));
    }
    stop.store(true, Ordering::Relaxed);
    ingest_thread
        .join()
        .expect("stub ingest thread must not panic");

    assert!(
        refusals.is_empty(),
        "no sample tick may exit refused while the ingest holds the lock only in short batches: {refusals:?}"
    );
}

/// **Test 2 (acceptance criterion 2, the kept half):** a sampler waiting past
/// a lock genuinely held for the whole busy timeout still refuses. The
/// refusal path itself is unchanged; only the zero wait this bead removed is
/// gone.
#[test]
fn a_sampler_still_refuses_when_the_lock_outlasts_the_busy_timeout() {
    let state = StateDir::new();
    let db_path = migrated_db_path(state.path());

    let busy_timeout = MonotonicDuration::from_millis(200);
    let mut holder = open(
        &db_path,
        AccessMode::ReadWrite,
        &PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1_000),
        },
    )
    .expect("holder connection must open");
    let held = holder
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .expect("holder must acquire the writer slot");

    let sampler_policy = PragmaPolicy { busy_timeout };
    let conn = open(&db_path, AccessMode::ReadWrite, &sampler_policy)
        .expect("sampler connection must open");
    let started = std::time::Instant::now();
    let error = start_sample_run(
        &conn,
        Trigger::Timer,
        UtcTimestamp::from_unix_nanos(0),
        "aub-va6s-test",
    )
    .expect_err("a lock held past the busy timeout must still refuse");
    let waited = started.elapsed();
    drop(held);

    assert!(
        waited >= StdDuration::from_millis(180),
        "the refusal must wait roughly the configured busy timeout, not return instantly: waited {waited:?}"
    );
    assert!(
        error.to_string().contains("database is locked") || error.to_string().contains("busy"),
        "the refusal must be the SQLite busy class: {error}"
    );
}
