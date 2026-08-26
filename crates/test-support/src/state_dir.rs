//! An isolated state directory per test, with the production permission mode.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh, private state directory that removes itself on drop, including after a
/// panic. Created with the production permission mode (0700) so a test that passes
/// here cannot pass because the directory was more permissive than production.
pub struct StateDir {
    path: PathBuf,
}

impl StateDir {
    /// Creates a fresh directory under the system temp dir with mode 0700.
    pub fn new() -> Self {
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("aub-test-{}-{suffix}", std::process::id()));
        fs::create_dir(&path).expect("state dir must be creatable");
        set_mode_0700(&path);
        StateDir { path }
    }

    /// The directory path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Default for StateDir {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for StateDir {
    fn drop(&mut self) {
        // Best effort: a test that already failed should not be masked by a cleanup
        // error, but the directory must not survive the test.
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(unix)]
fn set_mode_0700(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .expect("state dir must be settable to 0700");
}

#[cfg(not(unix))]
fn set_mode_0700(_path: &Path) {}
