//! The kill-between-stages proof for the two-stage meter attempt lifecycle
//! (`aub-sth.6`, PLAN.md invariants 23 and 24, section 34.7): a process that
//! dies between the attempt-start commit and the result write leaves exactly
//! one start with no result. The property is about a process, not a function,
//! so this test drives the real binary through its documented crash-injection
//! hook (`__attempt-crash-hook`, the same surface the e2e case drives).

use std::path::PathBuf;
use std::process::Command;

/// One fresh isolated state directory per test, removed on drop.
struct StateDir(std::path::PathBuf);

impl StateDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("aub-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("state dir must be creatable");
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for StateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn aub() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aub"))
}

fn read_back(state: &StateDir) -> String {
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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The bead's own minimum proof of its claim: the process is killed at the
/// documented injection point, after the attempt-start commit and before any
/// result, and the database holds exactly one started attempt with no terminal
/// result. No fabricated timeout, no missing attempt, no second start.
#[test]
fn killed_between_start_and_result_leaves_exactly_one_start_with_no_result() {
    let state = StateDir::new("meter-attempt-kill");

    let crashed = aub()
        .args(["__attempt-crash-hook", "start"])
        .env("AUB_STATE_DIR", state.path())
        .status()
        .expect("the aub binary must run");

    // The injection point aborts the process: a crash by signal, not a
    // classified exit. An ordinary exit here would mean the hook did not
    // inject the crash at all.
    assert_eq!(
        crashed.code(),
        None,
        "the crash stage must end by signal, got {crashed:?}"
    );

    let report = read_back(&state);
    assert!(
        report.contains("starts=1"),
        "exactly one started attempt must survive the kill, got: {report}"
    );
    assert!(
        report.contains("results=0"),
        "no terminal result must exist for the killed attempt, got: {report}"
    );

    // The control on the same database: the next real attempt runs both stages
    // and completes, without the interrupted one having been tidied away.
    let done = aub()
        .args(["__attempt-crash-hook", "complete"])
        .env("AUB_STATE_DIR", state.path())
        .status()
        .expect("the aub binary must run");
    assert!(
        done.success(),
        "the complete stage must exit cleanly after the killed start, got {done:?}"
    );
    let report = read_back(&state);
    assert!(
        report.contains("starts=2 results=1"),
        "the completed attempt must be the second start with the only result: {report}"
    );
}

/// The permitted adjacent positive: both stages run and the database holds one
/// start and one result. Near-identical to the kill case and differing only in
/// the crash injection.
#[test]
fn the_complete_stage_writes_both_facts_and_exits_cleanly() {
    let state = StateDir::new("meter-attempt-complete");

    let done = aub()
        .args(["__attempt-crash-hook", "complete"])
        .env("AUB_STATE_DIR", state.path())
        .status()
        .expect("the aub binary must run");
    assert!(
        done.success(),
        "the complete stage must exit cleanly, got {done:?}"
    );

    let report = read_back(&state);
    assert!(
        report.contains("starts=1 results=1"),
        "one attempt, one terminal result: {report}"
    );
}
