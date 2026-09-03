//! Construction and atomic publication of the status projection (`aub-sth.15`, PLAN.md 11.5, 15, 16).
//!
//! May not depend on:
//! - provider adapters
//! - terminal-formatting crates
//!
//! # Single current file
//!
//! The status projection is disposable and presents the current compact meter state
//! for fast shell integration. Its retention policy is SingleCurrentFile: publication
//! is atomic (write temp file, fsync, atomic rename) and replaces the previous file,
//! so no historical projection files accumulate on disk.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::Error;

/// The canonical file name for the published status projection.
pub const PROJECTION_FILE_NAME: &str = "status.json";

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Returns the path to the current projection file within a state directory.
pub fn projection_path(state_dir: &Path) -> PathBuf {
    state_dir.join(PROJECTION_FILE_NAME)
}

/// Atomically publishes the status projection into the state directory.
///
/// Writes content to a unique temporary file in the same directory, fsyncs the
/// file, and atomically renames it over the target projection file. Only the single
/// current file survives on disk; no historical versions or orphaned temporary files
/// accumulate.
pub fn publish_projection(state_dir: &Path, content: &[u8]) -> Result<PathBuf, Error> {
    if !state_dir.is_dir() {
        fs::create_dir_all(state_dir).map_err(|e| {
            Error::Store(format!(
                "cannot create state directory {}: {e}",
                state_dir.display()
            ))
        })?;
    }

    let target = projection_path(state_dir);
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp_path = state_dir.join(format!(
        "{PROJECTION_FILE_NAME}.tmp.{}.{counter}",
        std::process::id()
    ));

    let write_result = (|| -> Result<(), std::io::Error> {
        let mut file = File::create(&temp_path)?;
        file.write_all(content)?;
        file.sync_all()?;
        fs::rename(&temp_path, &target)?;
        Ok(())
    })();

    if let Err(err) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(Error::Store(format!(
            "cannot atomically publish projection to {}: {err}",
            target.display()
        )));
    }

    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-projection-test-{}-{suffix}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("scratch dir must be creatable");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn exactly_one_projection_file_present_after_repeated_publication() {
        let scratch = ScratchDir::new();
        let state_dir = scratch.path();

        for i in 1..=50 {
            let payload = format!("{{\"generation\": {i}}}");
            let published = publish_projection(state_dir, payload.as_bytes()).unwrap();
            assert_eq!(published, state_dir.join(PROJECTION_FILE_NAME));

            let read_back = fs::read_to_string(&published).unwrap();
            assert_eq!(read_back, payload);
        }

        let entries: Vec<PathBuf> = fs::read_dir(state_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();

        assert_eq!(
            entries.len(),
            1,
            "expected exactly one file in state dir, found: {entries:?}"
        );
        assert_eq!(entries[0], state_dir.join(PROJECTION_FILE_NAME));
    }
}
