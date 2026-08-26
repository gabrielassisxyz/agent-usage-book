//! Records the git revision that produced this binary, since calibration results and
//! schema migrations persist it and a fit that cannot say which source revision built its
//! fitter has a hole in its reproducibility claim.
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
}
