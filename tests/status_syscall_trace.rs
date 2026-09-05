//! Runtime proof that `aub status` opens no network socket and writes nothing to the
//! state directory (aub-me5.7, PLAN.md sections 16.2 and 34.31).
//!
//! The module-graph checks (boundary rules 06, 15 and 19, plus the status-contract
//! unit test in src/cli.rs) prove the status path's *source* never names the transport
//! or the store. This file is the belt to that braces: it runs the compiled binary
//! under `strace` and inspects the syscalls it actually made, which catches a
//! dependency that reaches the network or the disk through something that did not
//! look like transport or store from the source (PLAN.md section 34.31, "Where
//! practical, CI additionally checks process syscalls").
//!
//! The tracer is Linux-and-strace-only. Elsewhere, the one integration test that needs
//! it records why it is skipped instead of passing silently; `tracer_skip_reason` is
//! the pure decision behind that record, and it is exercised deterministically by the
//! unit tests below regardless of what this machine happens to have installed.

use std::path::{Path, PathBuf};
use std::process::Command;

// --- the syscall-tracer job's own decision logic (unit-testable without strace) ----

/// `None` when the tracer can run here; `Some(reason)` otherwise. Takes its inputs as
/// plain values (never probes the environment itself) so both branches are
/// deterministically testable, independent of what this machine has installed.
fn tracer_skip_reason(os: &str, strace_present: bool) -> Option<String> {
    if os != "linux" {
        return Some(format!(
            "platform is not linux (it is {os}); the syscall tracer job supports linux+strace only"
        ));
    }
    if !strace_present {
        return Some(
            "strace is not installed on this linux host; the syscall tracer job supports linux+strace only"
                .to_string(),
        );
    }
    None
}

fn strace_present() -> bool {
    Command::new("strace")
        .arg("-V")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[test]
fn tracer_skip_reason_names_an_unsupported_platform() {
    let reason = tracer_skip_reason("macos", false).expect("macos must be unsupported");
    assert!(reason.contains("macos"), "{reason}");
}

#[test]
fn tracer_skip_reason_names_a_missing_strace_on_linux() {
    let reason =
        tracer_skip_reason("linux", false).expect("linux without strace must be unsupported");
    assert!(reason.contains("strace is not installed"), "{reason}");
}

#[test]
fn tracer_skip_reason_is_none_when_the_tracer_can_run() {
    assert_eq!(tracer_skip_reason("linux", true), None);
}

// --- the syscall-trace parser (unit-testable against synthetic strace output) ------

/// Parses `strace -f -e trace=network,openat` output and returns one description per
/// violation of the status contract: a `socket()` call of any family (no network
/// access is permitted at all, PLAN.md section 16.2's "must not perform: HTTP"), and
/// an `openat()` naming a path under `state_dir` opened with a write-capable flag (no
/// database writes, same section).
///
/// A read-only `openat()` under `state_dir` is the one bounded file read the status
/// contract explicitly allows (the projection) and is never a violation.
fn trace_violations(trace: &str, state_dir: &Path) -> Vec<String> {
    let state_dir_marker = state_dir.to_string_lossy().into_owned();
    let write_flags = ["O_WRONLY", "O_RDWR", "O_CREAT", "O_APPEND", "O_TRUNC"];

    trace
        .lines()
        .filter_map(|line| {
            let call = strip_pid_prefix(line);
            if call.starts_with("socket(") {
                return Some(format!("socket opened: {line}"));
            }
            let args = call.strip_prefix("openat(")?;
            if !args.contains(&state_dir_marker) {
                return None;
            }
            if write_flags.iter().any(|flag| args.contains(flag)) {
                return Some(format!("state directory opened for writing: {line}"));
            }
            None
        })
        .collect()
}

/// `strace -f` prefixes every line with the PID that made the call, e.g.
/// `12345 openat(...)`. Strips that prefix so the call itself starts at column 0.
fn strip_pid_prefix(line: &str) -> &str {
    let trimmed = line.trim_start();
    match trimmed.split_once(char::is_whitespace) {
        Some((pid, rest)) if !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit()) => {
            rest.trim_start()
        }
        _ => trimmed,
    }
}

#[test]
fn trace_violations_catches_a_socket_call() {
    let trace = "12345 socket(AF_INET, SOCK_STREAM, IPPROTO_TCP) = 3\n";
    let violations = trace_violations(trace, Path::new("/tmp/aub-test/state"));
    assert!(
        violations.iter().any(|v| v.contains("socket opened")),
        "{violations:?}"
    );
}

#[test]
fn trace_violations_catches_a_unix_socket_call_too() {
    // The status contract forbids network access entirely, not only AF_INET: any
    // socket() call is a violation regardless of address family.
    let trace = "12345 socket(AF_UNIX, SOCK_STREAM, 0) = 3\n";
    let violations = trace_violations(trace, Path::new("/tmp/aub-test/state"));
    assert!(
        violations.iter().any(|v| v.contains("socket opened")),
        "{violations:?}"
    );
}

#[test]
fn trace_violations_catches_a_write_open_under_the_state_directory() {
    let trace = "12345 openat(AT_FDCWD, \"/tmp/aub-test/state/projection\", O_WRONLY|O_CREAT|O_TRUNC, 0644) = 4\n";
    let violations = trace_violations(trace, Path::new("/tmp/aub-test/state"));
    assert!(
        violations.iter().any(|v| v.contains("opened for writing")),
        "{violations:?}"
    );
}

#[test]
fn trace_violations_permits_a_read_only_open_of_the_projection() {
    let trace = "12345 openat(AT_FDCWD, \"/tmp/aub-test/state/projection\", O_RDONLY) = 4\n";
    let violations = trace_violations(trace, Path::new("/tmp/aub-test/state"));
    assert!(violations.is_empty(), "{violations:?}");
}

#[test]
fn trace_violations_permits_a_write_open_outside_the_state_directory() {
    // The config file lives beside, not under, the state directory: a write open
    // there is not a violation of *this* contract, only of ones this file does not
    // assert (status never writes it either, but that is out of scope here).
    let trace = "12345 openat(AT_FDCWD, \"/tmp/aub-test/aub.toml\", O_WRONLY|O_CREAT, 0644) = 4\n";
    let violations = trace_violations(trace, Path::new("/tmp/aub-test/state"));
    assert!(violations.is_empty(), "{violations:?}");
}

// --- an isolated environment to run the real binary in ------------------------------

struct Environment {
    root: PathBuf,
}

impl Environment {
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("aub-status-syscall-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::write(
            root.join("aub.toml"),
            format!(
                "state.dir = \"{}\"\n\n[[accounts]]\nname = \"work-primary\"\nprovider = \"anthropic\"\n",
                root.join("state").display()
            ),
        )
        .unwrap();
        std::fs::write(
            root.join("state").join("projection"),
            r#"{"schema_version":2,"ledger_generation":1,"accounts":[{"account_id":1,"logical_name":"work-primary","provider":"anthropic","last_successful_observation":null,"latest_attempt":null}]}"#,
        )
        .unwrap();
        Self { root }
    }

    fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn status_opens_no_socket_and_writes_nothing_to_the_state_directory() {
    if let Some(reason) = tracer_skip_reason(std::env::consts::OS, strace_present()) {
        eprintln!("[STATUS_SYSCALL_TRACE_SKIPPED] {reason}");
        return;
    }

    let env = Environment::new("live");
    let trace_path = env.root.join("trace.log");

    let strace_status = Command::new("strace")
        .args(["-f", "-e", "trace=network,openat", "-o"])
        .arg(&trace_path)
        .arg(env!("CARGO_BIN_EXE_aub"))
        .env("HOME", env.root.join("home"))
        .env("AUB_CONFIG_FILE", env.root.join("aub.toml"))
        .arg("status")
        .status()
        .expect("strace must be runnable: tracer_skip_reason already confirmed it is installed");
    assert!(
        strace_status.success(),
        "strace exited non-zero running aub status"
    );

    let trace = std::fs::read_to_string(&trace_path).expect("strace must write its trace file");
    let violations = trace_violations(&trace, &env.state_dir());
    assert!(
        violations.is_empty(),
        "aub status violated the no-network/no-write contract:\n{}",
        violations.join("\n")
    );
}
