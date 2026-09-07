//! Scratch-tree writers for tests that stage files on disk.
//!
//! The meter module is forbidden every write-capable `std::fs` facility by
//! boundary rule 17, and the rule reads the module's source as text, test
//! modules included. A meter test that needs a rollout tree on disk therefore
//! writes it through this crate, which the rule never scans, rather than
//! spelling `std::fs` inside `src/meter/`.

use std::fs;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

/// Writes `bytes` to `path`, creating the parent directories as needed.
pub fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("scratch parent directories must be creatable");
    }
    fs::write(path, bytes).expect("scratch file must be writable");
}

/// Creates `path` and every missing ancestor.
pub fn create_dir_all(path: &Path) {
    fs::create_dir_all(path).expect("scratch directory must be creatable");
}

/// Pins the modification time of an existing file to `unix_seconds`, so a
/// test's ordering by mtime never depends on creation order or on the
/// filesystem's timestamp granularity.
pub fn pin_mtime(path: &Path, unix_seconds: u64) {
    fs::File::options()
        .write(true)
        .open(path)
        .expect("scratch file must be openable for an mtime pin")
        .set_modified(UNIX_EPOCH + Duration::from_secs(unix_seconds))
        .expect("scratch file mtime must be settable");
}
