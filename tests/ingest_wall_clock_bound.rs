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

use agent_usage_book::domain::time::{
    Clock, FakeClock, MonotonicDuration, MonotonicInstant, RealClock, UtcTimestamp,
};
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::ingest::{IngestPass, PersistEvent, persist_ingest_batch};

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

/// Scheduling slack for the wall-clock slot bound: jitter between the clock
/// check and the commit return plus the commit fsync for a one-or-two-row
/// WAL transaction. Fixed at the 60 ms the case has always used; IO latency
/// is measured in the observed item cost, never absorbed here.
const WALL_CLOCK_SCHEDULING_SLACK: Duration = Duration::from_millis(60);

/// The allowance for one bounded transaction: the configured bound, plus one
/// more item at the cost this run actually observed, plus scheduling slack.
///
/// The slack covers thread-scheduling jitter between the clock check and the
/// commit return, plus the commit fsync for a one-or-two-row WAL transaction.
/// It does not cover IO latency: latency that stretches an item (a sleep that
/// wakes late, an insert stuck behind a disk queue) is part of that item's
/// observed cost and is measured in `observed_max_item`, never absorbed by
/// widening this slack.
fn wall_clock_slot_allowance(
    bound: MonotonicDuration,
    observed_max_item: Duration,
    slack: Duration,
) -> Duration {
    Duration::from_nanos(bound.as_nanos()) + observed_max_item + slack
}

/// Lands `item_count` stub events, `per_item_cost` apart, in transactions no
/// longer than `max_batch_seconds` (`None` reproduces the pre-`aub-mh1c`
/// shape: the whole run in one transaction, however long that takes). Mirrors
/// `persist_ingest_batch`'s own loop shape: at least one item always lands
/// before the wall-clock check can close a transaction, so the writer always
/// makes progress. `on_commit` is called after every transaction commits with
/// two durations: how long the transaction held the slot from acquiring it to
/// the start of `commit` (what the loop controls), and how long the commit
/// itself then took (what the loop cannot shorten), so a caller can assert on
/// the first while naming the second.
///
/// Returns the largest per-item wall-clock cost the run observed (each item's
/// sleep plus its insert, timed together), so the caller builds its allowance
/// from what this run cost rather than from the configured per-item value.
/// A slow machine stretches the observed item, not the verdict.
fn run_stub_ingest(
    conn: &mut rusqlite::Connection,
    item_count: u64,
    per_item_cost: Duration,
    max_batch_seconds: Option<MonotonicDuration>,
    on_commit: impl FnMut(Duration, Duration),
) -> Duration {
    run_wall_clock_stub_with_schedule(
        conn,
        item_count,
        |_| per_item_cost,
        max_batch_seconds,
        on_commit,
    )
}

/// The schedule-driven form of [`run_stub_ingest`]: item `i` (zero-based,
/// counting across transactions) sleeps `item_cost(i)` before its insert, so
/// a caller can drive uneven per-item costs. Returns the largest per-item
/// wall-clock cost observed, timed around each item's sleep plus insert.
fn run_wall_clock_stub_with_schedule(
    conn: &mut rusqlite::Connection,
    item_count: u64,
    mut item_cost: impl FnMut(u64) -> Duration,
    max_batch_seconds: Option<MonotonicDuration>,
    mut on_commit: impl FnMut(Duration, Duration),
) -> Duration {
    let clock = RealClock::new();
    let mut landed = 0u64;
    let mut first_transaction = true;
    let mut observed_max_item = Duration::ZERO;
    loop {
        if !first_transaction {
            std::thread::sleep(INTER_BATCH_YIELD);
        }
        first_transaction = false;

        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .expect("the stub writer must acquire the slot");
        let slot_start = clock.monotonic_now();
        let acquired = Instant::now();
        let mut landed_this_transaction = 0u64;
        while landed < item_count {
            if landed_this_transaction > 0
                && let Some(bound) = max_batch_seconds
                && clock.monotonic_now().duration_since(slot_start) >= bound
            {
                break;
            }
            let cost = item_cost(landed);
            let item_start = Instant::now();
            std::thread::sleep(cost);
            tx.execute(
                "INSERT INTO stub_item (id) VALUES (?1)",
                rusqlite::params![landed as i64],
            )
            .unwrap();
            observed_max_item = observed_max_item.max(item_start.elapsed());
            landed += 1;
            landed_this_transaction += 1;
        }
        // What the loop controls ends here: the hold runs from acquiring the
        // slot to the start of the commit. The commit's own fsync follows and
        // is reported separately, because the loop cannot shorten it (`aub-p7o9`).
        let hold = acquired.elapsed();
        let commit_start = Instant::now();
        tx.commit().unwrap();
        on_commit(hold, commit_start.elapsed());
        if landed >= item_count {
            break;
        }
    }
    observed_max_item
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
/// configured bound plus one more item at the cost this run observed (the
/// loop can only check the clock between items, never mid-item): the direct,
/// deterministic form of "a batch that hits it commits what it has"
/// (`aub-mh1c`). This needs no concurrent reader to observe: the writer's own
/// measured hold is the proof.
///
/// The hold runs from acquiring the slot to the start of `commit`, never
/// past it: the commit's own fsync is what the loop cannot shorten, so it is
/// recorded beside the hold rather than scored against the allowance
/// (`aub-p7o9`, after three landing gates and one Actions run went red with
/// the per-item cost steady and all the excess in the commit).
///
/// The allowance is the bound plus the largest per-item cost measured during
/// this run plus a fixed scheduling slack. Under IO pressure an item costs
/// more than its configured sleep (the write waits behind a disk queue, the
/// sleep wakes late), so scoring against the configured cost mistakes a slow
/// machine for an overrun. Scoring against the observed cost keeps the
/// verdict on the code: a loop that checks the clock at every legal
/// opportunity still passes when loaded, while a loop that skips its check
/// holds for at least one extra observed item and still fails.
#[test]
fn no_transaction_holds_the_slot_past_the_bound_plus_one_item() {
    let (_db_path, mut writer) = stub_db("bounded-duration");
    let per_item = Duration::from_millis(80);
    let bound = MonotonicDuration::from_millis(150);
    let slack = WALL_CLOCK_SCHEDULING_SLACK; // scheduling slop, not the mechanism
    let mut holds = Vec::new();
    let observed_max_item =
        run_stub_ingest(&mut writer, 15, per_item, Some(bound), |hold, commit| {
            holds.push((hold, commit));
        });
    let allowance = wall_clock_slot_allowance(bound, observed_max_item, slack);
    let mut max_seen = Duration::ZERO;
    for (hold, commit) in holds {
        max_seen = max_seen.max(hold);
        assert!(
            hold <= allowance,
            "one transaction held the slot {hold:?} (commit took {commit:?}), over the {bound:?} bound plus observed one-item cost {observed_max_item:?} plus slack {slack:?} (allowance {allowance:?})"
        );
    }
    assert!(
        max_seen >= Duration::from_millis(150),
        "the bound must actually have been exercised: the longest transaction was only {max_seen:?}"
    );
}

/// The allowance is the bound plus the largest item cost the run observed
/// plus the slack, not the bound plus the configured per-item value. Proved
/// in two parts: the pure arithmetic against known uneven costs (no timing
/// involved), and the stub plumbing against a stub whose scheduled item costs
/// are known and uneven (the observed maximum must cover the largest
/// scheduled cost, because each sleep lasts at least its scheduled length).
#[test]
fn wall_clock_slot_allowance_uses_the_largest_observed_item_cost() {
    assert!(
        WALL_CLOCK_SCHEDULING_SLACK <= Duration::from_millis(60),
        "the scheduling slack must stay at or below its 60 ms value: {WALL_CLOCK_SCHEDULING_SLACK:?}"
    );
    let bound = MonotonicDuration::from_millis(150);
    let slack = WALL_CLOCK_SCHEDULING_SLACK;
    let known = [
        Duration::from_millis(20),
        Duration::from_millis(200),
        Duration::from_millis(50),
    ];
    let largest = known.into_iter().max().unwrap();
    assert_eq!(
        wall_clock_slot_allowance(bound, largest, slack),
        Duration::from_nanos(bound.as_nanos()) + largest + slack,
        "the allowance must be exactly bound plus largest observed item plus slack"
    );
    let old_allowance = Duration::from_nanos(bound.as_nanos()) + Duration::from_millis(80) + slack;
    assert!(
        wall_clock_slot_allowance(bound, largest, slack) > old_allowance,
        "with uneven costs the observed allowance must exceed the old configured-cost allowance"
    );

    let (_db_path, mut writer) = stub_db("uneven-costs");
    let schedule = [
        Duration::from_millis(5),
        Duration::from_millis(25),
        Duration::from_millis(10),
    ];
    let scheduled_largest = schedule.iter().max().copied().unwrap();
    let observed = run_wall_clock_stub_with_schedule(
        &mut writer,
        9,
        |i| schedule[(i as usize) % schedule.len()],
        Some(bound),
        |_, _| {},
    );
    assert!(
        observed >= scheduled_largest,
        "the observed maximum {observed:?} must cover the largest scheduled cost {scheduled_largest:?}"
    );
    assert_eq!(
        wall_clock_slot_allowance(bound, observed, slack),
        Duration::from_nanos(bound.as_nanos()) + observed + slack,
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
    run_stub_ingest(&mut writer, item_count, per_item, None, |hold, _| {
        commits += 1;
        only_duration = hold;
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
        |_, _| {
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

    run_stub_ingest(&mut writer, 15, Duration::from_millis(80), None, |_, _| {});

    let result = waiter.join().unwrap();
    assert!(
        result.is_err(),
        "a waiting connection must be refused when nothing bounds the writer's transaction"
    );
}

/// One strong-identity persist event for the production-function bound test
/// below, built through the shared identity framework exactly the
/// orchestrator builds them, so the batch under test carries rows the real
/// path would produce.
fn production_persist_event(id: &str, file: &str, occurred_nanos: i64) -> PersistEvent {
    use agent_usage_book::dedup::{canonical_identity, canonical_payload_digest};
    use agent_usage_book::domain::ids::{NativeSessionId, SessionId, SourceNamespace};
    use agent_usage_book::domain::tokens::{
        CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, UsageVector,
    };
    use agent_usage_book::evidence::{CoverageCompleteness, EvidenceQuality, Provenance};
    use agent_usage_book::transcripts::NormalizedUsageEvent;
    use agent_usage_book::transcripts::parser::{
        EvidenceClassification, ParserVersion, STRONG_IDENTITY_PREFIX,
    };

    let event = NormalizedUsageEvent::new(
        UsageVector::new(
            KnownTokenVector::new(
                InputTokens::new(10),
                OutputTokens::new(5),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(0),
            ),
            std::collections::BTreeMap::new(),
            CoverageCompleteness::Complete,
            EvidenceQuality::Measured,
        ),
        EvidenceClassification::Reported,
        Provenance::new(vec![
            file.to_string(),
            format!("{STRONG_IDENTITY_PREFIX}{id}"),
        ]),
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
        relative_path: Some(file.to_string()),
    }
}

/// A clock that advances by a fixed step every time its monotonic time is
/// read, so the production persist loop's own per-event check sees
/// wall-clock time pass without a real sleep.
struct ProductionTickingClock {
    state: std::cell::Cell<FakeClock>,
    step: MonotonicDuration,
}

impl ProductionTickingClock {
    fn new(step: MonotonicDuration) -> Self {
        Self {
            state: std::cell::Cell::new(FakeClock::new(UtcTimestamp::from_unix_nanos(0))),
            step,
        }
    }
}

impl Clock for ProductionTickingClock {
    fn now(&self) -> UtcTimestamp {
        self.state.get().now()
    }

    fn monotonic_now(&self) -> MonotonicInstant {
        let mut clock = self.state.get();
        let instant = clock.monotonic_now();
        clock.advance(self.step);
        self.state.set(clock);
        instant
    }
}

/// `persist_ingest_batch` itself stops at the wall-clock bound (`aub-p7o9`),
/// not only the stub above: with an injected clock that advances past
/// `max_batch_seconds` mid-batch, the call lands at least one event, stops
/// before the rest, and the outcome reports how many landed so the caller can
/// retry with the remainder. The stub test stays, since it proves the
/// concurrent-reader side this production-function test does not.
#[test]
fn persist_ingest_batch_stops_at_the_wall_clock_bound_mid_batch() {
    let dir = scratch("production-bound");
    let db_path = dir.join("ledger.db");
    let mut conn = test_support::open_migrated(&db_path, &policy(Duration::from_secs(30)));

    let events = vec![
        production_persist_event("m1", "corpus/a.jsonl", 1_000),
        production_persist_event("m2", "corpus/a.jsonl", 2_000),
        production_persist_event("m3", "corpus/b.jsonl", 3_000),
    ];
    let total = events.len();
    let pass = IngestPass {
        events,
        sessions: Vec::new(),
        watermarks: Vec::new(),
        quarantined: Vec::new(),
        collisions: Vec::new(),
        whole_file_sources: Vec::new(),
        created_at: UtcTimestamp::from_unix_nanos(1_000_000),
    };

    // The clock advances a full second on every read against a 500 ms bound.
    // The loop never checks before the first event, so one always lands; the
    // check before the second already reads past the budget and cuts there.
    let clock = ProductionTickingClock::new(MonotonicDuration::from_millis(1_000));
    let outcome = persist_ingest_batch(
        &mut conn,
        &pass,
        &clock,
        MonotonicDuration::from_millis(500),
    )
    .expect("the bounded production batch must land");
    let landed =
        (outcome.events_written.value() + outcome.events_already_ingested.value()) as usize;
    assert!(
        landed >= 1 && landed < total,
        "the wall-clock bound must cut the production batch after at least one event but before the rest: landed={landed} of total={total}"
    );

    // The caller's retry carries only the events the first call never
    // reached; a clock that never advances and a generous budget let it land.
    let remaining = IngestPass {
        events: pass.events[landed..].to_vec(),
        sessions: Vec::new(),
        watermarks: Vec::new(),
        quarantined: Vec::new(),
        collisions: Vec::new(),
        whole_file_sources: Vec::new(),
        created_at: UtcTimestamp::from_unix_nanos(1_000_000),
    };
    let remainder = total - landed;
    let second = persist_ingest_batch(
        &mut conn,
        &remaining,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        MonotonicDuration::from_seconds(2),
    )
    .expect("the retry must land");
    assert_eq!(
        second.events_written.value() as usize,
        remainder,
        "the retry must land exactly the remainder the first call reported: landed={landed} remainder={remainder} of total={total}"
    );
    let stored: i64 = conn
        .query_row("SELECT COUNT(*) FROM usage_event", [], |row| row.get(0))
        .expect("the event count must be readable");
    assert_eq!(
        stored as usize, total,
        "every event lands exactly once across the cut-short call and its retry: stored={stored} total={total}"
    );
}
