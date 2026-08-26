//! Loads test fixtures relative to the crate root, not the process working directory.

use std::path::{Path, PathBuf};

/// Resolves a fixture path relative to `crate_root`, so the result is independent of
/// the working directory the test was invoked from.
pub fn fixture_path(crate_root: &Path, relative: &str) -> PathBuf {
    crate_root.join(relative)
}

/// Reads a fixture relative to `crate_root`, failing with the resolved path when the
/// file is unreadable.
pub fn load_fixture(crate_root: &Path, relative: &str) -> String {
    let path = fixture_path(crate_root, relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("fixture {} unreadable: {e}", path.display()))
}
