//! Verifies the shipped scheduler and hook examples (aub-eun.9):
//!
//! - every example spells the `aub` binary as an absolute path, never a bare
//!   command name, because none of a systemd unit, a cron entry or a
//!   compositor keybinding reads an interactive shell's `PATH`;
//! - the systemd unit and timer are syntactically valid, checked with
//!   `systemd-analyze verify` where that tool exists on this platform, and
//!   skipped with a recorded reason otherwise.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The placeholder install path every example ships with. One definition, so
/// changing it means changing the examples and this test in the same place.
const AUB_ABSOLUTE_PATH: &str = "/usr/local/bin/aub";

// The timer is deliberately absent here: a systemd `.timer` unit schedules
// its paired `.service` and never invokes a binary itself (that is the
// `.service` file's job, checked below), so it has no absolute path to
// carry.
const EXAMPLES_INVOKING_THE_BINARY: &[&str] = &[
    "examples/scheduler/systemd/aub-sample.service",
    "examples/scheduler/cron/aub-sample.cron",
    "examples/hooks/aub-session-start.sh",
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// **Unit test**: every shipped example invokes the binary through an
/// absolute path. A naive edit that shortens `/usr/local/bin/aub` back down
/// to the bare command name `aub` still reads fine to a human and still
/// fails silently on a fresh machine, since neither systemd, cron nor a
/// compositor keybinding inherits an interactive shell's `PATH`.
///
/// Comment lines are excluded before the check: every example also explains
/// the absolute path in prose above the line that uses it, and that prose
/// keeps saying `/usr/local/bin/aub` regardless of what the invocation line
/// itself was edited down to, which would otherwise let a mutated
/// `ExecStart=aub sample --due` pass this test on the strength of the
/// comment two lines above it.
#[test]
fn every_example_uses_an_absolute_path_to_the_binary() {
    let root = repo_root();
    for relative in EXAMPLES_INVOKING_THE_BINARY {
        let path = root.join(relative);
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let code_contains_it = content
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .any(|line| line.contains(AUB_ABSOLUTE_PATH));
        assert!(
            code_contains_it,
            "{relative}: expected a non-comment line invoking the binary through the absolute \
             path {AUB_ABSOLUTE_PATH}, found none"
        );
    }
}

/// Whether `systemd-analyze` exists on this platform. Checked once per test
/// rather than cached, since the check itself is what runs the process the
/// caller wants to run next either way.
fn systemd_analyze_available() -> bool {
    Command::new("systemd-analyze")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// **Integration test**: `systemd-analyze verify` parses the shipped unit.
///
/// `systemd-analyze verify` refuses to load a unit whose `ExecStart` binary
/// is not present and executable on disk; that is a hard load error, not a
/// lint warning, and `--recursive-errors` does not change it. The shipped
/// example necessarily names a placeholder install path that is legitimately
/// absent on the machine running this test, so verification substitutes that
/// placeholder for a real scratch executable in a temporary copy before
/// handing the file to `systemd-analyze`. That proves the unit's own section
/// structure and key syntax, which is the property this test owns, without
/// asserting anything about a path that is different on every machine that
/// installs `aub`.
fn verify_systemd_unit(relative: &str) {
    if !systemd_analyze_available() {
        eprintln!(
            "SKIP {relative}: systemd-analyze not found on this platform, cannot verify unit syntax"
        );
        return;
    }

    let root = repo_root();
    let source_path = root.join(relative);
    let content = std::fs::read_to_string(&source_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", source_path.display()));

    let file_name = Path::new(relative)
        .file_name()
        .expect("example path has a file name");
    let scratch_dir = std::env::temp_dir().join(format!(
        "aub-unit-verify-{}-{}",
        std::process::id(),
        file_name.to_string_lossy()
    ));
    std::fs::create_dir_all(&scratch_dir).expect("create scratch dir");

    // Only a unit that itself names ExecStart needs the substitution: a
    // `.timer` schedules its paired service and carries no binary path of
    // its own, so it verifies against its checked-in content directly.
    let scratch_content = if content.contains(AUB_ABSOLUTE_PATH) {
        let stub_bin = scratch_dir.join("aub-stub");
        std::fs::write(&stub_bin, "#!/bin/sh\nexit 0\n").expect("write stub binary");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub_bin, std::fs::Permissions::from_mode(0o755))
                .expect("make stub binary executable");
        }
        content.replace(AUB_ABSOLUTE_PATH, stub_bin.to_str().unwrap())
    } else {
        content
    };
    let scratch_unit = scratch_dir.join(file_name);
    std::fs::write(&scratch_unit, scratch_content).expect("write scratch unit");

    let output = Command::new("systemd-analyze")
        .arg("verify")
        .arg(&scratch_unit)
        .output()
        .expect("run systemd-analyze verify");

    let _ = std::fs::remove_dir_all(&scratch_dir);

    assert!(
        output.status.success(),
        "{relative} failed systemd-analyze verify: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn systemd_service_example_verifies() {
    verify_systemd_unit("examples/scheduler/systemd/aub-sample.service");
}

#[test]
fn systemd_timer_example_verifies() {
    verify_systemd_unit("examples/scheduler/systemd/aub-sample.timer");
}
