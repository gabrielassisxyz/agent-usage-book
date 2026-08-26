//! Records the provenance a calibration result or schema migration persists: the git
//! revision and the pinned toolchain version that produced this binary. A fit that cannot
//! say which source revision or which compiler built its fitter has a hole in its
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

    let toolchain = toolchain_channel();
    println!("cargo:rustc-env=AUB_TOOLCHAIN_VERSION={toolchain}");
}

/// Reads the `channel` value out of `rust-toolchain.toml`'s `[toolchain]` table, so the
/// build metadata records the pin rather than whatever compiler happened to be on the
/// host. Falls back to the sentinel `"unknown"` when the file cannot be read or parsed;
/// the test in `src/build_info.rs` rejects that sentinel, so a build whose toolchain
/// version cannot be determined fails `cargo test` instead of shipping a provenance
/// field nothing can verify.
fn toolchain_channel() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("rust-toolchain.toml");
    std::fs::read_to_string(path)
        .ok()
        .as_deref()
        .and_then(channel_from_toolchain_toml)
        .unwrap_or_else(|| "unknown".to_string())
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
