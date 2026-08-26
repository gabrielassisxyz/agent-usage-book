//! Records the provenance a calibration result or schema migration persists: the git
//! revision and the toolchain version that actually produced this binary. A fit that
//! cannot say which source revision or which compiler built its fitter has a hole in its
//! reproducibility claim.
//!
//! No `rerun-if-changed` is emitted, so cargo reruns this script on every build. The
//! alternative, watching `.git/HEAD` and the refs directory, misses a checkout whose
//! branch ref moved without `.git/HEAD` itself changing, and a stale revision baked into a
//! binary is exactly the failure this exists to prevent.

use std::process::Command;

fn main() {
    let revision = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=AUB_GIT_REVISION={revision}");

    let toolchain = toolchain_version();
    println!("cargo:rustc-env=AUB_TOOLCHAIN_VERSION={toolchain}");
}

/// The toolchain version the binary was actually built with: the version `rustc --version`
/// reports, cross-checked against the channel pinned in `rust-toolchain.toml`. The two
/// agree only while rustup honours the pin, and they diverge exactly when provenance
/// matters, so a disagreement fails the build rather than recording the pin and looking
/// correct.
fn toolchain_version() -> String {
    let pinned = channel_from_toolchain_file()
        .unwrap_or_else(|| panic!("rust-toolchain.toml does not declare a [toolchain] channel"));
    let reported = rustc_reported_version();
    if pinned != reported {
        panic!(
            "toolchain mismatch: rustc reports {reported} but rust-toolchain.toml pins {pinned}"
        );
    }
    reported
}

/// Runs `rustc --version` and returns the version token from its output, e.g. `1.97.1`
/// out of `rustc 1.97.1 (8bab26f4f 2026-08-14)`.
fn rustc_reported_version() -> String {
    let output = Command::new("rustc")
        .args(["--version"])
        .output()
        .expect("rustc --version must run");
    assert!(
        output.status.success(),
        "rustc --version failed with status {}",
        output.status
    );
    let stdout = String::from_utf8(output.stdout).expect("rustc --version output must be UTF-8");
    stdout
        .split_whitespace()
        .nth(1)
        .expect("rustc --version output must contain a version token")
        .to_string()
}

/// The toolchain file for the package whose manifest lives in `manifest_dir`.
///
/// The directory is a parameter so the caller decides where it comes from: the build
/// script reads it from the `CARGO_MANIFEST_DIR` environment variable at run time,
/// never from `env!("CARGO_MANIFEST_DIR")`. `env!` bakes the directory that *compiled*
/// this script into the binary, and the shared `CARGO_TARGET_DIR` hands one worktree's
/// compiled build script to another: the baked path then points at a worktree that may
/// be gone, and the build fails blaming a `rust-toolchain.toml` that is correct.
pub fn toolchain_file_path(manifest_dir: &std::path::Path) -> std::path::PathBuf {
    manifest_dir.join("rust-toolchain.toml")
}

/// Reads `rust-toolchain.toml` and returns the `channel` value from its `[toolchain]`
/// table, or `None` when the file is readable but the key is absent. A file that cannot
/// be read is a different failure and panics naming the path that was tried, so the
/// message cannot send the next reader to a file that is correct.
fn channel_from_toolchain_file() -> Option<String> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .expect("cargo must set CARGO_MANIFEST_DIR when running a build script");
    let path = toolchain_file_path(std::path::Path::new(&manifest_dir));
    let contents = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()));
    channel_from_toolchain_toml(&contents)
}

/// Parses the `channel` value from the `[toolchain]` table of a `rust-toolchain.toml`
/// document. Returns `None` when the table or the key is absent or the value is empty.
fn channel_from_toolchain_toml(contents: &str) -> Option<String> {
    let mut in_toolchain = false;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed == "[toolchain]" {
            in_toolchain = true;
            continue;
        }
        if in_toolchain && trimmed.starts_with('[') {
            break;
        }
        if !in_toolchain {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        if key.trim() == "channel" {
            let value = value.trim().trim_matches('"');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}
