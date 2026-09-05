//! Durable, accumulating counts of `aub sample` disposition failures by
//! reason (`aub-b0w6`).
//!
//! `crate::store::sample_tick` records only the *last* tick's outcome and
//! replaces it on every write, so a lone `persist-failed` or
//! `due-lookup-failed` tick sandwiched between two successful ones vanishes
//! the moment the next tick lands. The 2026-09-04 21:05:07 transient
//! (`aub-lz0k`) was exactly that: a `SQLITE_CORRUPT` surfacing once, self-
//! healing, and visible afterwards only in the scheduler's own journal. This
//! module is the counter that survives it: every occurrence of a given
//! `(category, reason)` pair adds to a running total that a later success
//! never resets, so a recurrence of the same reason is a number that grew
//! rather than a line that scrolled past.
//!
//! May not depend on:
//! - presentation
//! - provider adapters

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::error::Error;
use crate::store::startup::ensure_dir_mode_0700;

/// The file name of the durable failure-count ledger inside the state directory.
pub const SAMPLING_FAILURE_COUNTS_FILE_NAME: &str = "sampling-failure-counts.json";

const SCHEMA_VERSION: u32 = 1;

/// One `(category, reason)` pair's accumulated occurrence count. `category`
/// names the disposition (`persist_failed`, `due_lookup_failed`) the same way
/// the JSON `sample` output already spells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamplingFailureCount {
    pub category: String,
    pub reason: String,
    pub count: u64,
}

/// The failure-count file's path inside a state directory.
pub fn sampling_failure_counts_path(state_dir: &Path) -> PathBuf {
    state_dir.join(SAMPLING_FAILURE_COUNTS_FILE_NAME)
}

/// Increments the count for `(category, reason)` by one, leaving every other
/// pair's count untouched. Reads the existing file first, so this adds to a
/// running total rather than replacing it the way `sample_tick` replaces its
/// single latest-tick record.
pub fn record_sampling_failure(
    state_dir: &Path,
    category: &str,
    reason: &str,
) -> Result<(), Error> {
    ensure_dir_mode_0700(state_dir)?;
    let mut counts = read_counts_map(state_dir)?;
    *counts
        .entry((category.to_string(), reason.to_string()))
        .or_insert(0) += 1;
    write_counts_map(state_dir, &counts)
}

/// Reads every recorded `(category, reason)` count, sorted for a
/// deterministic report. Empty when no failure has ever been recorded.
pub fn read_sampling_failure_counts(state_dir: &Path) -> Result<Vec<SamplingFailureCount>, Error> {
    let counts = read_counts_map(state_dir)?;
    Ok(counts
        .into_iter()
        .map(|((category, reason), count)| SamplingFailureCount {
            category,
            reason,
            count,
        })
        .collect())
}

fn read_counts_map(state_dir: &Path) -> Result<BTreeMap<(String, String), u64>, Error> {
    let path = sampling_failure_counts_path(state_dir);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => {
            return Err(Error::Store(format!(
                "cannot read the sampling failure counts at {path:?}: {error}"
            )));
        }
    };
    let document: Value = serde_json::from_str(&text).map_err(|error| {
        Error::Store(format!(
            "cannot parse the sampling failure counts {path:?}: {error}"
        ))
    })?;
    let entries = document
        .get("failures")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Store(format!("{path:?} carries no failures array")))?;
    let mut counts = BTreeMap::new();
    for entry in entries {
        let category = entry
            .get("category")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Store(format!("{path:?} carries an entry with no category")))?;
        let reason = entry
            .get("reason")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Store(format!("{path:?} carries an entry with no reason")))?;
        let count = entry
            .get("count")
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::Store(format!("{path:?} carries an entry with no count")))?;
        counts.insert((category.to_string(), reason.to_string()), count);
    }
    Ok(counts)
}

fn write_counts_map(
    state_dir: &Path,
    counts: &BTreeMap<(String, String), u64>,
) -> Result<(), Error> {
    let failures: Vec<Value> = counts
        .iter()
        .map(|((category, reason), count)| {
            json!({ "category": category, "reason": reason, "count": count })
        })
        .collect();
    let document = json!({
        "schema_version": SCHEMA_VERSION,
        "failures": failures,
    });
    atomic_write(
        &sampling_failure_counts_path(state_dir),
        document.to_string().as_bytes(),
    )
}

/// Writes `bytes` to `target` so a reader never observes a torn file, and so
/// the file exists at mode 0600 from the moment it first appears. Mirrors
/// `crate::store::sample_tick`'s own publication helper; duplicated rather
/// than shared because the two files have no other relationship and sharing
/// the helper would reach across a module boundary for twenty lines.
fn atomic_write(target: &Path, bytes: &[u8]) -> Result<(), Error> {
    let parent = target.parent().unwrap_or_else(|| Path::new(""));
    let file_name = target
        .file_name()
        .ok_or_else(|| {
            Error::Store(format!(
                "sampling failure counts target {target:?} has no file name"
            ))
        })?
        .to_string_lossy()
        .into_owned();
    let temporary = parent.join(format!("{file_name}.tmp-{}", std::process::id()));

    let mut file = open_temporary(&temporary)?;
    file.write_all(bytes).map_err(|error| {
        Error::Store(format!(
            "cannot write the sampling failure counts temporary: {error}"
        ))
    })?;
    file.sync_all().map_err(|error| {
        Error::Store(format!(
            "cannot fsync the sampling failure counts temporary: {error}"
        ))
    })?;
    drop(file);

    fs::rename(&temporary, target).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        Error::Store(format!(
            "cannot publish the sampling failure counts file: {error}"
        ))
    })?;
    sync_directory(parent)?;
    Ok(())
}

#[cfg(unix)]
fn open_temporary(path: &Path) -> Result<fs::File, Error> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| Error::Store(format!("cannot create {path:?} at mode 0600: {error}")))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| Error::Store(format!("cannot set {path:?} to mode 0600: {error}")))?;
    Ok(file)
}

#[cfg(not(unix))]
fn open_temporary(path: &Path) -> Result<fs::File, Error> {
    fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|error| Error::Store(format!("cannot create {path:?}: {error}")))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), Error> {
    let dir = fs::File::open(path)
        .map_err(|error| Error::Store(format!("cannot open {path:?} to sync it: {error}")))?;
    dir.sync_all()
        .map_err(|error| Error::Store(format!("cannot fsync the directory {path:?}: {error}")))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), Error> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-sampling-failure-counts-test-{}-{suffix}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("scratch dir must be creatable");
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
    fn no_recorded_failure_reads_as_an_empty_list() {
        let scratch = ScratchDir::new();
        assert_eq!(
            read_sampling_failure_counts(scratch.path()).unwrap(),
            vec![]
        );
    }

    #[test]
    fn one_recorded_failure_counts_once() {
        let scratch = ScratchDir::new();
        record_sampling_failure(scratch.path(), "persist_failed", "disk full").unwrap();
        let counts = read_sampling_failure_counts(scratch.path()).unwrap();
        assert_eq!(
            counts,
            vec![SamplingFailureCount {
                category: "persist_failed".to_string(),
                reason: "disk full".to_string(),
                count: 1,
            }]
        );
    }

    /// The behaviour this counter exists for: the same reason recurring adds
    /// to the running total instead of the second write replacing the first,
    /// which is exactly how `sample_tick`'s single latest-tick record behaves
    /// and exactly what would make a recurrence invisible here too.
    #[test]
    fn the_same_reason_recurring_accumulates_rather_than_replacing() {
        let scratch = ScratchDir::new();
        record_sampling_failure(
            scratch.path(),
            "due_lookup_failed",
            "database disk image is malformed",
        )
        .unwrap();
        record_sampling_failure(
            scratch.path(),
            "due_lookup_failed",
            "database disk image is malformed",
        )
        .unwrap();
        let counts = read_sampling_failure_counts(scratch.path()).unwrap();
        assert_eq!(
            counts,
            vec![SamplingFailureCount {
                category: "due_lookup_failed".to_string(),
                reason: "database disk image is malformed".to_string(),
                count: 2,
            }]
        );
    }

    #[test]
    fn distinct_reasons_are_counted_separately() {
        let scratch = ScratchDir::new();
        record_sampling_failure(scratch.path(), "persist_failed", "disk full").unwrap();
        record_sampling_failure(scratch.path(), "persist_failed", "disk full").unwrap();
        record_sampling_failure(scratch.path(), "due_lookup_failed", "disk full").unwrap();
        let mut counts = read_sampling_failure_counts(scratch.path()).unwrap();
        counts.sort_by(|a, b| a.category.cmp(&b.category));
        assert_eq!(
            counts,
            vec![
                SamplingFailureCount {
                    category: "due_lookup_failed".to_string(),
                    reason: "disk full".to_string(),
                    count: 1,
                },
                SamplingFailureCount {
                    category: "persist_failed".to_string(),
                    reason: "disk full".to_string(),
                    count: 2,
                },
            ]
        );
    }

    #[test]
    fn a_corrupt_counts_file_is_a_named_store_failure_not_a_silent_reset() {
        let scratch = ScratchDir::new();
        fs::write(sampling_failure_counts_path(scratch.path()), b"not json").unwrap();
        let error = read_sampling_failure_counts(scratch.path()).unwrap_err();
        assert!(
            matches!(error, Error::Store(ref message) if message.contains("cannot parse")),
            "{error}"
        );
    }
}
