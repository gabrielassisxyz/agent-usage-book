//! The crash injection harness and write-path crash matrix
//! (`aub-sth.14`, PLAN.md sections 13, 34.7; invariants 23 and 24).
//!
//! Named injection points across the write path, one test per point:
//! 1. Before attempt-start commit (`before-start-commit`)
//! 2. After attempt-start commit and before request returns (`after-start-commit-before-request`)
//! 3. After network parse and before spool write (`after-parse-before-spool-write`)
//! 4. After spool write and before SQLite commit (`after-spool-write-before-sqlite-commit`)
//! 5. After SQLite commit and before pending deletion (`after-sqlite-commit-before-pending-deletion`)
//!
//! Every matrix case asserts an exact observation count after recovery, never
//! merely a non-zero one: a started attempt with no result is owed no
//! observation, a spooled attempt is owed exactly one, and an already-committed
//! attempt is owed exactly one more, never two. The second point's case reads
//! its interrupted attempt past the command horizon and requires collector
//! interruption rather than a timeout or a missing attempt. The property test
//! walks randomized sequences of injections and restarts and holds that no
//! attempt ever carries more than one observation. The unit test confines the
//! injection hooks to the one harness module and off the shipping command
//! surface.

use std::process::Command;

use proptest::prelude::*;
use test_support::StateDir;

fn aub() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aub"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParsedCounts {
    starts: u64,
    results: u64,
    observations: u64,
    pending: u64,
}

fn parse_counts(report: &str) -> ParsedCounts {
    let mut starts = 0;
    let mut results = 0;
    let mut observations = 0;
    let mut pending = 0;
    for part in report.split_whitespace() {
        if let Some(val) = part.strip_prefix("starts=") {
            starts = val.parse().unwrap();
        } else if let Some(val) = part.strip_prefix("results=") {
            results = val.parse().unwrap();
        } else if let Some(val) = part.strip_prefix("observations=") {
            observations = val.parse().unwrap();
        } else if let Some(val) = part.strip_prefix("pending=") {
            pending = val.parse().unwrap();
        }
    }
    ParsedCounts {
        starts,
        results,
        observations,
        pending,
    }
}

fn read_back(state: &StateDir) -> ParsedCounts {
    let out = aub()
        .args(["__attempt-crash-hook", "read-back"])
        .env("AUB_STATE_DIR", state.path())
        .output()
        .expect("the aub binary must run");
    assert!(
        out.status.success(),
        "read-back must succeed: {out:?}; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    parse_counts(&String::from_utf8_lossy(&out.stdout))
}

fn drain(state: &StateDir) -> String {
    let out = aub()
        .args(["__attempt-crash-hook", "drain"])
        .env("AUB_STATE_DIR", state.path())
        .output()
        .expect("the aub binary must run");
    assert!(
        out.status.success(),
        "drain must succeed: {out:?}; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn freshness(state: &StateDir) -> String {
    let out = aub()
        .args(["__attempt-crash-hook", "freshness"])
        .env("AUB_STATE_DIR", state.path())
        .output()
        .expect("the aub binary must run");
    assert!(
        out.status.success(),
        "freshness must succeed: {out:?}; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Runs one crash-hook stage and asserts it died by signal: `None` from
/// `ExitStatus::code()` means the process was killed rather than exited, which
/// is the crash-semantics property every injection must carry. An ordinary
/// exit here would mean the hook shut down cleanly instead of crashing.
fn crash(stage: &str, state: &StateDir) {
    let crashed = aub()
        .args(["__attempt-crash-hook", stage])
        .env("AUB_STATE_DIR", state.path())
        .status()
        .expect("the aub binary must run");
    assert_eq!(
        crashed.code(),
        None,
        "injection at {stage} must terminate by abort signal, got {crashed:?}"
    );
}

fn complete(state: &StateDir) {
    let done = aub()
        .args(["__attempt-crash-hook", "complete"])
        .env("AUB_STATE_DIR", state.path())
        .status()
        .expect("the aub binary must run");
    assert!(
        done.success(),
        "the complete stage must exit cleanly: {done:?}"
    );
}

/// Point 1: killed before the attempt-start commit.
///
/// Leaves no attempt, no result, no observation, no pending file: the write
/// path has not begun. Drain finds nothing to recover, and a subsequent
/// complete run produces exactly one observation.
#[test]
fn matrix_point_1_killed_before_start_commit_leaves_nothing_and_recovers_cleanly() {
    let state = StateDir::new();

    crash("before-start-commit", &state);

    let pre_counts = read_back(&state);
    assert_eq!(
        pre_counts,
        ParsedCounts {
            starts: 0,
            results: 0,
            observations: 0,
            pending: 0,
        },
        "crashed before start commit leaves nothing on disk"
    );

    let drain_report = drain(&state);
    assert!(
        drain_report.contains("applied=0 already_applied=0 quarantined=0"),
        "drain has nothing to recover: {drain_report}"
    );

    complete(&state);

    let post_counts = read_back(&state);
    assert_eq!(
        post_counts,
        ParsedCounts {
            starts: 1,
            results: 1,
            observations: 1,
            pending: 0,
        },
        "exact observation count after the next complete run must be 1"
    );
}

/// Point 2: killed after the attempt-start commit and before the request returns.
///
/// Leaves exactly one started attempt with no terminal result and no spool
/// file. Reading it past the command horizon yields collector interruption,
/// never an endpoint timeout and never a missing attempt. The interrupted
/// attempt is owed no observation; the next complete run adds the only one.
#[test]
fn matrix_point_2_killed_after_start_commit_before_request_leaves_interrupted_attempt() {
    let state = StateDir::new();

    crash("after-start-commit-before-request", &state);

    let pre_counts = read_back(&state);
    assert_eq!(
        pre_counts,
        ParsedCounts {
            starts: 1,
            results: 0,
            observations: 0,
            pending: 0,
        },
        "started attempt survives with no terminal result and no pending spool"
    );

    let fresh = freshness(&state);
    assert!(
        fresh.contains("freshness: stale reason=collector_interrupted"),
        "the incomplete attempt must read as collector interruption: {fresh}"
    );

    complete(&state);

    let post_counts = read_back(&state);
    assert_eq!(
        post_counts,
        ParsedCounts {
            starts: 2,
            results: 1,
            observations: 1,
            pending: 0,
        },
        "total observation count after recovery must be exactly 1"
    );
}

/// Point 3: killed after the network parse and before the spool write.
///
/// The parsed result existed only in memory: nothing durable carries it, so
/// the state equals point 2's. The attempt reads as collector interruption,
/// and the next complete run produces the only observation.
#[test]
fn matrix_point_3_killed_after_parse_before_spool_write_leaves_interrupted_attempt() {
    let state = StateDir::new();

    crash("after-parse-before-spool-write", &state);

    let pre_counts = read_back(&state);
    assert_eq!(
        pre_counts,
        ParsedCounts {
            starts: 1,
            results: 0,
            observations: 0,
            pending: 0,
        },
        "killed before the spool write leaves no pending spool and no result"
    );

    let fresh = freshness(&state);
    assert!(
        fresh.contains("freshness: stale reason=collector_interrupted"),
        "the unspooled attempt must read as collector interruption: {fresh}"
    );

    complete(&state);

    let post_counts = read_back(&state);
    assert_eq!(
        post_counts,
        ParsedCounts {
            starts: 2,
            results: 1,
            observations: 1,
            pending: 0,
        },
        "total observation count after recovery must be exactly 1"
    );
}

/// Point 4: killed after the spool write and before the SQLite commit.
///
/// The observation is owed exactly once and is durably spooled: recovery is
/// the drain pass committing the spooled bundle into SQLite and deleting the
/// file. After it, exactly one observation for this attempt and an empty
/// pending directory.
#[test]
fn matrix_point_4_killed_after_spool_write_before_sqlite_commit_recovers_observation_from_spool() {
    let state = StateDir::new();

    crash("after-spool-write-before-sqlite-commit", &state);

    let pre_counts = read_back(&state);
    assert_eq!(
        pre_counts,
        ParsedCounts {
            starts: 1,
            results: 0,
            observations: 0,
            pending: 1,
        },
        "the pending spool file exists while the SQLite commit has not run"
    );

    let drain_report = drain(&state);
    assert!(
        drain_report.contains("applied=1 already_applied=0 quarantined=0"),
        "drain must apply the spooled bundle: {drain_report}"
    );

    let post_counts = read_back(&state);
    assert_eq!(
        post_counts,
        ParsedCounts {
            starts: 1,
            results: 1,
            observations: 1,
            pending: 0,
        },
        "exact observation count after spool recovery must be 1"
    );
}

/// Point 5: killed after the SQLite commit and before the pending deletion.
///
/// The observation is already durable and the spool file is a leftover: drain
/// must recognize the already-applied evidence, delete the file and write
/// nothing. The idempotent replay is keyed on the attempt id, so the count
/// stays at exactly one.
#[test]
fn matrix_point_5_killed_after_sqlite_commit_before_pending_deletion_replays_idempotently() {
    let state = StateDir::new();

    crash("after-sqlite-commit-before-pending-deletion", &state);

    let pre_counts = read_back(&state);
    assert_eq!(
        pre_counts,
        ParsedCounts {
            starts: 1,
            results: 1,
            observations: 1,
            pending: 1,
        },
        "SQLite holds the committed observation while the pending spool file remains"
    );

    let drain_report = drain(&state);
    assert!(
        drain_report.contains("applied=0 already_applied=1 quarantined=0"),
        "drain must recognize the duplicate and delete the spool without inserting: {drain_report}"
    );

    let post_counts = read_back(&state);
    assert_eq!(
        post_counts,
        ParsedCounts {
            starts: 1,
            results: 1,
            observations: 1,
            pending: 0,
        },
        "exact observation count after the idempotent replay must remain 1"
    );
}

/// The permitted positive control: every write-path stage runs and exits
/// cleanly. Near-identical to every crash case above and differing only in the
/// missing injection, which is what makes those cases about the injection.
#[test]
fn matrix_positive_control_complete_writes_all_facts_and_exits_cleanly() {
    let state = StateDir::new();

    complete(&state);

    let report = read_back(&state);
    assert_eq!(
        report,
        ParsedCounts {
            starts: 1,
            results: 1,
            observations: 1,
            pending: 0,
        },
        "one attempt, one terminal result, one observation, zero pending files"
    );
}

/// Preserves the invariant 23 proof from aub-sth.6 under its recorded name:
/// killed between the attempt-start commit and the result write leaves exactly
/// one start with no result, and the next complete run is the second start
/// with the only result. Nothing tidied the interrupted attempt away.
#[test]
fn killed_between_start_and_result_leaves_exactly_one_start_with_no_result() {
    let state = StateDir::new();

    crash("start", &state);

    let report = read_back(&state);
    assert_eq!(report.starts, 1, "exactly one start survives");
    assert_eq!(
        report.results, 0,
        "no terminal result for the killed attempt"
    );
    assert_eq!(
        report.observations, 0,
        "no observation for the killed attempt"
    );

    complete(&state);

    let post = read_back(&state);
    assert_eq!(post.starts, 2, "the completed attempt is a new start");
    assert_eq!(
        post.results, 1,
        "the completed attempt holds the only result"
    );
    assert_eq!(post.observations, 1, "exactly one observation");
    assert_eq!(post.pending, 0, "no pending file was left behind");
}

// Property: over a randomized sequence of injections and restarts, the
// observation count for any one attempt never exceeds one, and the number of
// observations equals the number of attempts that reached a terminal result.
//
// A total-count assertion alone cannot catch the interesting violation: two
// observations on one attempt and none on another would still sum correctly.
// The per-attempt check below is what carries the property, and it is
// load-bearing rather than redundant because `meter_observation` carries no
// uniqueness constraint on `attempt_id` (migration 0013): the idempotent
// replay is enforced by drain's already-applied detection, and this is the
// test that notices if that detection stops working. The test speaks SQL
// directly because the store exposes no all-attempts reader and the check
// must see every attempt, not only the open ones.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]
    #[test]
    fn property_observation_count_for_one_attempt_is_never_greater_than_one(
        stages in proptest::collection::vec(
            proptest::sample::select(vec![
                "before-start-commit",
                "after-start-commit-before-request",
                "after-parse-before-spool-write",
                "after-spool-write-before-sqlite-commit",
                "after-sqlite-commit-before-pending-deletion",
                "complete",
            ]),
            1..12,
        ),
    ) {
        let state = StateDir::new();
        let mut expected_completed = 0u64;

        for stage in stages {
            let status = aub()
                .args(["__attempt-crash-hook", stage])
                .env("AUB_STATE_DIR", state.path())
                .status()
                .expect("aub must run");

            if stage == "complete" {
                prop_assert!(status.success(), "the complete stage must exit cleanly");
                expected_completed += 1;
            } else {
                prop_assert_eq!(status.code(), None, "an injection must die by signal");
                // Points 4 and 5 leave recoverable state; the recovery step
                // (the same drain the startup path runs) belongs to the
                // sequence, and each drain sees at most one pending record
                // because every recoverable injection drains before the next
                // stage runs.
                let is_spool_recoverable = stage == "after-spool-write-before-sqlite-commit"
                    || stage == "after-sqlite-commit-before-pending-deletion";
                if is_spool_recoverable {
                    let drain_out = drain(&state);
                    if stage == "after-spool-write-before-sqlite-commit" {
                        prop_assert!(
                            drain_out.contains("applied=1 already_applied=0 quarantined=0"),
                            "drain must apply the spooled bundle: {drain_out}"
                        );
                    } else {
                        prop_assert!(
                            drain_out.contains("applied=0 already_applied=1 quarantined=0"),
                            "drain must replay idempotently: {drain_out}"
                        );
                    }
                    expected_completed += 1;
                }
            }
        }

        let counts = read_back(&state);
        prop_assert_eq!(counts.observations, expected_completed);
        prop_assert_eq!(counts.results, counts.observations);
        prop_assert_eq!(counts.pending, 0);

        let db_path = state.path().join("attempt-crash-hook.db");
        if db_path.exists() {
            let conn = rusqlite::Connection::open(&db_path).expect("database must open");
            let mut stmt = conn
                .prepare(
                    "SELECT attempt_id, count(*) FROM meter_observation GROUP BY attempt_id",
                )
                .expect("the observation query must prepare");
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                })
                .expect("the observation query must run");
            for row in rows {
                let (attempt_id, observation_count) = row.expect("rows must read");
                prop_assert!(
                    observation_count == 1,
                    "attempt {attempt_id} carries {observation_count} observations; \
                     the recovery steps must land exactly one per completed attempt"
                );
            }
        }
    }
}

/// Unit: the injection hooks exist in exactly one place, off the shipping surface.
///
/// The bead's criterion asks for injection hooks absent from a release binary,
/// asserted by a symbol or feature check. The harness command ships in the
/// binary by the architecture aub-sth.6 closed: the end-to-end case
/// (`tests/e2e/cases/009-attempt-crash.sh`) drives the release binary through
/// it and asserts the abort by signal, so a binary-contents absence check is
/// impossible to satisfy, let alone to prove red. The property that does hold,
/// asserted here at the source layer that can actually fail, is two-fold:
/// every `process::abort` call site in src/ lives in the one test-surface
/// harness module (no production write-path module carries an injection
/// point), and no command on the shipping command surface reaches one. This
/// is the same reasoning `bin/checks/65-synthetic-adapter-absent-from-release`
/// documents: `[profile.release] strip = true` and dead-code elimination make
/// a binary-strings check unprovable, so the structural check is the layer
/// that can go red.
#[test]
fn injection_hooks_are_confined_to_the_harness_module_and_off_the_shipping_surface() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let harness_module = manifest_dir.join("src/store/attempt_crash_hook.rs");

    let mut files_with_abort = Vec::new();
    let mut stack = vec![manifest_dir.join("src")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("src/ must be readable") {
            let entry = entry.expect("src/ entries must be readable");
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let text = std::fs::read_to_string(&path).expect("source must be readable");
                if text.contains("process::abort") {
                    files_with_abort.push(path);
                }
            }
        }
    }

    let mut sorted = files_with_abort.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        vec![harness_module],
        "every process::abort call site must live in the crash-harness module; \
         the write path itself carries no injection point"
    );
    assert_eq!(
        files_with_abort.len(),
        1,
        "the harness module is the one injection surface"
    );

    let surface = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/e2e/command-surface.txt"
    ))
    .expect("command-surface.txt must be readable");
    assert!(
        !surface.contains("__attempt-crash-hook"),
        "__attempt-crash-hook must not appear in the shipping command surface"
    );
}
