//! `ingest.max_batch_seconds` (`aub-mh1c`) against a stub writer whose
//! per-event cost is inflated with a real sleep, standing in for the
//! profiled defect this bead fixes: a batch whose per-event cost is far
//! above what an ordinary SQLite insert costs.
//!
//! The claim under proof is general and does not depend on the real ingest
//! pipeline's own SQL: a writer that bounds its own transaction by wall
//! clock, yielding the slot between transactions, never holds the slot past
//! that bound, so a concurrent busy-timeout reader is served within its wait
//! however slow the writer's own per-item cost turns out to be. A writer that
//! does not bound itself holds the slot for the whole run instead, which is
//! exactly the gap `aub-va6s` left open (a batch sized only in events could
//! still run for minutes) and this bead closes. The stub reproduces the
//! shape `persist_ingest_batch` implements (`src/store/ingest.rs`) at a scale
//! this test can run in seconds: a real `BEGIN IMMEDIATE` transaction, a real
//! per-item sleep, and a real wall-clock check between items.
//!
//! The per-transaction duration each call actually held is measured directly
//! (no concurrency needed to observe it), which is the primary, deterministic
//! proof the bound holds. A single well-timed concurrent attempt then
//! confirms the same property from a waiting connection's own side. A tight
//! retry loop racing the writer at a similar period was tried first and
//! rejected: two near-periodic processes (the writer's cycle and SQLite's own
//! busy-handler poll schedule) can alias against each other and starve the
//! waiter for reasons that have nothing to do with the bound under test,
//! which is exactly the kind of flakiness that must not be treated as
//! evidence either way.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use agent_usage_book::domain::time::{Clock, MonotonicDuration, RealClock};
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};

fn scratch(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("aub-mh1c-wall-clock-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn policy(busy: Duration) -> PragmaPolicy {
    PragmaPolicy {
        busy_timeout: MonotonicDuration::from_nanos(busy.as_nanos() as u64),
    }
}

/// The interval the stub yields the writer slot for between two consecutive
/// transactions, mirroring `INTER_BATCH_YIELD` in `src/ingest.rs`.
const INTER_BATCH_YIELD: Duration = Duration::from_millis(5);

/// Lands `item_count` stub events, `per_item_cost` apart, in transactions no
/// longer than `max_batch_seconds` (`None` reproduces the pre-`aub-mh1c`
/// shape: the whole run in one transaction, however long that takes). Mirrors
/// `persist_ingest_batch`'s own loop shape: at least one item always lands
/// before the wall-clock check can close a transaction, so the writer always
/// makes progress. `on_commit` is called after every transaction commits,
/// with that transaction's own real held duration, so a caller can both
/// assert on it and use it to synchronize a concurrent attempt.
fn run_stub_ingest(
    conn: &mut rusqlite::Connection,
    item_count: u64,
    per_item_cost: Duration,
    max_batch_seconds: Option<MonotonicDuration>,
    mut on_commit: impl FnMut(Duration),
) {
    let clock = RealClock::new();
    let mut landed = 0u64;
    let mut first_transaction = true;
    loop {
        if !first_transaction {
            std::thread::sleep(INTER_BATCH_YIELD);
        }
        first_transaction = false;

        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .expect("the stub writer must acquire the slot");
        let slot_start = clock.monotonic_now();
        let wall_start = Instant::now();
        let mut landed_this_transaction = 0u64;
        while landed < item_count {
            if landed_this_transaction > 0
                && let Some(bound) = max_batch_seconds
                && clock.monotonic_now().duration_since(slot_start) >= bound
            {
                break;
            }
            std::thread::sleep(per_item_cost);
            tx.execute(
                "INSERT INTO stub_item (id) VALUES (?1)",
                rusqlite::params![landed as i64],
            )
            .unwrap();
            landed += 1;
            landed_this_transaction += 1;
        }
        tx.commit().unwrap();
        on_commit(wall_start.elapsed());
        if landed >= item_count {
            break;
        }
    }
}

fn stub_db(tag: &str) -> (PathBuf, rusqlite::Connection) {
    let dir = scratch(tag);
    let db_path = dir.join("stub.db");
    let conn = open(
        &db_path,
        AccessMode::ReadWrite,
        &policy(Duration::from_secs(30)),
    )
    .unwrap();
    conn.execute("CREATE TABLE stub_item (id INTEGER PRIMARY KEY)", [])
        .unwrap();
    (db_path, conn)
}

/// No transaction the stub commits ever holds the slot for longer than the
/// configured bound plus one more item's cost (the loop can only check the
/// clock between items, never mid-item): the direct, deterministic form of
/// "a batch that hits it commits what it has" (`aub-mh1c`). This needs no
/// concurrent reader to observe: the writer's own measured hold is the proof.
#[test]
fn no_transaction_holds_the_slot_past_the_bound_plus_one_item() {
    let (_db_path, mut writer) = stub_db("bounded-duration");
    let per_item = Duration::from_millis(80);
    let bound = MonotonicDuration::from_millis(150);
    let slack = Duration::from_millis(60); // scheduling slop, not the mechanism
    let mut max_seen = Duration::ZERO;
    run_stub_ingest(&mut writer, 15, per_item, Some(bound), |held| {
        max_seen = max_seen.max(held);
        assert!(
            held <= Duration::from_nanos(bound.as_nanos()) + per_item + slack,
            "one transaction held the slot {held:?}, over the {bound:?} bound plus one item's cost"
        );
    });
    assert!(
        max_seen >= Duration::from_millis(150),
        "the bound must actually have been exercised: the longest transaction was only {max_seen:?}"
    );
}

/// The planted negative: with no bound at all, the whole run lands in one
/// transaction, so its held duration is at least the full run's worth of
/// per-item cost, not the small bound above. Proves the assertion above is
/// measuring the mechanism and not a tautology that any duration would pass.
#[test]
fn without_a_bound_the_whole_run_lands_in_one_unbounded_transaction() {
    let (_db_path, mut writer) = stub_db("unbounded-duration");
    let per_item = Duration::from_millis(80);
    let item_count = 15u64;
    let mut commits = 0u64;
    let mut only_duration = Duration::ZERO;
    run_stub_ingest(&mut writer, item_count, per_item, None, |held| {
        commits += 1;
        only_duration = held;
    });
    assert_eq!(commits, 1, "with no bound the whole run is one transaction");
    assert!(
        only_duration >= per_item * (item_count as u32),
        "the single transaction must hold for the full run: {only_duration:?}"
    );
}

/// A concurrent connection waiting on its own busy timeout is served once the
/// wall-clock bound is in place: started right after the writer's first
/// commit (guaranteed mid-run, not a guess), its own wait comfortably covers
/// one more bounded transaction.
#[test]
fn a_waiting_connection_is_served_once_the_wall_clock_bound_is_in_place() {
    let (db_path, mut writer) = stub_db("bounded-liveness");
    let (fire_tx, fire_rx) = mpsc::channel::<()>();
    let mut fire_tx = Some(fire_tx);

    let waiter = std::thread::spawn(move || {
        fire_rx
            .recv()
            .expect("the writer must signal after its first commit");
        // 1.5s comfortably covers one more ~150ms-bounded transaction even
        // under generous scheduling slack, without racing the writer's cycle.
        let mut conn = open(
            &db_path,
            AccessMode::ReadWrite,
            &policy(Duration::from_millis(1_500)),
        )
        .unwrap();
        conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map(|tx| tx.commit().unwrap())
    });

    run_stub_ingest(
        &mut writer,
        15,
        Duration::from_millis(80),
        Some(MonotonicDuration::from_millis(150)),
        |_held| {
            if let Some(tx) = fire_tx.take() {
                let _ = tx.send(());
            }
        },
    );

    let result = waiter.join().unwrap();
    assert!(
        result.is_ok(),
        "a waiting connection must be served once the wall-clock bound is in place: {result:?}"
    );
}

/// The planted negative (`aub-va6s`'s own gap): without the wall-clock bound,
/// the same waiting connection, started after the writer's first (and only)
/// insert has already begun the one unbounded transaction, times out.
#[test]
fn a_waiting_connection_is_refused_without_the_wall_clock_bound() {
    let (db_path, mut writer) = stub_db("unbounded-liveness");

    let waiter = std::thread::spawn(move || {
        // Fires almost immediately: the one unbounded transaction holds the
        // slot for the whole run regardless of when the wait starts.
        std::thread::sleep(Duration::from_millis(50));
        let mut conn = open(
            &db_path,
            AccessMode::ReadWrite,
            &policy(Duration::from_millis(300)),
        )
        .unwrap();
        conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map(|tx| tx.commit().unwrap())
    });

    run_stub_ingest(&mut writer, 15, Duration::from_millis(80), None, |_held| {});

    let result = waiter.join().unwrap();
    assert!(
        result.is_err(),
        "a waiting connection must be refused when nothing bounds the writer's transaction"
    );
}
