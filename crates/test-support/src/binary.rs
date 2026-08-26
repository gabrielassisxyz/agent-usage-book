//! Locates the `aub` binary from the test run's target directory, never from `PATH`.

use std::path::{Path, PathBuf};

/// Resolves the `aub` binary under `target_dir`, failing loudly naming the expected
/// path when it is absent. It never consults `PATH`: a binary found on `PATH` may be
/// another pane's build, and a test that shells out to the wrong one fails or passes
/// for reasons that have nothing to do with the code under test.
pub fn aub_binary_in(target_dir: &Path) -> PathBuf {
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let path = target_dir.join(profile).join("aub");
    if path.is_file() {
        path
    } else {
        panic!(
            "aub binary not found at {}; build it first (cargo build)",
            path.display()
        );
    }
}
